// Speaker loopback WASAPI capture thread.
// Captures system audio output (what you hear) for use as the AEC reference signal.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::aec::SAMPLE_RATE;
use crate::audio::device::{self, CoTaskMemGuard, HandleGuard};
use crate::audio::pcm::{convert_to_f32_mono_into, resample_into};
use crate::sync::AudioProducer;

/// Run the loopback capture loop on a render device.
/// Captures system audio output and pushes mono f32 samples into `producer`.
/// While `paused` is true the WASAPI buffer is still drained but data is discarded.
/// Blocks until `stop` is set to true.
pub fn loopback_loop(
    render_device_id: &str,
    mut producer: AudioProducer,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    let mm_device = device::open_device_by_id(render_device_id)?;
    unsafe {
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)?;

        let pwfx = CoTaskMemGuard::new(audio_client.GetMixFormat()?);
        let wfx = &*pwfx.get();

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000,
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
                capture_client.GetBuffer(&mut buffer, &mut num_frames, &mut flags, None, None)?;

                let frames = num_frames as usize;

                // AUDCLNT_BUFFERFLAGS_SILENT (0x2): no real audio playing.
                // Skip conversion/resampling/push so the engine sees an empty
                // ref buffer and can bypass AEC entirely.
                // While paused: drain the WASAPI buffer but discard — no push.
                if flags & 0x2 != 0 || paused.load(Ordering::Relaxed) {
                    capture_client.ReleaseBuffer(num_frames)?;
                    packet_size = capture_client.GetNextPacketSize()?;
                    continue;
                }

                convert_to_f32_mono_into(buffer, frames, device_channels, bits, &mut mono_buf);

                let samples = if device_rate != SAMPLE_RATE {
                    resample_into(&mono_buf, device_rate, SAMPLE_RATE, &mut resampled_buf);
                    resampled_buf.as_slice()
                } else {
                    mono_buf.as_slice()
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
