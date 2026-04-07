// Session monitor: fully callback-driven, counter-free.
//
// Every relevant audio session callback (creation, state change, disconnect)
// calls recheck(), which queries the OS directly for the current active
// session count and sends Resume or Pause if the state changed.
//
// No internal counter is maintained — the OS is the single source of truth.
// This makes the monitor correct regardless of startup order, missed events,
// or rapid session churn.
//
// Device targeting: the monitor is given the render-side device ID (e.g. CABLE
// Input). At startup it reads that device's ContainerID — the GUID shared by
// all endpoints of the same virtual/physical device.  Only capture endpoints
// whose ContainerID matches are watched, so sessions on unrelated microphones
// (second real mic, webcam, etc.) are never counted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use windows::core::{implement, GUID};
use windows_core::Interface;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_ContainerId;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionEvents,
    IAudioSessionEvents_Impl, IAudioSessionManager2, IAudioSessionNotification,
    IAudioSessionNotification_Impl, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    MMDeviceEnumerator, AudioSessionStateActive, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::Audio::eCapture;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

use crate::engine::EngineCommand;

const VT_CLSID: u16 = 72;

// ---------------------------------------------------------------------------
// Shared state — no counter, just the last command we sent.
// ---------------------------------------------------------------------------

struct SharedState {
    /// ContainerID of the output render device (e.g. CABLE Input).
    /// Only capture endpoints with this ContainerID are watched.
    /// None = no output device; monitor registers nothing and stays paused.
    watch_container_id: Option<GUID>,
    cmd_tx: Sender<EngineCommand>,
    /// true = we last sent Resume; false = we last sent Pause (or haven't sent yet).
    engine_running: AtomicBool,
    verbose: bool,
    /// WASAPI does NOT AddRef IAudioSessionEvents on RegisterAudioSessionNotification;
    /// the caller must keep the objects alive for the duration of monitoring.
    session_events: Mutex<Vec<(IAudioSessionControl, IAudioSessionEvents)>>,
}

unsafe impl Send for SharedState {}
unsafe impl Sync for SharedState {}

// ---------------------------------------------------------------------------
// ContainerID helpers.
// ---------------------------------------------------------------------------

/// Read the ContainerID property from any IMMDevice endpoint.
/// Render and capture endpoints of the same virtual/physical device share it.
unsafe fn get_container_id(device: &IMMDevice) -> Option<GUID> {
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM(0)) }.ok()?;
    let pv = unsafe { store.GetValue(&PKEY_Device_ContainerId) }.ok()?;
    // PROPVARIANT layout: vt (u16) at offset 0.
    // For VT_CLSID (72): *mut GUID at offset 8 (after vt + 6 bytes reserved).
    unsafe {
        let raw = &pv as *const _ as *const u8;
        let vt = u16::from_ne_bytes([*raw, *raw.add(1)]);
        if vt != VT_CLSID { return None; }
        let guid_ptr = *(raw.add(8) as *const *const GUID);
        if guid_ptr.is_null() { return None; }
        Some(*guid_ptr) // copy before pv drops and PropVariantClear frees the pointer
    }
}

/// Given the device ID of a render endpoint, return its ContainerID.
fn resolve_watch_container(output_render_id: &str) -> Option<GUID> {
    unsafe {
        let device = crate::audio::device::open_device_by_id(output_render_id).ok()?;
        get_container_id(&device)
    }
}

// ---------------------------------------------------------------------------
// Recheck: ask the OS for the real count, send command only on change.
// ---------------------------------------------------------------------------

fn recheck(state: &SharedState) {
    // COM may not be initialized on the callback thread — initialize lazily.
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }

    let active = count_active_sessions(state.watch_container_id.as_ref());
    if active > 0 {
        if !state.engine_running.swap(true, Ordering::SeqCst) {
            if state.verbose {
                eprintln!("[monitor] {} active session(s) → Resume", active);
            }
            let _ = state.cmd_tx.send(EngineCommand::Resume);
        }
    } else {
        if state.engine_running.swap(false, Ordering::SeqCst) {
            if state.verbose {
                eprintln!("[monitor] 0 active sessions → Pause");
            }
            let _ = state.cmd_tx.send(EngineCommand::Pause);
        }
    }
}

// ---------------------------------------------------------------------------
// Count active sessions on capture endpoints matching the target ContainerID.
// Returns 0 immediately if watch_cid is None (no output device configured).
// ---------------------------------------------------------------------------

fn count_active_sessions(watch_cid: Option<&GUID>) -> usize {
    let Some(target_cid) = watch_cid else { return 0; };
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else { return 0; };

        let Ok(collection) =
            enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        else { return 0; };
        let collection: IMMDeviceCollection = collection;

        let dev_count = match collection.GetCount() { Ok(n) => n, Err(_) => return 0 };
        let mut active = 0usize;

        for i in 0..dev_count {
            let device: IMMDevice = match collection.Item(i) { Ok(d) => d, Err(_) => continue };

            // Filter by ContainerID: only watch the virtual cable's capture endpoint(s).
            match get_container_id(&device) {
                Some(cid) if cid == *target_cid => {} // match — proceed
                _ => continue,                        // no match — skip
            }

            let manager: IAudioSessionManager2 =
                match device.Activate(CLSCTX_ALL, None) { Ok(m) => m, Err(_) => continue };

            let Ok(session_enum) = manager.GetSessionEnumerator() else { continue };
            let session_enum: IAudioSessionEnumerator = session_enum;
            let session_count = match session_enum.GetCount() { Ok(c) => c, Err(_) => continue };

            for j in 0..session_count {
                let session: IAudioSessionControl =
                    match session_enum.GetSession(j) { Ok(s) => s, Err(_) => continue };
                if !matches!(session.GetState(), Ok(s) if s == AudioSessionStateActive) {
                    continue;
                }
                // Exclude sessions owned by this process.
                if let Ok(s2) = session.cast::<IAudioSessionControl2>() {
                    if s2.GetProcessId().ok() == Some(GetCurrentProcessId()) {
                        continue;
                    }
                }
                active += 1;
            }
        }

        active
    }
}

// ---------------------------------------------------------------------------
// Per-session events — every event triggers a recheck.
// ---------------------------------------------------------------------------

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    state: Arc<SharedState>,
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(
        &self, _: &windows::core::PCWSTR, _: *const windows::core::GUID,
    ) -> windows::core::Result<()> { Ok(()) }

    fn OnIconPathChanged(
        &self, _: &windows::core::PCWSTR, _: *const windows::core::GUID,
    ) -> windows::core::Result<()> { Ok(()) }

    fn OnSimpleVolumeChanged(
        &self, _: f32, _: BOOL, _: *const windows::core::GUID,
    ) -> windows::core::Result<()> { Ok(()) }

    fn OnChannelVolumeChanged(
        &self, _: u32, _: *const f32, _: u32, _: *const windows::core::GUID,
    ) -> windows::core::Result<()> { Ok(()) }

    fn OnGroupingParamChanged(
        &self, _: *const windows::core::GUID, _: *const windows::core::GUID,
    ) -> windows::core::Result<()> { Ok(()) }

    fn OnStateChanged(&self, _: AudioSessionState) -> windows::core::Result<()> {
        recheck(&self.state);
        Ok(())
    }

    fn OnSessionDisconnected(
        &self, _: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        recheck(&self.state);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-device session creation notification.
// ---------------------------------------------------------------------------

#[implement(IAudioSessionNotification)]
struct SessionNotification {
    state: Arc<SharedState>,
}

impl IAudioSessionNotification_Impl for SessionNotification_Impl {
    fn OnSessionCreated(
        &self, new_session: Option<&IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        if let Some(session) = new_session {
            let evts: IAudioSessionEvents =
                SessionEvents { state: Arc::clone(&self.state) }.into();
            unsafe { let _ = session.RegisterAudioSessionNotification(&evts); }
            // Must keep evts alive — WASAPI does not AddRef it.
            self.state.session_events.lock().unwrap().push((session.clone(), evts));
        }
        recheck(&self.state);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Register IAudioSessionNotification + existing-session events on the
// capture endpoint(s) whose ContainerID matches watch_cid.
// Does nothing and returns empty vec if watch_cid is None.
// Returns keepers that must stay alive for the duration of the session.
// ---------------------------------------------------------------------------

fn register_all(
    watch_cid: Option<&GUID>,
    state: &Arc<SharedState>,
) -> Vec<(IAudioSessionManager2, IAudioSessionNotification)> {
    let mut keepers = Vec::new();
    let Some(target_cid) = watch_cid else { return keepers; };
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else { return keepers; };

        let Ok(collection) =
            enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
        else { return keepers; };
        let collection: IMMDeviceCollection = collection;

        let dev_count = match collection.GetCount() { Ok(n) => n, Err(_) => return keepers };

        for i in 0..dev_count {
            let device: IMMDevice = match collection.Item(i) { Ok(d) => d, Err(_) => continue };

            match get_container_id(&device) {
                Some(cid) if cid == *target_cid => {}
                _ => continue,
            }

            let manager: IAudioSessionManager2 =
                match device.Activate(CLSCTX_ALL, None) { Ok(m) => m, Err(_) => continue };

            let notif: IAudioSessionNotification =
                SessionNotification { state: Arc::clone(state) }.into();
            if manager.RegisterSessionNotification(&notif).is_ok() {
                keepers.push((manager.clone(), notif));
            }

            // Register events on sessions that already exist.
            let Ok(session_enum) = manager.GetSessionEnumerator() else { continue };
            let session_enum: IAudioSessionEnumerator = session_enum;
            let session_count = match session_enum.GetCount() { Ok(c) => c, Err(_) => continue };
            for j in 0..session_count {
                let session: IAudioSessionControl =
                    match session_enum.GetSession(j) { Ok(s) => s, Err(_) => continue };
                let evts: IAudioSessionEvents =
                    SessionEvents { state: Arc::clone(state) }.into();
                let _ = session.RegisterAudioSessionNotification(&evts);
                state.session_events.lock().unwrap().push((session, evts));
            }
        }
    }
    keepers
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

/// `output_render_id`: device ID of the render endpoint rust-aec writes to
/// (e.g. CABLE Input).  The monitor watches only the capture endpoint(s) that
/// share the same ContainerID — i.e. the other side of the same virtual cable.
pub fn session_monitor_loop(
    output_render_id: Option<String>,
    cmd_tx: Sender<EngineCommand>,
    stop: Arc<AtomicBool>,
    verbose: bool,
) {
    crate::audio::device::com_init().expect("COM init failed in session-monitor thread");

    let watch_container_id = output_render_id
        .as_deref()
        .and_then(resolve_watch_container);

    if verbose {
        match &watch_container_id {
            Some(cid) => eprintln!("[monitor] watching ContainerID {:?}", cid),
            None => eprintln!("[monitor] no output device — monitor inactive (engine stays paused)"),
        }
    }

    let state = Arc::new(SharedState {
        watch_container_id,
        cmd_tx,
        engine_running: AtomicBool::new(false),
        verbose,
        session_events: Mutex::new(Vec::new()),
    });

    let _keepers = register_all(state.watch_container_id.as_ref(), &state);

    if verbose {
        eprintln!("[monitor] registered on {} device(s)", _keepers.len());
    }

    recheck(&state);

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    for (manager, notif) in &_keepers {
        unsafe { let _ = manager.UnregisterSessionNotification(notif); }
    }
    let pairs = state.session_events.lock().unwrap();
    for (session, evts) in pairs.iter() {
        unsafe { let _ = session.UnregisterAudioSessionNotification(evts); }
    }
}
