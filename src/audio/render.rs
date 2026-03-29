// WASAPI render client: outputs processed audio to a virtual audio cable device.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::aec::SAMPLE_RATE;
use crate::audio::device;
use crate::sync::AudioConsumer;

/// Run the render loop, pulling processed samples from `consumer` and writing
/// them to the given render device (expected to be a virtual audio cable).
/// Blocks until `stop` is set to true.
pub fn render_loop(
    device_id: &str,
    mut consumer: AudioConsumer,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mm_device = device::open_device_by_id(device_id)?;
    unsafe {
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)?;

        let pwfx = audio_client.GetMixFormat()?;
        let wfx = &*pwfx;

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000,
            0,
            pwfx,
            None,
        )?;

        let buffer_size = audio_client.GetBufferSize()?;

        let event: HANDLE = CreateEventW(None, false, false, None)?;
        audio_client.SetEventHandle(event)?;

        let render_client: IAudioRenderClient = audio_client.GetService()?;

        audio_client.Start()?;

        let device_channels = wfx.nChannels as usize;
        let device_rate = wfx.nSamplesPerSec as usize;
        let bits = wfx.wBitsPerSample;

        while !stop.load(Ordering::Relaxed) {
            let _ = WaitForSingleObject(event, 100);

            let padding = audio_client.GetCurrentPadding()?;
            let available_frames = (buffer_size - padding) as usize;
            if available_frames == 0 {
                continue;
            }

            // Read mono f32 samples from the consumer.
            let mono_frames_needed = if device_rate != SAMPLE_RATE {
                ((available_frames as f64) * (SAMPLE_RATE as f64 / device_rate as f64)).ceil()
                    as usize
            } else {
                available_frames
            };

            let mut mono_buf = vec![0.0f32; mono_frames_needed];
            let read = consumer.pop(&mut mono_buf);
            if read == 0 {
                continue;
            }
            mono_buf.truncate(read);

            // Resample if needed.
            let mono_buf = if device_rate != SAMPLE_RATE {
                simple_resample(&mono_buf, SAMPLE_RATE, device_rate)
            } else {
                mono_buf
            };

            let frames_to_write = mono_buf.len().min(available_frames);
            if frames_to_write == 0 {
                continue;
            }

            let buffer = render_client.GetBuffer(frames_to_write as u32)?;
            write_to_device_buffer(
                buffer,
                &mono_buf[..frames_to_write],
                device_channels,
                bits,
            );
            render_client.ReleaseBuffer(frames_to_write as u32, 0)?;
        }

        audio_client.Stop()?;
    }
    Ok(())
}

/// Write mono f32 samples into a WASAPI render buffer (multi-channel, various bit depths).
unsafe fn write_to_device_buffer(
    buffer: *mut u8,
    mono: &[f32],
    channels: usize,
    bits: u16,
) {
    match bits {
        32 => {
            let data =
                std::slice::from_raw_parts_mut(buffer as *mut f32, mono.len() * channels);
            for (i, &sample) in mono.iter().enumerate() {
                for ch in 0..channels {
                    data[i * channels + ch] = sample;
                }
            }
        }
        16 => {
            let data =
                std::slice::from_raw_parts_mut(buffer as *mut i16, mono.len() * channels);
            for (i, &sample) in mono.iter().enumerate() {
                let s = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                for ch in 0..channels {
                    data[i * channels + ch] = s;
                }
            }
        }
        _ => {}
    }
}

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
