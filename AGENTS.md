# Rust AEC — Agent Guidelines

## Project Overview

Real-time Acoustic Echo Cancellation (AEC) for Windows. Captures microphone + speaker loopback via WASAPI, runs WebRTC AEC3 (sonora), and outputs clean audio to a virtual audio cable so other apps (Discord, Zoom, Teams, etc.) can use it as their microphone input. Runs as a system tray application.

## Architecture

```
Main thread:       Win32 message pump + system tray icon (src/tray.rs)
Engine thread:     AEC processing loop (src/engine.rs)
  mic-capture:     WASAPI capture → mic_ring (src/audio/capture.rs)
  loopback:        WASAPI loopback → ref_ring (src/audio/loopback.rs)
  render:          out_ring → Virtual Cable (src/audio/render.rs)
```

- **Main thread**: Runs Win32 message pump for the system tray icon. Sends `EngineCommand` to the engine thread via `crossbeam-channel`.
- **Engine thread**: Owns the AEC processor + 3 audio threads + ring buffers. Handles device hot-swap by rebuilding the full pipeline.
- **Inter-thread comms**: Lock-free SPSC ring buffers (`ringbuf` crate), 500ms capacity. Commands via `crossbeam-channel`.
- **Processing**: 10ms frames (480 samples @ 48kHz). AEC via `sonora` (pure-Rust WebRTC AEC3 port).
- **Audio API**: WASAPI directly via the `windows` crate (v0.58). Windows-only.

## Key Files

| File | Purpose |
|---|---|
| `src/main.rs` | CLI parsing, device selection (with cable filtering), tray + engine startup |
| `src/engine.rs` | `AudioEngine` — AEC processing loop, audio thread lifecycle, `EngineCommand` handling |
| `src/tray.rs` | Win32 system tray icon, context menus, `TrayState` shared with engine |
| `src/autostart.rs` | Registry-based Windows autostart (`HKCU\...\Run`) |
| `src/audio/device.rs` | WASAPI device enumeration, substring matching, virtual cable detection |
| `src/audio/capture.rs` | Mic capture thread (shared mode, event-driven, 10ms buffer) |
| `src/audio/loopback.rs` | Speaker loopback capture (`AUDCLNT_STREAMFLAGS_LOOPBACK`) |
| `src/audio/render.rs` | Writes clean audio to virtual cable render endpoint |
| `src/aec/mod.rs` | `AecProcessor` wrapping `sonora::AudioProcessing` |
| `src/sync/mod.rs` | `AudioRingBuf` — SPSC ring buffer wrapper |
| `build.rs` | Embeds `resources/app.ico` via `embed-resource` |

## CLI Usage

```
rust_aec.exe [--verbose] [mic_name] [speaker_name] [output_name]
```

- `--verbose` / `-v`: Show console with diagnostics. Without this, the console is hidden (FreeConsole).
- All positional arguments are optional substring matches (case-insensitive).
- **mic_name**: Microphone device. Default: first real (non-virtual-cable) capture device.
- **speaker_name**: Speaker for loopback. Default: Windows default render device.
- **output_name**: Virtual cable output. Default: auto-detects device containing "cable".

## Virtual Audio Cable Setup (Required)

Install [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) (free). It creates "CABLE Input" (render) and "CABLE Output" (capture) devices.

## Key Design Decisions

- **No tray crate**: Uses Win32 API directly (Shell_NotifyIconW, CreatePopupMenu, etc.) via the `windows` crate to avoid extra dependencies.
- **Cable filtering**: When auto-selecting a microphone, devices with "cable" in the name are skipped to avoid selecting a virtual cable as input.
- **Device hot-swap**: When the user changes a device via the tray menu, the entire audio pipeline is torn down and rebuilt. The AEC re-adapts in ~1-2 seconds.
- **Shared state**: `TrayState` (device lists + current selections) is protected by `Arc<Mutex<>>`, accessed by both the tray (for menu building) and the engine (for device IDs).

## Development Notes

- All audio conversion handles both f32 and i16 PCM formats, with mono mixdown and naive linear resampling when device sample rate != 48kHz.
- The `sonora` crate is the AEC engine (pure Rust WebRTC AEC3 port).
- Loopback capture uses WASAPI's built-in loopback mode — no extra virtual device needed for capturing speaker output.

## Robustness / Glitch Prevention

These defenses keep the audio pipeline stable over long sessions and across different apps (QQ, Discord, Zoom, etc.):

- **AUDCLNT_BUFFERFLAGS_SILENT handling**: When WASAPI marks a capture buffer as silent, the buffer contents are *undefined*. Both `capture.rs` and `loopback.rs` detect flag `0x2` and push clean zeros instead. Without this, garbage reference data causes the AEC to diverge and suppress real voice.
- **Gap-free render output**: The render thread always writes a full WASAPI buffer, zero-padding any shortfall from the ring buffer. Prevents audio discontinuities that voice chat apps (QQ, etc.) interpret as stream end.
- **Reference clock drift drain**: Mic and speaker devices may run on different hardware clocks (up to ~0.1% drift). The engine drains excess reference data when the ref ring buffer exceeds 4 frames (~40ms), keeping the AEC delay bounded so echo cancellation stays aligned.
- **NaN/Inf sanitization**: Both capture threads replace any non-finite samples (from buggy audio drivers) with 0.0 before pushing to ring buffers. Prevents permanent AEC divergence.
- **Ring buffer capacity (500ms)**: Provides headroom for OS scheduling jitter and burst processing without overflow.
- **Output clamping**: f32 render output is clamped to [-1.0, 1.0] to prevent out-of-range values from reaching the virtual cable consumer.
- **Pre-allocated AEC render buffer**: The `AecProcessor` reuses a fixed buffer for `process_render_f32` to avoid per-frame heap allocation.
