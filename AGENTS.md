# Rust AEC — Agent Guidelines

## Project Overview

Real-time Acoustic Echo Cancellation (AEC) for Windows. Captures microphone + speaker loopback via WASAPI, runs WebRTC AEC3 (sonora), and outputs clean audio to a virtual audio cable so other apps (Discord, Zoom, Teams, etc.) can use it as their microphone input.

## Architecture

```
Physical Mic ──► [capture thread] ──► mic_ring ──┐
                                                  ├──► [main thread: AEC] ──► out_ring ──► [render thread] ──► Virtual Cable
Speaker Output ──► [loopback thread] ──► ref_ring ┘
```

- **3 threads + main**: mic-capture, loopback-capture, render (each init COM separately). Main thread runs AEC loop.
- **Inter-thread comms**: lock-free SPSC ring buffers (`ringbuf` crate), 200ms capacity.
- **Processing**: 10ms frames (480 samples @ 48kHz). AEC via `sonora` (pure-Rust WebRTC AEC3 port).
- **Audio API**: WASAPI directly via the `windows` crate (v0.58). Windows-only.

## Key Files

| File | Purpose |
|---|---|
| `src/main.rs` | CLI arg parsing, device selection, thread spawn, AEC processing loop |
| `src/audio/device.rs` | WASAPI device enumeration (`IMMDeviceEnumerator`), substring matching |
| `src/audio/capture.rs` | Mic capture thread (shared mode, event-driven, 10ms buffer) |
| `src/audio/loopback.rs` | Speaker loopback capture (`AUDCLNT_STREAMFLAGS_LOOPBACK`) |
| `src/audio/render.rs` | Writes clean audio to virtual cable render endpoint |
| `src/aec/mod.rs` | `AecProcessor` wrapping `sonora::aec3` |
| `src/sync/mod.rs` | `AudioRingBuf` — SPSC ring buffer wrapper |

## CLI Usage

```
rust_aec.exe [mic_name] [speaker_name] [output_name]
```

All arguments are optional substring matches (case-insensitive):
- **mic_name**: Microphone device. Default: Windows default capture device.
- **speaker_name**: Speaker for loopback. Default: Windows default render device.
- **output_name**: Virtual cable output. Default: auto-detects device containing "cable".

## Virtual Audio Cable Setup (Required)

The program outputs clean audio to a virtual audio cable. **You must install one** for other apps to receive the cleaned microphone signal.

### Step 1: Install a Virtual Audio Cable

Install **one** of these:
- [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) (free) — creates "CABLE Input" (render) and "CABLE Output" (capture) devices.
- [VB-Audio VoiceMeeter](https://vb-audio.com/Voicemeeter/) — more advanced routing, also free.

### Step 2: Run the AEC program

```
rust_aec.exe
```

It auto-detects devices named "cable". Or specify explicitly:

```
rust_aec.exe "Realtek" "Speakers" "CABLE Input"
```

### Step 3: Select the virtual cable as microphone in your app

In Discord / Zoom / Teams / OBS / any app:
1. Open audio/voice settings.
2. Change **Input Device** (microphone) to **"CABLE Output"** (or equivalent name from your virtual cable software).
3. That's it — the app now receives echo-cancelled audio.

### How it works

```
Your Mic ──► rust_aec ──► "CABLE Input" (virtual speaker/render side)
                                │
                                ▼
                          "CABLE Output" (virtual mic/capture side)
                                │
                                ▼
                     Discord / Zoom / Teams picks this as mic
```

The virtual cable has two sides:
- **Render side** ("CABLE Input"): rust_aec writes clean audio here.
- **Capture side** ("CABLE Output"): Other apps read from here as a microphone.

## Development Notes

- All audio conversion handles both f32 and i16 PCM formats, with mono mixdown and naive linear resampling when device sample rate != 48kHz.
- `crossbeam-channel` is declared in Cargo.toml but unused — can be removed.
- The `sonora` crate is the actual AEC engine (pure Rust), not `webrtc-audio-processing` as originally planned.
- Loopback capture uses WASAPI's built-in loopback mode — no extra virtual device needed for capturing speaker output.
