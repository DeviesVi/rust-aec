// Speaker loopback WASAPI capture thread.
// Captures system audio output (what you hear) for use as the AEC reference signal.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::aec::SAMPLE_RATE;
use crate::audio::device;
use crate::sync::AudioProducer;

/// Run the loopback capture loop on a render device.
/// Captures system audio output and pushes mono f32 samples into `producer`.
/// Blocks until `stop` is set to true.
pub fn loopback_loop(
    render_device_id: &str,
    mut producer: AudioProducer,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mm_device = device::open_device_by_id(render_device_id)?;
    unsafe {
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)?;

        let pwfx = audio_client.GetMixFormat()?;
        let wfx = &*pwfx;

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000,
            0,
            pwfx,
            None,
        )?;

        let event: HANDLE = CreateEventW(None, false, false, None)?;
        audio_client.SetEventHandle(event)?;

        let capture_client: IAudioCaptureClient = audio_client.GetService()?;

        audio_client.Start()?;

        let device_channels = wfx.nChannels as usize;
        let device_rate = wfx.nSamplesPerSec as usize;
        let bits = wfx.wBitsPerSample;

        while !stop.load(Ordering::Relaxed) {
            let _ = WaitForSingleObject(event, 100);

            let mut packet_size = capture_client.GetNextPacketSize()?;
            while packet_size > 0 {
                let mut buffer = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                capture_client.GetBuffer(
                    &mut buffer,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )?;

                let frames = num_frames as usize;
                let samples = convert_to_f32_mono(buffer, frames, device_channels, bits);

                let samples = if device_rate != SAMPLE_RATE {
                    simple_resample(&samples, device_rate, SAMPLE_RATE)
                } else {
                    samples
                };

                producer.push(&samples);

                capture_client.ReleaseBuffer(num_frames)?;
                packet_size = capture_client.GetNextPacketSize()?;
            }
        }

        audio_client.Stop()?;
    }
    Ok(())
}

/// Convert raw WASAPI buffer to mono f32.
unsafe fn convert_to_f32_mono(buffer: *const u8, frames: usize, channels: usize, bits: u16) -> Vec<f32> { unsafe {
    let mut mono = Vec::with_capacity(frames);
    match bits {
        32 => {
            let data = std::slice::from_raw_parts(buffer as *const f32, frames * channels);
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().sum();
                mono.push(sum / channels as f32);
            }
        }
        16 => {
            let data = std::slice::from_raw_parts(buffer as *const i16, frames * channels);
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|&s| s as f32 / 32768.0).sum();
                mono.push(sum / channels as f32);
            }
        }
        _ => {
            mono.resize(frames, 0.0);
        }
    }
    mono
}}

/// Naive linear resampling.
fn simple_resample(input: &[f32], from_rate: usize, to_rate: usize) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = (src_idx - idx as f64) as f32;
        let s0 = input[idx.min(input.len() - 1)];
        let s1 = input[(idx + 1).min(input.len() - 1)];
        output.push(s0 + frac * (s1 - s0));
    }
    output
}
