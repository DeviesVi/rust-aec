#![windows_subsystem = "windows"]

// Rust AEC Virtual Microphone
//
// Captures microphone audio (WASAPI), captures speaker loopback (WASAPI),
// runs echo cancellation (sonora AEC3), and outputs clean audio to a
// virtual audio cable device.
//
// Runs as a system tray application. Pass --verbose for console diagnostics.

mod aec;
mod audio;
mod autostart;
mod config;
mod engine;
mod sync;
mod tray;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};

use crate::audio::device;
use crate::engine::{AudioEngine, EngineCommand};
use crate::tray::TrayState;

fn main() {
    let verbose = std::env::args().any(|a| a == "--verbose" || a == "-v");

    match run(verbose) {
        Ok(()) => {}
        Err(e) => {
            let msg = format!("rust_aec error:\n{:#}", e);
            if verbose {
                eprintln!("{}", msg);
            } else {
                // Show a message box since the console may be gone.
                show_error_box(&msg);
            }
            std::process::exit(1);
        }
    }
}

fn show_error_box(msg: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide_msg: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let wide_title: Vec<u16> = "Rust AEC".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide_msg.as_ptr()),
            PCWSTR(wide_title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn run(verbose: bool) -> Result<()> {
    unsafe {
        if verbose {
            // Allocate a dedicated console window for diagnostics.
            // Closing this window (or pressing Ctrl+C in it) exits the app.
            let _ = windows::Win32::System::Console::AllocConsole();
        } else {
            // Free any console inherited from a parent shell.
            let _ = windows::Win32::System::Console::FreeConsole();
        }
    }

    // Use STA COM on main thread (required for Win32 message pump / shell).
    // Audio threads will init their own MTA COM.
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        )
        .ok()
        .context("CoInitializeEx (STA)")?;
    }

    // --- Device enumeration ---
    let capture_devices = device::list_capture_devices()?;
    let render_devices = device::list_render_devices()?;

    if verbose {
        println!("=== Capture devices (microphones) ===");
        for d in &capture_devices {
            println!("  [{}] {}", d.index, d.name);
        }
        println!("=== Render devices (speakers / virtual cables) ===");
        for d in &render_devices {
            println!("  [{}] {}", d.index, d.name);
        }
        println!();
    }

    // --- CLI device selection ---
    let args: Vec<String> = std::env::args().collect();
    // Skip flags like --verbose when looking for positional args.
    let positional: Vec<&str> = args[1..]
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();

    let mic_query = positional.first().copied();
    let speaker_query = positional.get(1).copied();
    let output_query = positional.get(2).copied();

    // Load persisted device choices (priority: CLI arg > saved config > auto-detect).
    let cfg = config::load();

    // Select mic: CLI arg > saved config ID > default (if not a cable) > first real mic.
    // Returns None when no mic is available (e.g. Remote Desktop with no audio).
    let mic_id: Option<String> = if let Some(q) = mic_query {
        Some(
            device::find_device_id_by_name(&capture_devices, q)
                .context("Mic device not found")?,
        )
    } else if let Some(id) = cfg.mic {
        if capture_devices.iter().any(|d| d.id == id) {
            if verbose {
                println!("Mic: loaded from config ({})", device::device_name_by_id(&capture_devices, &id));
            }
            Some(id)
        } else {
            // Saved device no longer present — fall through to auto-detect.
            match device::default_capture_device_id() {
                Ok(default_id) => {
                    let default_name = device::device_name_by_id(&capture_devices, &default_id);
                    if device::is_virtual_cable(&default_name) {
                        device::find_real_capture_device(&capture_devices).ok()
                    } else {
                        Some(default_id)
                    }
                }
                Err(_) => None,
            }
        }
    } else {
        match device::default_capture_device_id() {
            Ok(default_id) => {
                let default_name = device::device_name_by_id(&capture_devices, &default_id);
                if device::is_virtual_cable(&default_name) {
                    if verbose {
                        println!(
                            "Default capture device '{}' is a virtual cable, looking for a real mic...",
                            default_name
                        );
                    }
                    device::find_real_capture_device(&capture_devices).ok()
                } else {
                    Some(default_id)
                }
            }
            Err(_) => {
                if verbose {
                    println!("No capture device found, starting without microphone.");
                }
                None
            }
        }
    };

    // Select speaker for loopback: CLI arg > saved config ID > default render device.
    let speaker_id: Option<String> = if let Some(q) = speaker_query {
        Some(
            device::find_device_id_by_name(&render_devices, q)
                .context("Speaker device not found")?,
        )
    } else if let Some(id) = cfg.speaker {
        if render_devices.iter().any(|d| d.id == id) {
            if verbose {
                println!("Speaker: loaded from config ({})", device::device_name_by_id(&render_devices, &id));
            }
            Some(id)
        } else {
            device::default_render_device_id().ok()
        }
    } else {
        match device::default_render_device_id() {
            Ok(id) => Some(id),
            Err(_) => {
                if verbose {
                    println!("No render device found, starting without speaker.");
                }
                None
            }
        }
    };

    // Select output virtual cable: CLI arg > saved config ID > device with "cable input" in name.
    let output_id: Option<String> = if let Some(q) = output_query {
        Some(
            device::find_device_id_by_name(&render_devices, q)
                .context("Output virtual cable device not found")?,
        )
    } else if let Some(id) = cfg.output {
        if render_devices.iter().any(|d| d.id == id) {
            if verbose {
                println!("Output: loaded from config ({})", device::device_name_by_id(&render_devices, &id));
            }
            Some(id)
        } else {
            device::find_device_id_by_name(&render_devices, "cable input").ok()
        }
    } else {
        match device::find_device_id_by_name(&render_devices, "cable input") {
            Ok(id) => Some(id),
            Err(_) => {
                if verbose {
                    println!("No virtual audio cable found, starting without output.");
                }
                None
            }
        }
    };

    if verbose {
        match &mic_id {
            Some(id) => println!(
                "Mic: {}",
                device::device_name_by_id(&capture_devices, id)
            ),
            None => println!("Mic: (none)"),
        }
        match &speaker_id {
            Some(id) => println!(
                "Speaker: {}",
                device::device_name_by_id(&render_devices, id)
            ),
            None => println!("Speaker: (none)"),
        }
        match &output_id {
            Some(id) => println!(
                "Output: {}",
                device::device_name_by_id(&render_devices, id)
            ),
            None => println!("Output: (none)"),
        }
    }

    // --- Shared state and command channel ---
    let state = Arc::new(Mutex::new(TrayState {
        capture_devices,
        render_devices,
        current_mic_id: mic_id,
        current_speaker_id: speaker_id,
        current_output_id: output_id,
    }));

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EngineCommand>();

    // --- Spawn engine thread ---
    let engine_state = state.clone();
    let engine_thread = thread::Builder::new()
        .name("aec-engine".into())
        .spawn(move || {
            let engine = AudioEngine {
                cmd_rx,
                state: engine_state,
                verbose,
            };
            if let Err(e) = engine.run() {
                eprintln!("[error] engine: {:#}", e);
            }
        })?;

    // --- Run system tray on main thread (message pump) ---
    tray::run_tray(state, cmd_tx)?;

    // Tray message pump exited — wait for engine.
    engine_thread.join().ok();

    Ok(())
}
