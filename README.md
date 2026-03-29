# rust_aec

Real-time Acoustic Echo Cancellation for Windows. Removes speaker echo from your microphone and outputs clean audio to a virtual microphone that other apps (Discord, Zoom, Teams, OBS) can use.

Runs as a **system tray application** with a notification area icon for device selection.

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
# Normal mode — runs as a tray icon, no console output
cargo run --release

# Verbose mode — shows console with diagnostic output
cargo run --release -- --verbose
```

The program auto-detects the VB-Audio cable, selects a real microphone (skipping virtual cables), and starts processing. A system tray icon appears in the notification area.

### 4. Set the Virtual Mic in Your App

In Discord, Zoom, Teams, OBS, or any app:

- Go to **audio / voice settings**
- Set **Input Device** (microphone) to **CABLE Output**

Your app now receives echo-cancelled audio.

## Usage

```
rust_aec.exe [--verbose] [mic_name] [speaker_name] [output_name]
```

### Flags

| Flag | Description |
|---|---|
| `--verbose`, `-v` | Show console window with diagnostic output (device lists, buffer levels, peak levels every 2s) |

### Positional Arguments

All positional arguments are optional. Each is a case-insensitive substring matched against the device's friendly name.

| Argument | Default | Example |
|---|---|---|
| `mic_name` | First real (non-cable) microphone | `"Realtek"` |
| `speaker_name` | Windows default speakers | `"Speakers"` |
| `output_name` | Auto-detect device with "cable" in name | `"CABLE Input"` |

**Examples:**

```sh
# Use all defaults (auto-detect everything, tray icon only)
rust_aec.exe

# Verbose mode with all defaults
rust_aec.exe --verbose

# Specify microphone only
rust_aec.exe "Realtek Microphone"

# Specify all three devices with verbose output
rust_aec.exe --verbose "Realtek" "Speakers (Realtek)" "CABLE Input"
```

## System Tray

The application runs in the Windows notification area (system tray). Right-click the tray icon to:

- **Microphone** — Select which capture device to use (radio-button selection)
- **Speaker (Loopback)** — Select which render device to capture system audio from
- **Start with Windows** — Toggle automatic startup (adds/removes a registry entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`)
- **Exit** — Stop the AEC engine and quit

Device changes take effect immediately — the audio pipeline restarts with the new device.

## Project Structure

```
src/
  main.rs              # CLI parsing, device selection, tray + engine startup
  engine.rs            # AEC processing loop + audio thread management
  tray.rs              # Win32 system tray icon and context menus
  autostart.rs         # Windows registry autostart (HKCU Run key)
  audio/
    device.rs          # WASAPI device enumeration, cable filtering
    capture.rs         # Microphone capture thread
    loopback.rs        # Speaker loopback capture thread
    render.rs          # Clean audio output thread (writes to virtual cable)
  aec/
    mod.rs             # AEC processor (sonora WebRTC AEC3)
  sync/
    mod.rs             # Lock-free ring buffers for inter-thread audio
build.rs               # Embeds app.ico via Windows resource compiler
resources/
  app.ico              # Application icon
  app.rc               # Windows resource script
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

### Architecture

```
Main thread:       Win32 message pump + system tray icon
Engine thread:     AEC processing loop (reads mic + reference, writes output)
  mic-capture:     WASAPI capture → mic ring buffer
  loopback:        WASAPI loopback → ref ring buffer
  render:          out ring buffer → WASAPI virtual cable
```

Commands flow from the tray to the engine via a crossbeam channel. Device changes trigger a full pipeline restart (stop threads, rebuild ring buffers, respawn).

## Troubleshooting

**"No virtual audio cable found"**
- Install [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) and restart the program.
- Or pass the output device name explicitly: `rust_aec.exe "" "" "My Cable Device"`.

**No echo cancellation effect**
- Make sure the correct speaker device is selected for loopback (the one actually playing audio).
- Right-click the tray icon and verify the correct speaker is selected.

**Other app still hears echo**
- Confirm the app's input device is set to **CABLE Output**, not your physical microphone.

**Tray icon not visible**
- Check the Windows notification area overflow (click the ^ arrow in the taskbar).
- Run with `--verbose` to see console output and verify the program is running.
