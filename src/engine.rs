// Audio engine: manages AEC processing and audio threads.
// Runs on its own thread; receives commands from the tray via crossbeam channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{bail, Result};
use crossbeam_channel::Receiver;

use crate::aec::{AecProcessor, FRAME_SIZE, SAMPLE_RATE};
use crate::audio::device;
use crate::sync::AudioRingBuf;
use crate::tray::TrayState;

pub enum EngineCommand {
    SetMicDevice(String),
    SetSpeakerDevice(String),
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
    /// Updates state and returns the mic id if found.
    fn try_find_mic(&self) -> Option<String> {
        let capture = device::list_capture_devices().ok()?;
        // Try default first.
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
        if mic_id.is_some() {
            let mut st = self.state.lock().unwrap();
            st.capture_devices = capture;
            st.current_mic_id = mic_id.clone();
        }
        mic_id
    }

    pub fn run(&self) -> Result<()> {
        device::com_init()?;

        let (mut mic_id, speaker_id, output_id) = {
            let st = self.state.lock().unwrap();
            (
                st.current_mic_id.clone(),
                st.current_speaker_id.clone(),
                st.current_output_id.clone(),
            )
        };

        if output_id.is_empty() {
            bail!("No output device configured");
        }

        let mut pipeline: Option<Pipeline> = None;
        let mut processor: Option<AecProcessor> = None;
        let mut mic_frame = vec![0.0f32; FRAME_SIZE];
        let mut ref_frame = vec![0.0f32; FRAME_SIZE];
        let mut out_frame = vec![0.0f32; FRAME_SIZE];
        let mut frames_processed: u64 = 0;
        let mut last_report = Instant::now();

        // Start pipeline if mic is available.
        if let Some(ref mid) = mic_id {
            match Pipeline::new(mid, &speaker_id, &output_id) {
                Ok(p) => {
                    pipeline = Some(p);
                    processor = Some(AecProcessor::new()?);
                }
                Err(e) => {
                    if self.verbose {
                        eprintln!("[engine] Failed to start pipeline: {:#}", e);
                    }
                    mic_id = None;
                    self.state.lock().unwrap().current_mic_id = None;
                }
            }
        } else if self.verbose {
            eprintln!("[engine] No microphone available, waiting for device...");
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
                    }
                    {
                        let mut st = self.state.lock().unwrap();
                        st.current_mic_id = Some(new_id.clone());
                    }
                    mic_id = Some(new_id);
                    let st = self.state.lock().unwrap();
                    pipeline = Some(Pipeline::new(
                        mic_id.as_ref().unwrap(),
                        &st.current_speaker_id,
                        &st.current_output_id,
                    )?);
                    drop(st);
                    processor = Some(AecProcessor::new()?);
                    frames_processed = 0;
                    if self.verbose {
                        eprintln!("[engine] Switched mic device, pipeline restarted.");
                    }
                }
                Ok(EngineCommand::SetSpeakerDevice(new_id)) => {
                    if let Some(ref mut p) = pipeline {
                        p.shutdown();
                    }
                    {
                        let mut st = self.state.lock().unwrap();
                        st.current_speaker_id = new_id;
                    }
                    let st = self.state.lock().unwrap();
                    if let Some(ref mid) = mic_id {
                        pipeline = Some(Pipeline::new(
                            mid,
                            &st.current_speaker_id,
                            &st.current_output_id,
                        )?);
                        processor = Some(AecProcessor::new()?);
                        frames_processed = 0;
                        if self.verbose {
                            eprintln!("[engine] Switched speaker device, pipeline restarted.");
                        }
                    }
                    drop(st);
                }
                Ok(EngineCommand::RefreshDevices) => {
                    if self.verbose {
                        eprintln!("[engine] Refreshing devices...");
                    }
                    // If we don't have a mic yet, try to find one.
                    if mic_id.is_none() {
                        if let Some(new_mic) = self.try_find_mic() {
                            mic_id = Some(new_mic);
                            let st = self.state.lock().unwrap();
                            match Pipeline::new(
                                mic_id.as_ref().unwrap(),
                                &st.current_speaker_id,
                                &st.current_output_id,
                            ) {
                                Ok(p) => {
                                    pipeline = Some(p);
                                    processor = Some(AecProcessor::new()?);
                                    frames_processed = 0;
                                    if self.verbose {
                                        eprintln!("[engine] Microphone found, pipeline started.");
                                    }
                                }
                                Err(e) => {
                                    if self.verbose {
                                        eprintln!("[engine] Failed to start pipeline: {:#}", e);
                                    }
                                    mic_id = None;
                                    drop(st);
                                    self.state.lock().unwrap().current_mic_id = None;
                                }
                            }
                        }
                    } else {
                        // Already have a mic — restart pipeline (devices may have changed).
                        if let Some(ref mut p) = pipeline {
                            p.shutdown();
                        }
                        // Re-enumerate and pick best mic.
                        if let Some(new_mic) = self.try_find_mic() {
                            mic_id = Some(new_mic);
                        }
                        let st = self.state.lock().unwrap();
                        match Pipeline::new(
                            mic_id.as_ref().unwrap(),
                            &st.current_speaker_id,
                            &st.current_output_id,
                        ) {
                            Ok(p) => {
                                pipeline = Some(p);
                                processor = Some(AecProcessor::new()?);
                                frames_processed = 0;
                                if self.verbose {
                                    eprintln!("[engine] Pipeline restarted after device refresh.");
                                }
                            }
                            Err(e) => {
                                if self.verbose {
                                    eprintln!("[engine] Failed to restart pipeline: {:#}", e);
                                }
                                pipeline = None;
                                processor = None;
                                mic_id = None;
                                drop(st);
                                self.state.lock().unwrap().current_mic_id = None;
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

            proc.process_frame(&mic_frame, &ref_frame, &mut out_frame);

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
