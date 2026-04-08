# Rust AEC — Agent Guidelines

## Project Overview

Real-time Acoustic Echo Cancellation (AEC) for Windows. Captures microphone + speaker loopback via WASAPI, runs WebRTC AEC3 (sonora), and outputs clean audio to a virtual audio cable so other apps (Discord, Zoom, Teams, etc.) can use it as their microphone input. Runs as a system tray application.

## Architecture

```
Main thread:       Win32 message pump + system tray icon (src/tray.rs)
Session monitor:   WASAPI session callbacks → Resume/Pause commands (src/audio/session_monitor.rs)
Engine thread:     AEC processing loop (src/engine.rs)
  loopback-capture:  WASAPI loopback → ref_ring (src/audio/loopback.rs)   ┐
  render:            out_ring → Virtual Cable (src/audio/render.rs)        ├─ ref pipeline stays warm across Pause
  mic-capture:       WASAPI capture → mic_ring (src/audio/capture.rs)     ┘   mic still stops on Pause
```

- **Main thread**: Runs Win32 message pump for the system tray icon. Sends `EngineCommand` to the engine thread via `crossbeam-channel`.
- **Engine thread**: Owns the AEC processor + audio threads + ring buffers. The reference pipeline (loopback-capture + render) and AEC state can stay warm across `EngineCommand::Pause`/`Resume`; only `MicCapture` is torn down on `Pause`, so the real microphone is released while long-lived allocations stay stable. While paused, loopback still drains WASAPI but discards data, render writes silence with `AUDCLNT_BUFFERFLAGS_SILENT`, and the engine thread blocks on the next command instead of polling. Resume latency for restarting mic capture is still ~50–100 ms.
- **Session monitor thread**: Registers `IAudioSessionNotification` only on capture endpoint(s) whose `ContainerID` matches the configured output cable. Each callback re-queries the OS for the live active session count; own-process sessions (rust-aec's MicCapture) are excluded by PID. `IAudioSessionEvents` keepers are deduplicated by session-instance ID and pruned on disconnect/expiry so repeated pause/resume or external session churn does not grow memory usage over time. Sends `Resume`/`Pause` to the engine only when state changes.
- **Inter-thread comms**: Lock-free SPSC ring buffers (`ringbuf` crate), 200ms capacity. Commands via `crossbeam-channel`.
- **Processing**: 10ms frames (480 samples @ 48kHz). AEC via `sonora` (pure-Rust WebRTC AEC3 port).
- **Audio API**: WASAPI directly via the `windows` crate (v0.58). Windows-only.

## Key Files

| File | Purpose |
|---|---|
| `src/main.rs` | CLI parsing, device selection (with cable filtering), tray + engine startup |
| `src/engine.rs` | `AudioEngine` — AEC processing loop, `RefPipeline`/`MicCapture` lifecycle, `EngineCommand` handling |
| `src/audio/session_monitor.rs` | COM callback session monitor — detects recording sessions via `IAudioSessionNotification`/`IAudioSessionEvents`, sends `Resume`/`Pause` to engine |
| `src/tray.rs` | Win32 system tray icon, context menus, `TrayState` shared with engine |
| `src/autostart.rs` | Registry-based Windows autostart (`HKCU\...\Run`) |
| `src/audio/device.rs` | WASAPI device enumeration, substring matching, virtual cable detection |
| `src/audio/capture.rs` | Mic capture thread (shared mode, event-driven, 10ms buffer) |
| `src/audio/loopback.rs` | Speaker loopback capture (`AUDCLNT_STREAMFLAGS_LOOPBACK`) |
| `src/audio/pcm.rs` | Reusable PCM scratch helpers for conversion/resampling/zero-fill |
| `src/audio/render.rs` | Writes clean audio to virtual cable render endpoint |
| `src/aec/mod.rs` | `AecProcessor` wrapping `sonora::AudioProcessing` |
| `src/sync/mod.rs` | `AudioRingBuf` — SPSC ring buffer wrapper |
| `build.rs` | Embeds `resources/app.ico` via `embed-resource` |
| `vendor/sonora-aec3/` | Local fork of `sonora-aec3` (v0.1.0) with off-by-one fix in `adaptive_fir_filter.rs::update_size` |

## CLI Usage

```
rust_aec.exe [--verbose] [mic_name] [speaker_name] [output_name]
```

- `--verbose` / `-v`: Open a dedicated console window for diagnostic output (device lists, buffer levels, peak levels every 2s). Ctrl+C in that window exits the app.
- All positional arguments are optional substring matches (case-insensitive).
- **mic_name**: Microphone device. Default: first real (non-virtual-cable) capture device.
- **speaker_name**: Speaker for loopback. Default: Windows default render device.
- **output_name**: Virtual cable output. Default: auto-detects device containing "cable".

## Virtual Audio Cable Setup (Required)

Install [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) (free). It creates "CABLE Input" (render) and "CABLE Output" (capture) devices.

## Key Design Decisions

- **GUI subsystem (`#![windows_subsystem = "windows"]`)**: No console window on startup. With `--verbose`, allocates a dedicated console window via `AllocConsole()`. This gives reliable Ctrl+C support. Note: `AttachConsole(ATTACH_PARENT_PROCESS)` was attempted but GUI subsystem processes do not receive `CTRL_C_EVENT` even after attaching, so a dedicated window is the only reliable approach.
- **No tray crate**: Uses Win32 API directly (Shell_NotifyIconW, CreatePopupMenu, etc.) via the `windows` crate to avoid extra dependencies.
- **Cable filtering**: When auto-selecting a microphone, devices with "cable" in the name are skipped to avoid selecting a virtual cable as input.
- **Device hot-swap**: When the user changes a device via the tray menu, the entire audio pipeline is torn down and rebuilt. Mic-device changes also reset the warm AEC state so the next resume cannot reuse filters adapted to the previous microphone. The AEC re-adapts in ~1-2 seconds.
- **Shared state**: `TrayState` (device lists + current selections) is protected by `Arc<Mutex<>>`, accessed by both the tray (for menu building) and the engine (for device IDs).
- **On-demand pipeline**: The reference pipeline is started on demand and may stay alive while idle so pause/resume cycles do not repeatedly churn long-lived allocations. `MicCapture` is still stopped whenever all external recording sessions end, which keeps the real microphone handle (and the in-use indicator) released while idle.
- **WASAPI session event lifetime**: `IAudioSessionManager2::RegisterSessionNotification` and `IAudioSessionControl::RegisterAudioSessionNotification` do **not** `AddRef` the callback objects. The caller must hold strong references for the entire monitoring period. In `session_monitor.rs`, `IAudioSessionNotification` objects are stored in `_keepers` and `IAudioSessionEvents` objects in `SharedState::session_events`; dropping either silently kills all callbacks.
- **Session callback dedupe/prune**: Session-event keepers are keyed by `IAudioSessionControl2::GetSessionInstanceIdentifier()` when available. This prevents duplicate registrations between startup enumeration and `OnSessionCreated`, and lets the monitor unregister/drop expired or disconnected callbacks before the keeper vector can grow indefinitely.
- **Counter-free session detection**: Rather than maintaining a local active-session count (which can drift on missed events or late startup), each callback calls `count_active_sessions()` to query the OS directly. Sessions owned by the current process (PID match via `IAudioSessionControl2::GetProcessId`) are excluded so MicCapture's own session cannot prevent a self-Pause.

## Development Notes

- All audio conversion handles both f32 and i16 PCM formats, with mono mixdown and naive linear resampling when device sample rate != 48kHz.
- The `sonora` crate is the AEC engine (pure Rust WebRTC AEC3 port).
- Loopback capture uses WASAPI's built-in loopback mode — no extra virtual device needed for capturing speaker output.
- `vendor/sonora-aec3` is a local fork of `sonora-aec3` v0.1.0 pinned via `[patch.crates-io]` in `Cargo.toml`. The only change is a guard in `AdaptiveFirFilter::update_size()` (`adaptive_fir_filter.rs`) preventing `zero_filter` from being called with `old_size > new_size`, which caused a slice-index panic after ~37,000 frames (~6 minutes) of continuous AEC use. The upstream bug is a floating-point rounding issue in the partition-count interpolation that can produce a smaller `current_size_partitions` than `old_size` on a size-shrink step.

## Known Issues

*(none)*

## Robustness

- **AUDCLNT_BUFFERFLAGS_SILENT**: When WASAPI marks a capture buffer as silent (flag `0x2`), the buffer contents are undefined. Both `capture.rs` and `loopback.rs` push clean zeros instead.
- **Zero-filled reusable buffers**: The PCM scratch buffers in `audio/pcm.rs` are fully zeroed before silent packets or short reads are reused, so stale samples cannot leak back into capture/render output.
- **Gap-free render output**: The render thread always writes a full WASAPI buffer, zero-padding any shortfall from the ring buffer. While paused it releases the buffer with `AUDCLNT_BUFFERFLAGS_SILENT` instead of manually clearing memory, which keeps CPU usage low without risking stale audio.
