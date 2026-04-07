// Session monitor: polls all capture devices for active recording sessions.
// When any sessions are found on capture devices (excluding the real mic),
// the engine is instructed to resume (start pipeline / open mic).
// When no sessions remain for a debounce period, the engine is paused
// (pipeline stopped, mic released so the recording indicator turns off).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl, IAudioSessionEnumerator,
    IAudioSessionManager2, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::Audio::eCapture;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use crate::engine::EngineCommand;

/// How long to wait after sessions drop to zero before pausing the engine.
const PAUSE_DEBOUNCE: Duration = Duration::from_secs(3);

/// Poll interval.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Count active recording sessions on all capture devices except `excluded_id`.
unsafe fn count_active_sessions(excluded_id: Option<&str>) -> usize {
    let Ok(enumerator) = (unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }) else {
        return 0;
    };

    let Ok(collection) = (unsafe {
        enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
    }) else {
        return 0;
    };
    let collection: IMMDeviceCollection = collection;

    let device_count = match unsafe { collection.GetCount() } {
        Ok(n) => n,
        Err(_) => return 0,
    };

    let mut active = 0usize;

    for i in 0..device_count {
        let device: IMMDevice = match unsafe { collection.Item(i) } {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Skip the excluded mic device.
        if let Some(excl) = excluded_id {
            if let Ok(pwstr) = unsafe { device.GetId() } {
                let id = unsafe { pwstr.to_string() }.unwrap_or_default();
                unsafe { CoTaskMemFree(Some(pwstr.0 as *const _)) };
                if id == excl {
                    continue;
                }
            }
        }

        // Get session manager for this capture device.
        let manager: IAudioSessionManager2 =
            match unsafe { device.Activate(CLSCTX_ALL, None) } {
                Ok(m) => m,
                Err(_) => continue,
            };

        let enumerator: IAudioSessionEnumerator =
            match unsafe { manager.GetSessionEnumerator() } {
                Ok(e) => e,
                Err(_) => continue,
            };

        let session_count = match unsafe { enumerator.GetCount() } {
            Ok(c) => c,
            Err(_) => continue,
        };

        for j in 0..session_count {
            let session: IAudioSessionControl = match unsafe { enumerator.GetSession(j) } {
                Ok(s) => s,
                Err(_) => continue,
            };
            if matches!(unsafe { session.GetState() }, Ok(s) if s == AudioSessionStateActive) {
                active += 1;
            }
        }
    }

    active
}

/// Runs on its own thread. Polls capture devices and sends engine commands.
/// Stopped when `stop` is set to true.
pub fn session_monitor_loop(
    excluded_mic_id: Option<String>,
    cmd_tx: Sender<EngineCommand>,
    stop: Arc<AtomicBool>,
    verbose: bool,
) {
    crate::audio::device::com_init().expect("COM init failed in session-monitor thread");

    let mut engine_active = false;
    let mut zero_since: Option<Instant> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let active = unsafe { count_active_sessions(excluded_mic_id.as_deref()) };

        if active > 0 {
            zero_since = None;
            if !engine_active {
                engine_active = true;
                if verbose {
                    eprintln!(
                        "[monitor] {} active capture session(s) detected — resuming engine",
                        active
                    );
                }
                let _ = cmd_tx.send(EngineCommand::Resume);
            }
        } else {
            let since = zero_since.get_or_insert_with(Instant::now);
            if engine_active && since.elapsed() >= PAUSE_DEBOUNCE {
                engine_active = false;
                if verbose {
                    eprintln!("[monitor] no active capture sessions — pausing engine");
                }
                let _ = cmd_tx.send(EngineCommand::Pause);
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}
