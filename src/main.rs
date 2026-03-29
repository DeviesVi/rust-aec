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
mod engine;
mod sync;
mod tray;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};

use crate::audio::device;
use crate::engine::{AudioEngine, EngineCommand};
use crate::tray::TrayState;

fn main() -> Result<()> {
    let verbose = std::env::args().any(|a| a == "--verbose" || a == "-v");

    // Hide the console window unless --verbose is passed.
    if !verbose {
        unsafe {
            let _ = windows::Win32::System::Console::FreeConsole();
        }
    }

    device::com_init()?;

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

    // Select mic: user arg > default (if not a cable) > first real mic.
    let mic_id = if let Some(q) = mic_query {
        device::find_device_id_by_name(&capture_devices, q)
            .context("Mic device not found")?
    } else {
        let default_id = device::default_capture_device_id()?;
        let default_name = device::device_name_by_id(&capture_devices, &default_id);
        if device::is_virtual_cable(&default_name) {
            if verbose {
                println!(
                    "Default capture device '{}' is a virtual cable, looking for a real mic...",
                    default_name
                );
            }
            device::find_real_capture_device(&capture_devices)?
        } else {
            default_id
        }
    };

    // Select speaker for loopback.
    let speaker_id = if let Some(q) = speaker_query {
        device::find_device_id_by_name(&render_devices, q)
            .context("Speaker device not found")?
    } else {
        device::default_render_device_id()?
    };

    // Select output virtual cable.
    let output_id = if let Some(q) = output_query {
        device::find_device_id_by_name(&render_devices, q)
            .context("Output virtual cable device not found")?
    } else {
        match device::find_device_id_by_name(&render_devices, "cable") {
            Ok(id) => id,
            Err(_) => {
                bail!(
                    "No virtual audio cable found. Install VB-Audio Virtual Cable \
                     or pass the output device name as an argument."
                );
            }
        }
    };

    if verbose {
        println!(
            "Mic: {}",
            device::device_name_by_id(&capture_devices, &mic_id)
        );
        println!(
            "Speaker: {}",
            device::device_name_by_id(&render_devices, &speaker_id)
        );
        println!(
            "Output: {}",
            device::device_name_by_id(&render_devices, &output_id)
        );
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

    // --- Ctrl+C handler ---
    {
        let cmd_tx = cmd_tx.clone();
        ctrlc::set_handler(move || {
            let _ = cmd_tx.send(EngineCommand::Shutdown);
        })
        .ok();
    }

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
