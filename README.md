# rust_aec

<img src="resources/icon.svg" width="80" align="right" alt="rust_aec icon">

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
3. Writes the clean audio to the CABLE Input virtual device.
4. Other apps select CABLE Output as their microphone input.

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
# Normal mode — runs as a tray icon, no console window
cargo run --release

# Verbose mode — prints diagnostics to the terminal you run it from
cargo run --release -- --verbose
```

The program starts immediately even if no devices are detected (e.g. when launched at startup before audio drivers are ready, or via Remote Desktop). It waits silently and starts the AEC pipeline automatically when all required devices become available — on session unlock or physical console login.

Device selections made via the tray menu are saved to `rust_aec.cfg` (next to the executable) and restored on the next launch.

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
| `--verbose`, `-v` | Attach to the parent terminal for diagnostic output (device lists, buffer levels, peak levels every 2s). Falls back to a new console window if not run from a terminal. |

### Positional Arguments

All positional arguments are optional. Each is a case-insensitive substring matched against the device's friendly name. If an argument is omitted, the last saved choice from `rust_aec.cfg` is used (if the device is still present), then auto-detection, then wait.

| Argument | Default | Example |
|---|---|---|
| `mic_name` | First real (non-cable) microphone | `"Realtek"` |
| `speaker_name` | Windows default speakers | `"Speakers"` |
| `output_name` | Device with "cable input" in name | `"CABLE Input"` |

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

- **Microphone** — Select which capture device to use (radio-button selection; shows "No devices found" if none are active)
- **Speaker (Loopback)** — Select which render device to capture system audio from
- **Output (Cable)** — Select which render device to write clean audio to (typically CABLE Input)
- **Start with Windows** — Toggle automatic startup (adds/removes a registry entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`)
- **Exit** — Stop the AEC engine and quit

Device changes take effect immediately — the audio pipeline restarts with the new device. Device lists are refreshed each time the menu is opened. **Selections are saved to `rust_aec.cfg`** next to the executable and restored on the next launch.

### Remote Desktop & Startup Behaviour

- When launched with no audio devices available (e.g. Remote Desktop, early autostart), the app starts silently with a tray icon and waits.
- On **session unlock** or **physical console login**, it re-enumerates devices and starts the pipeline automatically.
- No error dialogs or crashes — the app is always running and ready.

## Project Structure

```
src/
  main.rs              # CLI parsing, device selection, tray + engine startup
  engine.rs            # AEC processing loop + audio thread management
  tray.rs              # Win32 system tray icon and context menus
  config.rs            # Load/save device selections to rust_aec.cfg
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
  icon.svg             # Application icon (source SVG)
  app.ico              # Application icon (compiled from icon.svg)
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
| Rust edition | 2024 |

### Architecture

```
Main thread:       Win32 message pump + system tray icon
Engine thread:     AEC processing loop (reads mic + reference, writes output)
  mic-capture:     WASAPI capture → mic ring buffer
  loopback:        WASAPI loopback → ref ring buffer
  render:          out ring buffer → WASAPI virtual cable
```

Commands flow from the tray to the engine via a crossbeam channel (`SetMicDevice`, `SetSpeakerDevice`, `SetOutputDevice`, `RefreshDevices`, `Shutdown`). Device changes trigger a full pipeline restart. `RefreshDevices` is sent automatically on Windows session unlock and console connect events (via `WTSRegisterSessionNotification`).

## Config File

Device selections are automatically saved to `rust_aec.cfg` in the same directory as the executable whenever you change a device from the tray menu. On the next launch the saved IDs are restored (if the devices are still present), falling back to auto-detection otherwise.

```ini
mic=<WASAPI endpoint ID>
speaker=<WASAPI endpoint ID>
output=<WASAPI endpoint ID>
```

Priority order: **CLI argument > saved config > auto-detect > wait**.

You can delete `rust_aec.cfg` to reset all devices to auto-detect.

## Troubleshooting

**No tray menu submenus show a device**
- The cable or mic drivers may not have initialised yet. Wait a moment and right-click again — the list refreshes each time.
- Alternatively, lock and unlock the session to trigger an automatic device refresh.

**No echo cancellation effect**
- Make sure the correct speaker device is selected under **Speaker (Loopback)** — it must be the device actually playing audio.
- Make sure **Output (Cable)** is set to **CABLE Input**.

**Other app still hears echo**
- Confirm the app's input device is set to **CABLE Output**, not your physical microphone.

**Tray icon not visible**
- Check the Windows notification area overflow (click the ^ arrow in the taskbar).
- Run with `--verbose` to see console output and verify the program is running.

**Works via Remote Desktop but not physically (or vice versa)**
- Lock and unlock the session — this triggers a device refresh and restarts the pipeline.
