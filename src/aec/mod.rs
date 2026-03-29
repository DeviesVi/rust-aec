// AEC processing engine using sonora (pure Rust WebRTC AEC3 port).

use anyhow::Result;
use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig};

/// Frame size in samples at 48kHz (10ms).
pub const FRAME_SIZE: usize = 480;
/// Number of audio channels.
pub const NUM_CHANNELS: usize = 1;
/// Sample rate in Hz.
pub const SAMPLE_RATE: usize = 48_000;

pub struct AecProcessor {
    apm: AudioProcessing,
}

impl AecProcessor {
    pub fn new() -> Result<Self> {
        let stream_config = StreamConfig::new(SAMPLE_RATE as u32, NUM_CHANNELS as u16);
        let config = Config {
            echo_canceller: Some(EchoCanceller::default()),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(stream_config)
            .render_config(stream_config)
            .build();
        Ok(Self { apm })
    }

    /// Process one 10ms frame.
    /// `mic_frame` and `ref_frame` must each be exactly FRAME_SIZE samples.
    /// Returns processed (echo-cancelled) samples.
    pub fn process_frame(&mut self, mic_frame: &[f32], ref_frame: &[f32], out: &mut [f32]) {
        // Feed far-end (speaker/reference) signal.
        let mut render_out = vec![0.0f32; FRAME_SIZE];
        if let Err(e) = self.apm.process_render_f32(
            &[ref_frame],
            &mut [&mut render_out],
        ) {
            eprintln!("[aec] process_render error: {e}");
        }

        // Process near-end (microphone) signal — echo cancellation applied here.
        if let Err(e) = self.apm.process_capture_f32(
            &[mic_frame],
            &mut [out],
        ) {
            eprintln!("[aec] process_capture error: {e}");
            // Passthrough mic audio on error.
            out.copy_from_slice(mic_frame);
        }
    }
}
