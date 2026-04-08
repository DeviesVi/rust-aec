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
use std::time::Instant;

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

fn drain_consumer(consumer: &mut crate::sync::AudioConsumer, scratch: &mut [f32]) {
    loop {
        let to_read = consumer.available().min(scratch.len());
        if to_read == 0 {
            break;
        }
        consumer.pop(&mut scratch[..to_read]);
    }
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
}

impl RefPipeline {
    fn new(speaker_id: &str, output_id: &str) -> Result<Self> {
        let buf_capacity = SAMPLE_RATE / 5; // 200 ms

        let ref_ring = AudioRingBuf::new(buf_capacity);
        let out_ring = AudioRingBuf::new(buf_capacity);

        let (ref_prod, ref_cons) = ref_ring.split();
        let (out_prod, out_cons) = out_ring.split();

        let stop = Arc::new(AtomicBool::new(false));

        let stop_ref = stop.clone();
        let speaker_id = speaker_id.to_string();
        let ref_thread = thread::Builder::new()
            .name("loopback-capture".into())
            .spawn(move || {
                let _com = device::com_init().expect("COM init failed in loopback thread");
                crate::audio::loopback::loopback_loop(&speaker_id, ref_prod, stop_ref)
            })?;

        let stop_out = stop.clone();
        let output_id = output_id.to_string();
        let out_thread = thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let _com = device::com_init().expect("COM init failed in render thread");
                crate::audio::render::render_loop(&output_id, out_cons, stop_out)
            })?;

        Ok(Self {
            ref_thread: Some(ref_thread),
            out_thread: Some(out_thread),
            ref_cons,
            out_prod,
            stop,
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
}

// ---------------------------------------------------------------------------
// AudioEngine
// ---------------------------------------------------------------------------

impl AudioEngine {
    fn try_find_mic(state: &Mutex<TrayState>) -> Option<String> {
        let capture = device::list_capture_devices().ok()?;
        let mic_id = match device::default_capture_device_id() {
            Ok(default_id) => {
                let name = device::device_name_by_id(&capture, &default_id);
                if device::is_virtual_cable(&name) {
                    device::find_real_capture_device(&capture).ok()
                } else {
                    Some(default_id)
                }
            }
            Err(_) => device::find_real_capture_device(&capture).ok(),
        };
        let mut st = state.lock().unwrap();
        st.capture_devices = capture;
        st.current_mic_id = mic_id.clone();
        mic_id
    }

    fn try_find_speaker(state: &Mutex<TrayState>) -> Option<String> {
        let render = device::list_render_devices().ok()?;
        let speaker_id = device::default_render_device_id().ok();
        let mut st = state.lock().unwrap();
        st.render_devices = render;
        st.current_speaker_id = speaker_id.clone();
        speaker_id
    }

    fn try_find_output(state: &Mutex<TrayState>) -> Option<String> {
        let render = device::list_render_devices().ok()?;
        let output_id = device::find_device_id_by_name(&render, "cable input").ok();
        let mut st = state.lock().unwrap();
        st.render_devices = render;
        st.current_output_id = output_id.clone();
        output_id
    }

    fn refresh_missing(&self) -> (Option<String>, Option<String>, Option<String>) {
        let (mic, spk, out) = {
            let st = self.state.lock().unwrap();
            (
                st.current_mic_id.clone(),
                st.current_speaker_id.clone(),
                st.current_output_id.clone(),
            )
        };
        let mic = mic.or_else(|| Self::try_find_mic(&self.state));
        let spk = spk.or_else(|| Self::try_find_speaker(&self.state));
        let out = out.or_else(|| Self::try_find_output(&self.state));
        (mic, spk, out)
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
        let mut drain_buf = vec![0.0f32; FRAME_SIZE];
        let mut frames_processed: u64 = 0;
        let mut last_report = Instant::now();
        let mut paused = true;

        // Start long-lived reference threads immediately if devices are known.
        match (&speaker_id, &output_id) {
            (Some(spk), Some(out)) => match RefPipeline::new(spk, out) {
                Ok(p) => {
                    ref_pipe = Some(p);
                    if self.verbose {
                        eprintln!("[engine] Reference pipeline started (loopback + render).");
                    }
                }
                Err(e) => {
                    if self.verbose {
                        eprintln!("[engine] Failed to start reference pipeline: {:#}", e);
                    }
                }
            },
            _ => {
                if self.verbose {
                    eprintln!(
                        "[engine] Waiting for speaker/output devices (speaker={}, output={})...",
                        speaker_id.is_some(),
                        output_id.is_some(),
                    );
                }
            }
        }

        loop {
            // ----------------------------------------------------------------
            // Process commands (non-blocking).
            // ----------------------------------------------------------------
            match self.cmd_rx.try_recv() {
                Ok(EngineCommand::Shutdown) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                    }
                    if let Some(ref mut p) = ref_pipe {
                        p.shutdown();
                    }
                    return Ok(());
                }

                Ok(EngineCommand::SetMicDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture {
                        mc.shutdown();
                        mic_capture = None;
                        processor = None;
                    }
                    mic_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_mic_id = Some(new_id);
                }

                Ok(EngineCommand::SetSpeakerDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture { mc.shutdown(); }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe { p.shutdown(); }
                    ref_pipe = None;
                    speaker_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_speaker_id = Some(new_id);
                    if let (Some(spk), Some(out)) = (&speaker_id, &output_id) {
                        match RefPipeline::new(spk, out) {
                            Ok(p) => {
                                ref_pipe = Some(p);
                                if self.verbose { eprintln!("[engine] Reference pipeline restarted (new speaker)."); }
                            }
                            Err(e) => { if self.verbose { eprintln!("[engine] Failed to restart reference pipeline: {:#}", e); } }
                        }
                    }
                }

                Ok(EngineCommand::SetOutputDevice(new_id)) => {
                    if let Some(ref mut mc) = mic_capture { mc.shutdown(); }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe { p.shutdown(); }
                    ref_pipe = None;
                    output_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_output_id = Some(new_id);
                    if let (Some(spk), Some(out)) = (&speaker_id, &output_id) {
                        match RefPipeline::new(spk, out) {
                            Ok(p) => {
                                ref_pipe = Some(p);
                                if self.verbose { eprintln!("[engine] Reference pipeline restarted (new output)."); }
                            }
                            Err(e) => { if self.verbose { eprintln!("[engine] Failed to restart reference pipeline: {:#}", e); } }
                        }
                    }
                }

                Ok(EngineCommand::RefreshDevices) => {
                    if self.verbose { eprintln!("[engine] Refreshing devices..."); }
                    if let Some(ref mut mc) = mic_capture { mc.shutdown(); }
                    mic_capture = None;
                    processor = None;
                    if let Some(ref mut p) = ref_pipe { p.shutdown(); }
                    ref_pipe = None;
                    let (new_mic, new_spk, new_out) = self.refresh_missing();
                    mic_id = new_mic;
                    speaker_id = new_spk;
                    output_id = new_out;
                    if let (Some(spk), Some(out)) = (&speaker_id, &output_id) {
                        match RefPipeline::new(spk, out) {
                            Ok(p) => {
                                ref_pipe = Some(p);
                                if self.verbose { eprintln!("[engine] Reference pipeline started after refresh."); }
                            }
                            Err(e) => { if self.verbose { eprintln!("[engine] Failed to start reference pipeline after refresh: {:#}", e); } }
                        }
                    }
                }

                Ok(EngineCommand::Pause) => {
                    paused = true;
                    if let Some(ref mut mc) = mic_capture { mc.shutdown(); }
                    mic_capture = None;
                    if self.verbose {
                        eprintln!("[engine] Processing paused; microphone released, reference pipeline kept alive.");
                    }
                }

                Ok(EngineCommand::Resume) => {
                    paused = false;
                    // Start loopback-capture and render threads if not running.
                    if ref_pipe.is_none() {
                        let (new_mic, new_spk, new_out) = self.refresh_missing();
                        mic_id = new_mic.or(mic_id);
                        speaker_id = new_spk.or(speaker_id);
                        output_id = new_out.or(output_id);
                        if let (Some(spk), Some(out)) = (&speaker_id, &output_id) {
                            match RefPipeline::new(spk, out) {
                                Ok(p) => { ref_pipe = Some(p); }
                                Err(e) => {
                                    if self.verbose { eprintln!("[engine] Failed to start reference pipeline on resume: {:#}", e); }
                                }
                            }
                        }
                    }
                    // Start mic-capture thread if not already persistent.
                    if mic_capture.is_none() {
                        let resolved_mic = mic_id.clone().or_else(|| Self::try_find_mic(&self.state));
                        mic_id = resolved_mic.clone().or(mic_id);
                        if let (Some(m), true) = (&resolved_mic, ref_pipe.is_some()) {
                            match MicCapture::new(m) {
                                Ok(mc) => {
                                    mic_capture = Some(mc);
                                }
                                Err(e) => {
                                    if self.verbose { eprintln!("[engine] Failed to start mic-capture on resume: {:#}", e); }
                                    mic_id = None;
                                    self.state.lock().unwrap().current_mic_id = None;
                                }
                            }
                        }
                    }
                    if processor.is_none() && mic_capture.is_some() && ref_pipe.is_some() {
                        processor = Some(AecProcessor::new()?);
                    }
                    frames_processed = 0;
                    if self.verbose && mic_capture.is_some() && ref_pipe.is_some() {
                        eprintln!("[engine] Resumed with persistent threads/resources.");
                    }
                }

                Err(_) => {}
            }

            // ----------------------------------------------------------------
            // Audio processing loop.
            // ----------------------------------------------------------------
            let Some(ref_pipe) = ref_pipe.as_mut() else {
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            };

            if paused {
                drain_consumer(&mut ref_pipe.ref_cons, &mut drain_buf);
                out_frame.fill(0.0);
                ref_pipe.out_prod.push(&out_frame);
                thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }

            if let Some(mc) = mic_capture.as_mut() {
                // --- All threads running: wait for a mic frame then run AEC ---
                if mc.cons.available() < FRAME_SIZE {
                    thread::sleep(std::time::Duration::from_millis(1));
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
                    processor.as_mut().unwrap().process_frame(&mic_frame, &ref_frame, &mut out_frame);
                }

                ref_pipe.out_prod.push(&out_frame);
                frames_processed += 1;

                if self.verbose && last_report.elapsed().as_secs() >= 2 {
                    let mic_peak = mic_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                    let out_peak = out_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                    println!(
                        "[diag] frames={}, mic_peak={:.4}, out_peak={:.4}, mic_buf={}, ref_buf={}, out_buf={}",
                        frames_processed, mic_peak, out_peak,
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
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}
