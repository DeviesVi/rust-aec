// Session monitor: detects when programs start/stop recording from any capture
// device (excluding the real mic).
//
// Uses two complementary mechanisms:
//   - IAudioSessionNotification COM callback for instant Resume on session creation.
//   - 1-second background poll for debounced Pause when all sessions end.
//
// This allows the mic to be released (indicator off) when idle while keeping
// startup latency to only the mic WASAPI open time (~20-50 ms).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::core::implement;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl, IAudioSessionEnumerator,
    IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
    IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::Audio::eCapture;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use crate::engine::EngineCommand;

/// How long to wait after sessions drop to zero before pausing the engine.
const PAUSE_DEBOUNCE: Duration = Duration::from_secs(3);

/// Interval for the Pause-detection poll.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// COM callback: fired immediately when any new session is created on a device.
// ---------------------------------------------------------------------------

#[implement(IAudioSessionNotification)]
struct SessionNotification {
    cmd_tx: Sender<EngineCommand>,
}

impl IAudioSessionNotification_Impl for SessionNotification_Impl {
    fn OnSessionCreated(
        &self,
        _new_session: Option<&IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        // A new recording session just appeared — wake the engine immediately.
        let _ = self.cmd_tx.send(EngineCommand::Resume);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Register IAudioSessionNotification on every active capture device except
/// `excluded_id`.  Returns the (manager, notification) pairs so the caller
/// can keep them alive for the duration of the session.
fn register_notifications(
    excluded_id: Option<&str>,
    cmd_tx: &Sender<EngineCommand>,
) -> Vec<(IAudioSessionManager2, IAudioSessionNotification)> {
    let mut keepers = Vec::new();
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(
            &MMDeviceEnumerator, None, CLSCTX_ALL,
        ) else {
            return keepers;
        };

        let Ok(collection) =
            enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        else {
            return keepers;
        };
        let collection: IMMDeviceCollection = collection;

        let count = match collection.GetCount() {
            Ok(n) => n,
            Err(_) => return keepers,
        };

        for i in 0..count {
            let device: IMMDevice = match collection.Item(i) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Skip excluded mic device.
            if let Some(excl) = excluded_id {
                if let Ok(pwstr) = device.GetId() {
                    let id = pwstr.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pwstr.0 as *const _));
                    if id == excl {
                        continue;
                    }
                }
            }

            let manager: IAudioSessionManager2 =
                match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

            let notif: IAudioSessionNotification =
                SessionNotification { cmd_tx: cmd_tx.clone() }.into();

            if manager.RegisterSessionNotification(&notif).is_ok() {
                keepers.push((manager, notif));
            }
        }
    }
    keepers
}

/// Count active recording sessions on all capture devices except `excluded_id`.
fn count_active_sessions(excluded_id: Option<&str>) -> usize {
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(
            &MMDeviceEnumerator, None, CLSCTX_ALL,
        ) else {
            return 0;
        };

        let Ok(collection) =
            enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        else {
            return 0;
        };
        let collection: IMMDeviceCollection = collection;

        let device_count = match collection.GetCount() {
            Ok(n) => n,
            Err(_) => return 0,
        };

        let mut active = 0usize;

        for i in 0..device_count {
            let device: IMMDevice = match collection.Item(i) {
                Ok(d) => d,
                Err(_) => continue,
            };

            if let Some(excl) = excluded_id {
                if let Ok(pwstr) = device.GetId() {
                    let id = pwstr.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pwstr.0 as *const _));
                    if id == excl {
                        continue;
                    }
                }
            }

            let manager: IAudioSessionManager2 =
                match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

            let session_enum: IAudioSessionEnumerator =
                match manager.GetSessionEnumerator() {
                    Ok(e) => e,
                    Err(_) => continue,
                };

            let session_count = match session_enum.GetCount() {
                Ok(c) => c,
                Err(_) => continue,
            };

            for j in 0..session_count {
                let session: IAudioSessionControl =
                    match session_enum.GetSession(j) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                if matches!(session.GetState(), Ok(s) if s == AudioSessionStateActive) {
                    active += 1;
                }
            }
        }

        active
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Runs on its own thread.
/// - Registers COM callbacks for instant Resume on new session creation.
/// - Polls every second to send Pause after sessions disappear for 3 s.
pub fn session_monitor_loop(
    excluded_mic_id: Option<String>,
    cmd_tx: Sender<EngineCommand>,
    stop: Arc<AtomicBool>,
    verbose: bool,
) {
    crate::audio::device::com_init().expect("COM init failed in session-monitor thread");

    // Register callbacks for instant detection of new sessions.
    let _keepers = register_notifications(excluded_mic_id.as_deref(), &cmd_tx);
    if verbose {
        eprintln!("[monitor] registered session notifications on {} device(s)", _keepers.len());
    }

    // Initial check: sessions may already exist before we registered callbacks.
    let initial = count_active_sessions(excluded_mic_id.as_deref());
    let mut engine_active = initial > 0;
    if engine_active {
        if verbose {
            eprintln!("[monitor] {} session(s) already active at startup — resuming", initial);
        }
        let _ = cmd_tx.send(EngineCommand::Resume);
    }
    let mut zero_since: Option<Instant> = if engine_active { None } else { Some(Instant::now()) };

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        std::thread::sleep(POLL_INTERVAL);

        let active = count_active_sessions(excluded_mic_id.as_deref());

        if active > 0 {
            zero_since = None;
            if !engine_active {
                // Callbacks should have sent Resume already, but be safe.
                engine_active = true;
                if verbose {
                    eprintln!("[monitor] {} session(s) active — resuming", active);
                }
                let _ = cmd_tx.send(EngineCommand::Resume);
            }
        } else {
            let since = zero_since.get_or_insert_with(Instant::now);
            if engine_active && since.elapsed() >= PAUSE_DEBOUNCE {
                engine_active = false;
                if verbose {
                    eprintln!("[monitor] no active sessions — pausing engine");
                }
                let _ = cmd_tx.send(EngineCommand::Pause);
            }
        }
    }

    // Unregister callbacks on shutdown.
    for (manager, notif) in &_keepers {
        unsafe { let _ = manager.UnregisterSessionNotification(notif); }
    }
}
