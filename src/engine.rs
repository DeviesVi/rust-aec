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
    pub fn run(&self) -> Result<()> {
        device::com_init()?;

        let (mic_id, speaker_id, output_id) = {
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

        let mut pipeline = Pipeline::new(&mic_id, &speaker_id, &output_id)?;
        let mut processor = AecProcessor::new()?;
        let mut mic_frame = vec![0.0f32; FRAME_SIZE];
        let mut ref_frame = vec![0.0f32; FRAME_SIZE];
        let mut out_frame = vec![0.0f32; FRAME_SIZE];

        let mut frames_processed: u64 = 0;
        let mut last_report = Instant::now();

        loop {
            // Check for commands (non-blocking).
            match self.cmd_rx.try_recv() {
                Ok(EngineCommand::Shutdown) => {
                    pipeline.shutdown();
                    return Ok(());
                }
                Ok(EngineCommand::SetMicDevice(new_id)) => {
                    pipeline.shutdown();
                    {
                        let mut st = self.state.lock().unwrap();
                        st.current_mic_id = new_id;
                    }
                    let (mic_id, speaker_id, output_id) = {
                        let st = self.state.lock().unwrap();
                        (
                            st.current_mic_id.clone(),
                            st.current_speaker_id.clone(),
                            st.current_output_id.clone(),
                        )
                    };
                    pipeline = Pipeline::new(&mic_id, &speaker_id, &output_id)?;
                    processor = AecProcessor::new()?;
                    frames_processed = 0;
                    if self.verbose {
                        eprintln!("[engine] Switched mic device, pipeline restarted.");
                    }
                }
                Ok(EngineCommand::SetSpeakerDevice(new_id)) => {
                    pipeline.shutdown();
                    {
                        let mut st = self.state.lock().unwrap();
                        st.current_speaker_id = new_id;
                    }
                    let (mic_id, speaker_id, output_id) = {
                        let st = self.state.lock().unwrap();
                        (
                            st.current_mic_id.clone(),
                            st.current_speaker_id.clone(),
                            st.current_output_id.clone(),
                        )
                    };
                    pipeline = Pipeline::new(&mic_id, &speaker_id, &output_id)?;
                    processor = AecProcessor::new()?;
                    frames_processed = 0;
                    if self.verbose {
                        eprintln!("[engine] Switched speaker device, pipeline restarted.");
                    }
                }
                Err(_) => {}
            }

            if pipeline.mic_cons.available() < FRAME_SIZE {
                thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }

            pipeline.mic_cons.pop(&mut mic_frame);

            let ref_available = pipeline.ref_cons.available().min(FRAME_SIZE);
            pipeline.ref_cons.pop(&mut ref_frame[..ref_available]);
            ref_frame[ref_available..].fill(0.0);

            processor.process_frame(&mic_frame, &ref_frame, &mut out_frame);

            pipeline.out_prod.push(&out_frame);
            frames_processed += 1;

            if self.verbose && last_report.elapsed().as_secs() >= 2 {
                let mic_peak = mic_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                let out_peak = out_frame.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
                println!(
                    "[diag] frames={}, mic_peak={:.4}, out_peak={:.4}, mic_buf={}, ref_buf={}, out_buf={}",
                    frames_processed,
                    mic_peak,
                    out_peak,
                    pipeline.mic_cons.available(),
                    pipeline.ref_cons.available(),
                    pipeline.out_prod.available(),
                );
                last_report = Instant::now();
            }
        }
    }
}
