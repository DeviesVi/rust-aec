// WASAPI render client: outputs processed audio to a virtual audio cable device.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioRenderClient,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::aec::SAMPLE_RATE;
use crate::audio::device::{self, CoTaskMemGuard, HandleGuard};
use crate::audio::pcm::{resample_into, resize_zeroed};
use crate::sync::AudioConsumer;

/// Run the render loop, pulling processed samples from `consumer` and writing
/// them to the given render device (expected to be a virtual audio cable).
/// While `paused` is true silence is written to the device without consuming
/// from the ring, keeping the WASAPI stream alive.
/// Blocks until `stop` is set to true.
pub fn render_loop(
    device_id: &str,
    mut consumer: AudioConsumer,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    let mm_device = device::open_device_by_id(device_id)?;
    unsafe {
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)?;

        let pwfx = CoTaskMemGuard::new(audio_client.GetMixFormat()?);
        let wfx = &*pwfx.get();

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000,
            0,
            pwfx.get(),
            None,
        )?;

        let buffer_size = audio_client.GetBufferSize()?;

        let event = HandleGuard::new(CreateEventW(None, false, false, None)?);
        audio_client.SetEventHandle(event.get())?;

        let render_client: IAudioRenderClient = audio_client.GetService()?;

        audio_client.Start()?;

        let device_channels = wfx.nChannels as usize;
        let device_rate = wfx.nSamplesPerSec as usize;
        let bits = wfx.wBitsPerSample;
        let mut mono_buf = Vec::new();
        let mut resampled_buf = Vec::new();

        println!(
            "[render] device: channels={}, rate={}, bits={}, buffer_size={}",
            device_channels, device_rate, bits, buffer_size
        );

        while !stop.load(Ordering::Relaxed) {
            let _ = WaitForSingleObject(event.get(), 20);

            let padding = audio_client.GetCurrentPadding()?;
            let available_frames = (buffer_size - padding) as usize;
            if available_frames == 0 {
                continue;
            }

            // While paused: write silence directly without touching the ring.
            if paused.load(Ordering::Relaxed) {
                let _buffer = render_client.GetBuffer(available_frames as u32)?;
                render_client
                    .ReleaseBuffer(available_frames as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                continue;
            }

            // Read mono f32 samples from the consumer.
            let mono_frames_needed = if device_rate != SAMPLE_RATE {
                ((available_frames as f64) * (SAMPLE_RATE as f64 / device_rate as f64)).ceil()
                    as usize
            } else {
                available_frames
            };

            resize_zeroed(&mut mono_buf, mono_frames_needed);
            consumer.pop(&mut mono_buf);
            // Unread portion stays zero (silence), ensuring gap-free output.

            // Resample if needed.
            let mono_buf = if device_rate != SAMPLE_RATE {
                resample_into(&mono_buf, SAMPLE_RATE, device_rate, &mut resampled_buf);
                resampled_buf.as_slice()
            } else {
                mono_buf.as_slice()
            };

            let frames_to_write = mono_buf.len().min(available_frames);
            if frames_to_write == 0 {
                continue;
            }

            let buffer = render_client.GetBuffer(frames_to_write as u32)?;
            write_to_device_buffer(buffer, &mono_buf[..frames_to_write], device_channels, bits);
            render_client.ReleaseBuffer(frames_to_write as u32, 0)?;
        }

        audio_client.Stop()?;
    }
    Ok(())
}

/// Write mono f32 samples into a WASAPI render buffer (multi-channel, various bit depths).
unsafe fn write_to_device_buffer(buffer: *mut u8, mono: &[f32], channels: usize, bits: u16) {
    unsafe {
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
}
