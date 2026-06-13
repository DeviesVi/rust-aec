// Audio engine: manages AEC processing and audio threads.
// Runs on its own thread; receives commands from the tray via crossbeam channel.
//
// The reference pipeline (loopback-capture + render) can stay alive while idle
// so Pause/Resume does not rebuild all long-lived resources every time.
// Mic capture still stops on Pause so the real microphone is released.
//
// Resume latency: ~50-100 ms (concurrent WASAPI init across all three threads).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Receiver;

use crate::aec::{AecProcessor, FRAME_SIZE, SAMPLE_RATE};
use crate::audio::device;
use crate::sync::AudioRingBuf;
use crate::tray::TrayState;

pub enum EngineCommand {
    SetMicDevice(String),
    SetSpeakerDevice(String),
    SetOutputDevice(String),
    RefreshDevices,
    /// Pause processing and release the real microphone.
    /// Long-lived reference/AEC resources may stay warm to keep memory stable.
    Pause,
    /// Resume processing, restarting the microphone if needed.
    /// Sent by the session monitor when a program begins recording.
    Resume,
    Shutdown,
}

pub struct AudioEngine {
    pub cmd_rx: Receiver<EngineCommand>,
    pub state: Arc<Mutex<TrayState>>,
    pub verbose: bool,
}

// ---------------------------------------------------------------------------
// RefPipeline: loopback-capture + render threads.
// ---------------------------------------------------------------------------

struct RefPipeline {
    ref_thread: Option<JoinHandle<Result<()>>>,
    out_thread: Option<JoinHandle<Result<()>>>,
    ref_cons: crate::sync::AudioConsumer,
    out_prod: crate::sync::AudioProducer,
    stop: Arc<AtomicBool>,
    _paused: Arc<AtomicBool>,
}

impl RefPipeline {
    fn new(speaker_id: &str, output_id: &str, paused: Arc<AtomicBool>) -> Result<Self> {
        let buf_capacity = SAMPLE_RATE / 5; // 200 ms

        let ref_ring = AudioRingBuf::new(buf_capacity);
        let out_ring = AudioRingBuf::new(buf_capacity);

        let (ref_prod, ref_cons) = ref_ring.split();
        let (out_prod, out_cons) = out_ring.split();

        let stop = Arc::new(AtomicBool::new(false));

        let stop_ref = stop.clone();
        let paused_ref = paused.clone();
        let speaker_id = speaker_id.to_string();
        let ref_thread = thread::Builder::new()
            .name("loopback-capture".into())
            .spawn(move || {
                let _com = device::com_init().expect("COM init failed in loopback thread");
                crate::audio::loopback::loopback_loop(&speaker_id, ref_prod, stop_ref, paused_ref)
            })?;

        let stop_out = stop.clone();
        let paused_out = paused.clone();
        let output_id = output_id.to_string();
        let out_thread = thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let _com = device::com_init().expect("COM init failed in render thread");
                crate::audio::render::render_loop(&output_id, out_cons, stop_out, paused_out)
            })?;

        Ok(Self {
            ref_thread: Some(ref_thread),
            out_thread: Some(out_thread),
            ref_cons,
            out_prod,
            stop,
            _paused: paused,
        })
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.ref_thread.take() {
            if let Err(e) = h.join().unwrap_or(Ok(())) {
                eprintln!("[error] loopback thread: {:#}", e);
            }
        }
        if let Some(h) = self.out_thread.take() {
            if let Err(e) = h.join().unwrap_or(Ok(())) {
                eprintln!("[error] render thread: {:#}", e);
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.ref_thread.as_ref().map_or(true, |h| h.is_finished())
            || self.out_thread.as_ref().map_or(true, |h| h.is_finished())
    }
}

// ---------------------------------------------------------------------------
// MicCapture: mic-capture thread.
// ---------------------------------------------------------------------------

struct MicCapture {
    thread: Option<JoinHandle<Result<()>>>,
    cons: crate::sync::AudioConsumer,
    stop: Arc<AtomicBool>,
}

impl MicCapture {
    fn new(mic_id: &str) -> Result<Self> {
        let buf_capacity = SAMPLE_RATE / 5;
        let mic_ring = AudioRingBuf::new(buf_capacity);
        let (mic_prod, mic_cons) = mic_ring.split();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_mic = stop.clone();
        let mic_id_owned = mic_id.to_string();

        let thread = thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || {
                let _com = device::com_init().expect("COM init failed in mic thread");
                crate::audio::capture::capture_loop(&mic_id_owned, mic_prod, stop_mic)
            })?;

        Ok(Self {
            thread: Some(thread),
            cons: mic_cons,
            stop,
        })
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            if let Err(e) = h.join().unwrap_or(Ok(())) {
                eprintln!("[error] mic-capture thread: {:#}", e);
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().map_or(true, |h| h.is_finished())
    }
}

// ---------------------------------------------------------------------------
// AudioEngine
// ---------------------------------------------------------------------------

impl AudioEngine {
    fn auto_mic_id(capture: &[device::DeviceInfo]) -> Option<String> {
        match device::default_capture_device_id() {
            Ok(default_id) => capture
                .iter()
                .find(|d| d.id == default_id)
                .and_then(|d| (!device::is_auto_mic_excluded(&d.name)).then(|| default_id.clone()))
                .or_else(|| device::find_real_capture_device(capture).ok()),
            Err(_) => device::find_real_capture_device(&capture).ok(),
        }
    }

    fn auto_speaker_id(render: &[device::DeviceInfo]) -> Option<String> {
        match device::default_render_device_id() {
            Ok(default_id) => render
                .iter()
                .find(|d| d.id == default_id)
                .and_then(|d| {
                    (!device::is_auto_speaker_excluded(&d.name)).then(|| default_id.clone())
                })
                .or_else(|| device::find_real_render_device(render).ok()),
            Err(_) => device::find_real_render_device(render).ok(),
        }
    }

    fn auto_output_id(render: &[device::DeviceInfo]) -> Option<String> {
        device::find_device_id_by_name(render, "cable input").ok()
    }

    fn refresh_devices_preserving(
        &self,
        current_mic: Option<&str>,
        current_speaker: Option<&str>,
        current_output: Option<&str>,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let capture_result = device::list_capture_devices();
        let render_result = device::list_render_devices();

        let (capture, render, preferred_mic, preferred_speaker, preferred_output) = {
            let st = self.state.lock().unwrap();
            (
                capture_result.unwrap_or_else(|_| st.capture_devices.clone()),
                render_result.unwrap_or_else(|_| st.render_devices.clone()),
                st.preferred_mic_id.clone(),
                st.preferred_speaker_id.clone(),
                st.preferred_output_id.clone(),
            )
        };

        let mic_id = match preferred_mic.as_deref() {
            Some(id) if capture.iter().any(|d| d.id == id) => Some(id.to_string()),
            Some(_) => None,
            None => current_mic
                .filter(|id| capture.iter().any(|d| d.id == *id))
                .map(str::to_string)
                .or_else(|| Self::auto_mic_id(&capture)),
        };
        let speaker_id = match preferred_speaker.as_deref() {
            Some(id) if render.iter().any(|d| d.id == id) => Some(id.to_string()),
            Some(_) => None,
            None => current_speaker
                .filter(|id| render.iter().any(|d| d.id == *id))
                .map(str::to_string)
                .or_else(|| Self::auto_speaker_id(&render)),
        };
        let output_id = match preferred_output.as_deref() {
            Some(id) if render.iter().any(|d| d.id == id) => Some(id.to_string()),
            Some(_) => None,
            None => current_output
                .filter(|id| render.iter().any(|d| d.id == *id))
                .map(str::to_string)
                .or_else(|| Self::auto_output_id(&render)),
        };

        let mut st = self.state.lock().unwrap();
        st.capture_devices = capture;
        st.render_devices = render;
        st.current_mic_id = mic_id.clone();
        if st.preferred_mic_id.is_none() {
            st.preferred_mic_id = mic_id.clone();
        }
        st.current_speaker_id = speaker_id.clone();
        if st.preferred_speaker_id.is_none() {
            st.preferred_speaker_id = speaker_id.clone();
        }
        st.current_output_id = output_id.clone();
        if st.preferred_output_id.is_none() {
            st.preferred_output_id = output_id.clone();
        }

        (mic_id, speaker_id, output_id)
    }

    fn start_ref_pipeline(
        &self,
        speaker_id: &Option<String>,
        output_id: &Option<String>,
        ref_pipe: &mut Option<RefPipeline>,
        paused_flag: Arc<AtomicBool>,
        context: &str,
    ) {
        if ref_pipe.is_some() {
            return;
        }

        if let (Some(spk), Some(out)) = (speaker_id, output_id) {
            match RefPipeline::new(spk, out, paused_flag) {
                Ok(p) => {
                    *ref_pipe = Some(p);
                    if self.verbose {
                        eprintln!("[engine] Reference pipeline {context}.");
                    }
                }
                Err(e) => {
                    if self.verbose {
                        eprintln!(
                            "[engine] Failed to start reference pipeline {context}: {:#}",
                            e
                        );
                    }
                }
            }
        }
    }

    fn ensure_mic_capture(
        &self,
        mic_id: &mut Option<String>,
        speaker_id: &mut Option<String>,
        output_id: &mut Option<String>,
        ref_pipe: &Option<RefPipeline>,
        mic_capture: &mut Option<MicCapture>,
        processor: &mut Option<AecProcessor>,
    ) -> Result<()> {
        if mic_capture.is_some() || ref_pipe.is_none() {
            return Ok(());
        }

        if mic_id.is_none() {
            let (new_mic, new_spk, new_out) = self.refresh_devices_preserving(
                mic_id.as_deref(),
                speaker_id.as_deref(),
                output_id.as_deref(),
            );
            *mic_id = new_mic;
            *speaker_id = new_spk;
            *output_id = new_out;
        }

        let Some(mic) = mic_id.as_deref() else {
            if self.verbose {
                eprintln!("[engine] Waiting for microphone device...");
            }
            return Ok(());
        };

        match MicCapture::new(mic) {
            Ok(mc) => {
                *mic_capture = Some(mc);
                if processor.is_none() {
                    *processor = Some(AecProcessor::new()?);
                }
            }
            Err(e) => {
                if self.verbose {
                    eprintln!("[engine] Failed to start mic-capture: {:#}", e);
                }
            }
        }

        Ok(())
    }

    fn ensure_running(
        &self,
        mic_id: &mut Option<String>,
        speaker_id: &mut Option<String>,
        output_id: &mut Option<String>,
        ref_pipe: &mut Option<RefPipeline>,
        mic_capture: &mut Option<MicCapture>,
        processor: &mut Option<AecProcessor>,
        paused: bool,
        paused_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        if paused {
            return Ok(());
        }

        self.reap_finished_audio(ref_pipe, mic_capture, processor);

        if ref_pipe.is_none() {
            let (new_mic, new_spk, new_out) = self.refresh_devices_preserving(
                mic_id.as_deref(),
                speaker_id.as_deref(),
                output_id.as_deref(),
            );
            *mic_id = new_mic;
            *speaker_id = new_spk;
            *output_id = new_out;
            self.start_ref_pipeline(
                speaker_id,
                output_id,
                ref_pipe,
                paused_flag.clone(),
                "started",
            );
        }

        self.ensure_mic_capture(
            mic_id,
            speaker_id,
            output_id,
            ref_pipe,
            mic_capture,
            processor,
        )?;

        paused_flag.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn reap_finished_audio(
        &self,
        ref_pipe: &mut Option<RefPipeline>,
        mic_capture: &mut Option<MicCapture>,
        processor: &mut Option<AecProcessor>,
    ) {
        if ref_pipe.as_ref().is_some_and(RefPipeline::is_finished) {
            if self.verbose {
                eprintln!("[engine] Reference audio thread exited; rebuilding pipeline.");
            }
            if let Some(mut p) = ref_pipe.take() {
                p.shutdown();
            }
            if let Some(mut mc) = mic_capture.take() {
                mc.shutdown();
            }
            *processor = None;
        }

        if mic_capture.as_ref().is_some_and(MicCapture::is_finished) {
            if self.verbose {
                eprintln!("[engine] Mic capture thread exited; restarting on next run check.");
            }
            if let Some(mut mc) = mic_capture.take() {
                mc.shutdown();
            }
            *processor = None;
        }
    }

    pub fn run(&self) -> Result<()> {
        let _com = device::com_init()?;

        let (mut mic_id, mut speaker_id, mut output_id) = {
            let st = self.state.lock().unwrap();
            (
                st.current_mic_id.clone(),
                st.current_speaker_id.clone(),
                st.current_output_id.clone(),
            )
        };

        let mut ref_pipe: Option<RefPipeline> = None;
        let mut mic_capture: Option<MicCapture> = None;
        let mut processor: Option<AecProcessor> = Some(AecProcessor::new()?);

        let mut mic_frame = vec![0.0f32; FRAME_SIZE];
        let mut ref_frame = vec![0.0f32; FRAME_SIZE];
        let mut out_frame = vec![0.0f32; FRAME_SIZE];
        let mut frames_processed: u64 = 0;
        let mut last_report = Instant::now();
        let mut paused = true;
        let paused_flag = Arc::new(AtomicBool::new(true));

        // Start long-lived reference threads immediately if devices are known.
        self.start_ref_pipeline(
            &speaker_id,
            &output_id,
            &mut ref_pipe,
            paused_flag.clone(),
            "started (loopback + render)",
        );
        if ref_pipe.is_none() && self.verbose {
            eprintln!(
                "[engine] Waiting for speaker/output devices (speaker={}, output={})...",
                speaker_id.is_some(),
                output_id.is_some(),
            );
        }

        loop {
            if !paused {
                self.reap_finished_audio(&mut ref_pipe, &mut mic_capture, &mut processor);
                self.ensure_running(
                    &mut mic_id,
                    &mut speaker_id,
                    &mut output_id,
                    &mut ref_pipe,
                    &mut mic_capture,
                    &mut processor,
                    paused,
                    paused_flag.clone(),
                )?;
            }
            // ----------------------------------------------------------------
            // Process commands. While paused or waiting for a reference
            // pipeline, the engine blocks here instead of polling.
            // ----------------------------------------------------------------
            let next_cmd = if paused {
                match self.cmd_rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return Ok(()),
                }
            } else {
                self.cmd_rx.try_recv().ok()
            };

            match next_cmd {
                Some(EngineCommand::Shutdown) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    if let Some(ref mut p) = ref_pipe {
                        p.shutdown();
                    }
                    return Ok(());
                }

                Some(EngineCommand::SetMicDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    mic_capture = None;
                    processor = None;
                    mic_id = Some(new_id.clone());
                    {
                        let mut st = self.state.lock().unwrap();
                        st.preferred_mic_id = Some(new_id.clone());
                        st.current_mic_id = Some(new_id);
                    }
                    self.ensure_running(
                        &mut mic_id,
                        &mut speaker_id,
                        &mut output_id,
                        &mut ref_pipe,
                        &mut mic_capture,
                        &mut processor,
                        paused,
                        paused_flag.clone(),
                    )?;
                }

                Some(EngineCommand::SetSpeakerDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe {
                        p.shutdown();
                    }
                    ref_pipe = None;
                    speaker_id = Some(new_id.clone());
                    {
                        let mut st = self.state.lock().unwrap();
                        st.preferred_speaker_id = Some(new_id.clone());
                        st.current_speaker_id = Some(new_id);
                    }
                    self.start_ref_pipeline(
                        &speaker_id,
                        &output_id,
                        &mut ref_pipe,
                        paused_flag.clone(),
                        "restarted (new speaker)",
                    );
                    self.ensure_running(
                        &mut mic_id,
                        &mut speaker_id,
                        &mut output_id,
                        &mut ref_pipe,
                        &mut mic_capture,
                        &mut processor,
                        paused,
                        paused_flag.clone(),
                    )?;
                }

                Some(EngineCommand::SetOutputDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe {
                        p.shutdown();
                    }
                    ref_pipe = None;
                    output_id = Some(new_id.clone());
                    {
                        let mut st = self.state.lock().unwrap();
                        st.preferred_output_id = Some(new_id.clone());
                        st.current_output_id = Some(new_id);
                    }
                    self.start_ref_pipeline(
                        &speaker_id,
                        &output_id,
                        &mut ref_pipe,
                        paused_flag.clone(),
                        "restarted (new output)",
                    );
                    self.ensure_running(
                        &mut mic_id,
                        &mut speaker_id,
                        &mut output_id,
                        &mut ref_pipe,
                        &mut mic_capture,
                        &mut processor,
                        paused,
                        paused_flag.clone(),
                    )?;
                }

                Some(EngineCommand::RefreshDevices) => {
                    if self.verbose {
                        eprintln!("[engine] Refreshing devices...");
                    }
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe {
                        p.shutdown();
                    }
                    ref_pipe = None;
                    let (new_mic, new_spk, new_out) = self.refresh_devices_preserving(
                        mic_id.as_deref(),
                        speaker_id.as_deref(),
                        output_id.as_deref(),
                    );
                    mic_id = new_mic;
                    speaker_id = new_spk;
                    output_id = new_out;
                    self.start_ref_pipeline(
                        &speaker_id,
                        &output_id,
                        &mut ref_pipe,
                        paused_flag.clone(),
                        "started after refresh",
                    );
                    self.ensure_running(
                        &mut mic_id,
                        &mut speaker_id,
                        &mut output_id,
                        &mut ref_pipe,
                        &mut mic_capture,
                        &mut processor,
                        paused,
                        paused_flag.clone(),
                    )?;
                }

                Some(EngineCommand::Pause) => {
                    paused = true;
                    paused_flag.store(true, Ordering::Relaxed);
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    mic_capture = None;
                    if self.verbose {
                        eprintln!(
                            "[engine] Processing paused; microphone released, reference pipeline kept alive."
                        );
                    }
                }

                Some(EngineCommand::Resume) => {
                    paused = false;
                    self.ensure_running(
                        &mut mic_id,
                        &mut speaker_id,
                        &mut output_id,
                        &mut ref_pipe,
                        &mut mic_capture,
                        &mut processor,
                        paused,
                        paused_flag.clone(),
                    )?;
                    frames_processed = 0;
                    if self.verbose && mic_capture.is_some() && ref_pipe.is_some() {
                        eprintln!("[engine] Resumed with persistent threads/resources.");
                    }
                }

                None => {}
            }

            // ----------------------------------------------------------------
            // Audio processing loop.
            // ----------------------------------------------------------------
            let Some(ref_pipe) = ref_pipe.as_mut() else {
                thread::sleep(Duration::from_millis(100));
                continue;
            };

            if paused {
                continue;
            }

            if let Some(mc) = mic_capture.as_mut() {
                // --- All threads running: wait for a mic frame then run AEC ---
                if mc.cons.available() < FRAME_SIZE {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                mc.cons.pop(&mut mic_frame);

                let ref_available = ref_pipe.ref_cons.available().min(FRAME_SIZE);
                ref_pipe.ref_cons.pop(&mut ref_frame[..ref_available]);
                ref_frame[ref_available..].fill(0.0);

                if ref_available == 0 {
                    // No far-end audio: pass through directly.
                    out_frame.copy_from_slice(&mic_frame);
                } else {
                    processor.as_mut().unwrap().process_frame(
                        &mic_frame,
                        &ref_frame,
                        &mut out_frame,
                    );
                }

                ref_pipe.out_prod.push(&out_frame);
                frames_processed += 1;

                if self.verbose && last_report.elapsed().as_secs() >= 2 {
                    let mic_peak = mic_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                    let out_peak = out_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                    println!(
                        "[diag] frames={}, mic_peak={:.4}, out_peak={:.4}, mic_buf={}, ref_buf={}, out_buf={}",
                        frames_processed,
                        mic_peak,
                        out_peak,
                        mc.cons.available(),
                        ref_pipe.ref_cons.available(),
                        ref_pipe.out_prod.available(),
                    );
                    last_report = Instant::now();
                }
            } else {
                // --- Loopback+render up but mic not yet started (edge case): feed silence ---
                let available = ref_pipe.ref_cons.available();
                if available >= FRAME_SIZE {
                    ref_pipe.ref_cons.pop(&mut ref_frame[..FRAME_SIZE]);
                }
                out_frame.fill(0.0);
                ref_pipe.out_prod.push(&out_frame);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
