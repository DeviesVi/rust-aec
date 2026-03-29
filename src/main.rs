// Rust AEC Virtual Microphone
//
// Captures microphone audio (WASAPI), captures speaker loopback (WASAPI),
// runs echo cancellation (sonora AEC3), and outputs clean audio to a
// virtual audio cable device.

mod aec;
mod audio;
mod sync;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Context, Result};

use crate::aec::{AecProcessor, FRAME_SIZE, SAMPLE_RATE};
use crate::audio::device;
use crate::sync::AudioRingBuf;

fn main() -> Result<()> {
    device::com_init()?;

    // --- Device selection ---
    let args: Vec<String> = std::env::args().collect();
    let mic_query = args.get(1).map(String::as_str);
    let speaker_query = args.get(2).map(String::as_str);
    let output_query = args.get(3).map(String::as_str);

    // List available devices.
    let capture_devices = device::list_capture_devices()?;
    let render_devices = device::list_render_devices()?;

    println!("=== Capture devices (microphones) ===");
    for d in &capture_devices {
        println!("  [{}] {}", d.index, d.name);
    }
    println!("=== Render devices (speakers / virtual cables) ===");
    for d in &render_devices {
        println!("  [{}] {}", d.index, d.name);
    }
    println!();

    // Select mic device ID.
    let mic_id = if let Some(q) = mic_query {
        device::find_device_id_by_name(&capture_devices, q)
            .context("Mic device not found")?
    } else {
        println!("No mic specified, using default capture device.");
        device::default_capture_device_id()?
    };

    // Select speaker device ID for loopback.
    let speaker_id = if let Some(q) = speaker_query {
        device::find_device_id_by_name(&render_devices, q)
            .context("Speaker device not found")?
    } else {
        println!("No speaker specified, using default render device for loopback.");
        device::default_render_device_id()?
    };

    // Select output virtual cable device ID.
    let output_id = if let Some(q) = output_query {
        device::find_device_id_by_name(&render_devices, q)
            .context("Output virtual cable device not found")?
    } else {
        match device::find_device_id_by_name(&render_devices, "cable") {
            Ok(id) => {
                println!("Auto-detected virtual cable.");
                id
            }
            Err(_) => {
                bail!(
                    "No virtual audio cable found. Install VB-Audio Virtual Cable \
                     or pass the output device name as the 3rd argument."
                );
            }
        }
    };

    println!(
        "Starting AEC: sample_rate={}Hz, frame_size={} samples ({}ms)",
        SAMPLE_RATE,
        FRAME_SIZE,
        FRAME_SIZE * 1000 / SAMPLE_RATE
    );

    // --- Ring buffers ---
    let buf_capacity = SAMPLE_RATE / 5; // 200ms
    let mic_ring = AudioRingBuf::new(buf_capacity);
    let ref_ring = AudioRingBuf::new(buf_capacity);
    let out_ring = AudioRingBuf::new(buf_capacity);

    let (mic_prod, mic_cons) = mic_ring.split();
    let (ref_prod, ref_cons) = ref_ring.split();
    let (out_prod, out_cons) = out_ring.split();

    // --- Ctrl+C handler ---
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            println!("\nShutting down...");
            stop.store(true, Ordering::SeqCst);
        })
        .context("Failed to set Ctrl+C handler")?;
    }

    // --- Spawn threads (pass device IDs, re-open on each thread) ---
    let stop_mic = stop.clone();
    let mic_thread = thread::Builder::new()
        .name("mic-capture".into())
        .spawn(move || {
            device::com_init().expect("COM init failed in mic thread");
            audio::capture::capture_loop(&mic_id, mic_prod, stop_mic)
        })?;

    let stop_ref = stop.clone();
    let ref_thread = thread::Builder::new()
        .name("loopback-capture".into())
        .spawn(move || {
            device::com_init().expect("COM init failed in loopback thread");
            audio::loopback::loopback_loop(&speaker_id, ref_prod, stop_ref)
        })?;

    let stop_out = stop.clone();
    let out_thread = thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            device::com_init().expect("COM init failed in render thread");
            audio::render::render_loop(&output_id, out_cons, stop_out)
        })?;

    // --- AEC processing on main thread ---
    let mut processor = AecProcessor::new()?;
    let mut mic_frame = vec![0.0f32; FRAME_SIZE];
    let mut ref_frame = vec![0.0f32; FRAME_SIZE];
    let mut out_frame = vec![0.0f32; FRAME_SIZE];
    let mut mic_cons = mic_cons;
    let mut ref_cons = ref_cons;
    let mut out_prod = out_prod;

    println!("AEC running. Press Ctrl+C to stop.");

    while !stop.load(Ordering::Relaxed) {
        if mic_cons.available() < FRAME_SIZE || ref_cons.available() < FRAME_SIZE {
            thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }

        mic_cons.pop(&mut mic_frame);
        ref_cons.pop(&mut ref_frame);

        processor.process_frame(&mic_frame, &ref_frame, &mut out_frame);

        out_prod.push(&out_frame);
    }

    let _ = mic_thread.join();
    let _ = ref_thread.join();
    let _ = out_thread.join();

    println!("Done.");
    Ok(())
}
