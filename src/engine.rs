// Audio engine: manages AEC processing and audio threads.
// Runs on its own thread; receives commands from the tray via crossbeam channel.

use std::panic;
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
    Shutdown,
}

pub struct AudioEngine {
    pub cmd_rx: Receiver<EngineCommand>,
    pub state: Arc<Mutex<TrayState>>,
    pub verbose: bool,
}

struct Pipeline {
    mic_thread: Option<JoinHandle<Result<()>>>,
    ref_thread: Option<JoinHandle<Result<()>>>,
    out_thread: Option<JoinHandle<Result<()>>>,
    mic_cons: crate::sync::AudioConsumer,
    ref_cons: crate::sync::AudioConsumer,
    out_prod: crate::sync::AudioProducer,
    stop: Arc<AtomicBool>,
}

impl Pipeline {
    fn new(mic_id: &str, speaker_id: &str, output_id: &str) -> Result<Self> {
        let buf_capacity = SAMPLE_RATE / 5; // 200ms
        let mic_ring = AudioRingBuf::new(buf_capacity);
        let ref_ring = AudioRingBuf::new(buf_capacity);
        let out_ring = AudioRingBuf::new(buf_capacity);

        let (mic_prod, mic_cons) = mic_ring.split();
        let (ref_prod, ref_cons) = ref_ring.split();
        let (out_prod, out_cons) = out_ring.split();

        let stop = Arc::new(AtomicBool::new(false));

        let stop_mic = stop.clone();
        let mic_id_owned = mic_id.to_string();
        let mic_thread = thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || {
                device::com_init().expect("COM init failed in mic thread");
                crate::audio::capture::capture_loop(&mic_id_owned, mic_prod, stop_mic)
            })?;

        let stop_ref = stop.clone();
        let speaker_id_owned = speaker_id.to_string();
        let ref_thread = thread::Builder::new()
            .name("loopback-capture".into())
            .spawn(move || {
                device::com_init().expect("COM init failed in loopback thread");
                crate::audio::loopback::loopback_loop(&speaker_id_owned, ref_prod, stop_ref)
            })?;

        let stop_out = stop.clone();
        let output_id_owned = output_id.to_string();
        let out_thread = thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                device::com_init().expect("COM init failed in render thread");
                crate::audio::render::render_loop(&output_id_owned, out_cons, stop_out)
            })?;

        Ok(Self {
            mic_thread: Some(mic_thread),
            ref_thread: Some(ref_thread),
            out_thread: Some(out_thread),
            mic_cons,
            ref_cons,
            out_prod,
            stop,
        })
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.mic_thread.take() {
            if let Err(e) = h.join().unwrap_or(Ok(())) {
                eprintln!("[error] mic-capture thread: {:#}", e);
            }
        }
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

impl AudioEngine {
    /// Try to find a mic by re-enumerating capture devices.
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

    /// Try to find a speaker (default render device).
    fn try_find_speaker(state: &Mutex<TrayState>) -> Option<String> {
        let render = device::list_render_devices().ok()?;
        let speaker_id = device::default_render_device_id().ok();
        let mut st = state.lock().unwrap();
        st.render_devices = render;
        st.current_speaker_id = speaker_id.clone();
        speaker_id
    }

    /// Try to find a virtual audio cable output device.
    fn try_find_output(state: &Mutex<TrayState>) -> Option<String> {
        let render = device::list_render_devices().ok()?;
        let output_id = device::find_device_id_by_name(&render, "cable input").ok();
        let mut st = state.lock().unwrap();
        st.render_devices = render;
        st.current_output_id = output_id.clone();
        output_id
    }

    /// Refresh all missing devices from state. Returns (mic, speaker, output).
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

    /// Try to start the pipeline if all devices are available.
    fn try_start_pipeline(
        mic_id: &Option<String>,
        speaker_id: &Option<String>,
        output_id: &Option<String>,
    ) -> Option<Result<Pipeline>> {
        match (mic_id.as_ref(), speaker_id.as_ref(), output_id.as_ref()) {
            (Some(m), Some(s), Some(o)) => Some(Pipeline::new(m, s, o)),
            _ => None,
        }
    }

    pub fn run(&self) -> Result<()> {
        device::com_init()?;

        let (mut mic_id, mut speaker_id, mut output_id) = {
            let st = self.state.lock().unwrap();
            (
                st.current_mic_id.clone(),
                st.current_speaker_id.clone(),
                st.current_output_id.clone(),
            )
        };

        let mut pipeline: Option<Pipeline> = None;
        let mut processor: Option<AecProcessor> = None;
        let mut mic_frame = vec![0.0f32; FRAME_SIZE];
        let mut ref_frame = vec![0.0f32; FRAME_SIZE];
        let mut out_frame = vec![0.0f32; FRAME_SIZE];
        let mut frames_processed: u64 = 0;
        let mut last_report = Instant::now();

        // Start pipeline if all devices are available.
        match Self::try_start_pipeline(&mic_id, &speaker_id, &output_id) {
            Some(Ok(p)) => {
                pipeline = Some(p);
                processor = Some(AecProcessor::new()?);
            }
            Some(Err(e)) => {
                if self.verbose {
                    eprintln!("[engine] Failed to start pipeline: {:#}", e);
                }
                // Clear whichever device caused the failure and wait.
                mic_id = None;
                self.state.lock().unwrap().current_mic_id = None;
            }
            None => {
                if self.verbose {
                    eprintln!(
                        "[engine] Waiting for devices (mic={}, speaker={}, output={})...",
                        mic_id.is_some(),
                        speaker_id.is_some(),
                        output_id.is_some(),
                    );
                }
            }
        }

        loop {
            // Check for commands (non-blocking).
            match self.cmd_rx.try_recv() {
                Ok(EngineCommand::Shutdown) => {
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                    }
                    return Ok(());
                }
                Ok(EngineCommand::SetMicDevice(new_id)) => {
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                        pipeline = None;
                        processor = None;
                    }
                    mic_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_mic_id = Some(new_id);
                    if let Some(result) = Self::try_start_pipeline(&mic_id, &speaker_id, &output_id) {
                        pipeline = Some(result?);
                        processor = Some(AecProcessor::new()?);
                        frames_processed = 0;
                        if self.verbose {
                            eprintln!("[engine] Switched mic device, pipeline restarted.");
                        }
                    }
                }
                Ok(EngineCommand::SetSpeakerDevice(new_id)) => {
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                        pipeline = None;
                        processor = None;
                    }
                    speaker_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_speaker_id = Some(new_id);
                    if let Some(result) = Self::try_start_pipeline(&mic_id, &speaker_id, &output_id) {
                        pipeline = Some(result?);
                        processor = Some(AecProcessor::new()?);
                        frames_processed = 0;
                        if self.verbose {
                            eprintln!("[engine] Switched speaker device, pipeline restarted.");
                        }
                    }
                }
                Ok(EngineCommand::SetOutputDevice(new_id)) => {
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                        pipeline = None;
                        processor = None;
                    }
                    output_id = Some(new_id.clone());
                    self.state.lock().unwrap().current_output_id = Some(new_id);
                    if let Some(result) = Self::try_start_pipeline(&mic_id, &speaker_id, &output_id) {
                        pipeline = Some(result?);
                        processor = Some(AecProcessor::new()?);
                        frames_processed = 0;
                        if self.verbose {
                            eprintln!("[engine] Switched output device, pipeline restarted.");
                        }
                    }
                }
                Ok(EngineCommand::RefreshDevices) => {
                    if self.verbose {
                        eprintln!("[engine] Refreshing devices...");
                    }
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                        pipeline = None;
                        processor = None;
                    }
                    let (new_mic, new_spk, new_out) = self.refresh_missing();
                    mic_id = new_mic;
                    speaker_id = new_spk;
                    output_id = new_out;
                    match Self::try_start_pipeline(&mic_id, &speaker_id, &output_id) {
                        Some(Ok(p)) => {
                            pipeline = Some(p);
                            processor = Some(AecProcessor::new()?);
                            frames_processed = 0;
                            if self.verbose {
                                eprintln!("[engine] Pipeline (re)started after device refresh.");
                            }
                        }
                        Some(Err(e)) => {
                            if self.verbose {
                                eprintln!("[engine] Failed to start pipeline: {:#}", e);
                            }
                        }
                        None => {
                            if self.verbose {
                                eprintln!(
                                    "[engine] Still waiting for devices (mic={}, speaker={}, output={}).",
                                    mic_id.is_some(),
                                    speaker_id.is_some(),
                                    output_id.is_some(),
                                );
                            }
                        }
                    }
                }
                Err(_) => {}
            }

            // If no pipeline, sleep and wait for commands.
            if pipeline.is_none() {
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            let p = pipeline.as_mut().unwrap();
            let proc = processor.as_mut().unwrap();

            if p.mic_cons.available() < FRAME_SIZE {
                thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }

            p.mic_cons.pop(&mut mic_frame);

            let ref_available = p.ref_cons.available().min(FRAME_SIZE);
            p.ref_cons.pop(&mut ref_frame[..ref_available]);
            ref_frame[ref_available..].fill(0.0);

            if ref_available == 0 {
                // No reference audio in the ring buffer means no speaker output
                // for at least 200 ms (the full buffer capacity drained).  With no
                // far-end signal there is no echo to cancel, so pass the mic
                // through directly — skipping AEC saves significant CPU.
                out_frame.copy_from_slice(&mic_frame);
            } else {
                // Wrap in catch_unwind: the sonora AEC3 library has an off-by-one
                // bug in its adaptive FIR filter that panics after ~6 minutes of
                // continuous use. Without this, the panic kills the engine thread
                // and audio stops permanently until the app is restarted.
                if panic::catch_unwind(panic::AssertUnwindSafe(|| {
                    proc.process_frame(&mic_frame, &ref_frame, &mut out_frame);
                }))
                .is_err()
                {
                    if self.verbose {
                        eprintln!("[engine] AEC panic (sonora bug) — reinitializing processor.");
                    }
                    // Pass through mic audio for this frame so there is no gap.
                    out_frame.copy_from_slice(&mic_frame);
                    // Drop the corrupt AEC state and start fresh.
                    if let Ok(new_proc) = AecProcessor::new() {
                        *proc = new_proc;
                    }
                }
            }

            p.out_prod.push(&out_frame);
            frames_processed += 1;

            if self.verbose && last_report.elapsed().as_secs() >= 2 {
                let mic_peak = mic_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                let out_peak = out_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                println!(
                    "[diag] frames={}, mic_peak={:.4}, out_peak={:.4}, mic_buf={}, ref_buf={}, out_buf={}",
                    frames_processed,
                    mic_peak,
                    out_peak,
                    p.mic_cons.available(),
                    p.ref_cons.available(),
                    p.out_prod.available(),
                );
                last_report = Instant::now();
            }
        }
    }
}
