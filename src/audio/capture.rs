// Microphone WASAPI capture thread.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::aec::SAMPLE_RATE;
use crate::audio::device::{self, CoTaskMemGuard, HandleGuard};
use crate::audio::pcm::{convert_to_f32_mono_into, resample_into, resize_zeroed};
use crate::sync::AudioProducer;

/// Run the microphone capture loop. Pushes f32 samples into `producer`.
/// Blocks until `stop` is set to true.
pub fn capture_loop(
    device_id: &str,
    mut producer: AudioProducer,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mm_device = device::open_device_by_id(device_id)?;
    unsafe {
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)?;

        let pwfx = CoTaskMemGuard::new(audio_client.GetMixFormat()?);
        let wfx = &*pwfx.get();

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000, // 10ms in 100ns units
            0,
            pwfx.get(),
            None,
        )?;

        let event = HandleGuard::new(CreateEventW(None, false, false, None)?);
        audio_client.SetEventHandle(event.get())?;

        let capture_client: IAudioCaptureClient = audio_client.GetService()?;

        audio_client.Start()?;

        let device_channels = wfx.nChannels as usize;
        let device_rate = wfx.nSamplesPerSec as usize;
        let bits = wfx.wBitsPerSample;
        let mut mono_buf = Vec::new();
        let mut resampled_buf = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            let _ = WaitForSingleObject(event.get(), 20);

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

                // AUDCLNT_BUFFERFLAGS_SILENT (0x2): buffer content is undefined.
                let samples = if flags & 0x2 != 0 {
                    resize_zeroed(&mut mono_buf, frames);
                    mono_buf.as_slice()
                } else {
                    convert_to_f32_mono_into(buffer, frames, device_channels, bits, &mut mono_buf);
                    mono_buf.as_slice()
                };

                let samples = if device_rate != SAMPLE_RATE {
                    resample_into(samples, device_rate, SAMPLE_RATE, &mut resampled_buf);
                    resampled_buf.as_slice()
                } else {
                    samples
                };

                producer.push(samples);

                capture_client.ReleaseBuffer(num_frames)?;
                packet_size = capture_client.GetNextPacketSize()?;
            }
        }

        audio_client.Stop()?;
    }
    Ok(())
}
