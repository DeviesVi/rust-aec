# rust_aec

Real-time Acoustic Echo Cancellation for Windows. Removes speaker echo from your microphone and outputs clean audio to a virtual microphone that other apps (Discord, Zoom, Teams, OBS) can use.

## How It Works

```
Physical Mic ──► [AEC Engine] ──► CABLE Input (virtual speaker)
Speaker Output ──► [reference] ──┘        │
                                          ▼
                                   CABLE Output  ◄── Discord / Zoom / Teams / OBS
                                   (virtual mic)
```

1. Captures your microphone and speaker output simultaneously.
2. Runs WebRTC AEC3 echo cancellation to remove the speaker signal from the mic.
3. Writes the clean audio to a virtual audio cable.
4. Other apps select the virtual cable's output as their microphone input.

## Requirements

- Windows 10/11
- Rust toolchain (`cargo`)
- A virtual audio cable driver (see [Setup](#setup))

## Setup

### 1. Install a Virtual Audio Cable

Install [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) (free).

It creates two Windows audio devices:
- **CABLE Input** — a virtual speaker (rust_aec writes clean audio here)
- **CABLE Output** — a virtual microphone (your apps read from here)

### 2. Build

```sh
cargo build --release
```

### 3. Run

```sh
cargo run --release
```

The program lists all detected audio devices, then starts processing. It auto-detects the VB-Audio cable by name.

### 4. Set the Virtual Mic in Your App

In Discord, Zoom, Teams, OBS, or any app:

- Go to **audio / voice settings**
- Set **Input Device** (microphone) to **CABLE Output**

Your app now receives echo-cancelled audio.

## Usage

```
rust_aec.exe [mic_name] [speaker_name] [output_name]
```

All arguments are optional. Each is a case-insensitive substring matched against the device's friendly name.

| Argument | Default | Example |
|---|---|---|
| `mic_name` | Windows default mic | `"Realtek"` |
| `speaker_name` | Windows default speakers | `"Speakers"` |
| `output_name` | Auto-detect device with "cable" in name | `"CABLE Input"` |

**Examples:**

```sh
# Use all defaults (auto-detect cable)
rust_aec.exe

# Specify microphone only
rust_aec.exe "Realtek Microphone"

# Specify all three devices
rust_aec.exe "Realtek" "Speakers (Realtek)" "CABLE Input"
```

Press `Ctrl+C` to stop.

## Project Structure

```
src/
  main.rs              # Device selection, thread orchestration, AEC loop
  audio/
    device.rs          # WASAPI device enumeration and selection
    capture.rs         # Microphone capture thread
    loopback.rs        # Speaker loopback capture thread
    render.rs          # Clean audio output thread (writes to virtual cable)
  aec/
    mod.rs             # AEC processor (sonora WebRTC AEC3)
  sync/
    mod.rs             # Lock-free ring buffers for inter-thread audio
```

## Technical Details

| Property | Value |
|---|---|
| Audio API | WASAPI (Windows Audio Session API) |
| AEC Engine | [sonora](https://crates.io/crates/sonora) — pure Rust WebRTC AEC3 |
| Sample rate | 48 kHz |
| Frame size | 480 samples (10ms) |
| Channels | Mono (downmixed internally) |
| Format support | f32 and i16 PCM |
| Buffer size | 200ms ring buffers |

Audio flows through three dedicated threads (mic capture, loopback capture, render) communicating via lock-free SPSC ring buffers. The main thread runs the AEC processing loop.

## Troubleshooting

**"No virtual audio cable found"**
- Install [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) and restart the program.
- Or pass the output device name explicitly: `rust_aec.exe "" "" "My Cable Device"`.

**No echo cancellation effect**
- Make sure the correct speaker device is selected for loopback (the one actually playing audio).
- Run with explicit device names to verify the right devices are selected.

**Other app still hears echo**
- Confirm the app's input device is set to **CABLE Output**, not your physical microphone.
