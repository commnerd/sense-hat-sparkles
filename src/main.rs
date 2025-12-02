use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

// LED matrix is 8x8, each pixel is 16 bits (RGB565)
const WIDTH: usize = 8;
const HEIGHT: usize = 8;

// Decibel thresholds - adjusted for real-world microphone input
// Typical microphone input ranges from -90dB (silence) to -20dB (very loud)
const MIN_DB: f32 = -80.0; // Minimum threshold (silence) - below this, lights off
const MAX_DB: f32 = 0.0;   // Maximum threshold (loud)
const MID_LOW: f32 = -60.0;  // Start transitioning to yellow (quiet sound)
const MID_HIGH: f32 = -40.0; // Start transitioning to red (moderate sound)
const FLASH_THRESHOLD: f32 = -20.0; // Threshold for flashing sequence (loud sound)

// Flashing sequence duration
const FLASH_DURATION: Duration = Duration::from_secs(5);

// Smoothing factor for color transitions (0.0 = no smoothing, 1.0 = no change)
// Lower values = faster transitions, higher values = slower/smoother transitions
const COLOR_SMOOTHING: f32 = 0.85;

fn clear_fb(fb: &mut File) -> std::io::Result<()> {
    // Turn all pixels off (black)
    let black_pixel: [u8; 2] = 0u16.to_le_bytes();
    fb.seek(SeekFrom::Start(0))?;

    for _ in 0..(WIDTH * HEIGHT) {
        fb.write_all(&black_pixel)?;
    }

    Ok(())
}

fn fill_fb(fb: &mut File, color: (u8, u8, u8)) -> std::io::Result<()> {
    // Convert RGB888 -> RGB565 (Sense HAT format)
    let r5 = (color.0 >> 3) as u16;
    let g6 = (color.1 >> 2) as u16;
    let b5 = (color.2 >> 3) as u16;
    let pixel: u16 = (r5 << 11) | (g6 << 5) | b5;
    let pixel_bytes = pixel.to_le_bytes();

    fb.seek(SeekFrom::Start(0))?;
    for _ in 0..(WIDTH * HEIGHT) {
        fb.write_all(&pixel_bytes)?;
    }
    fb.sync_all()?; // Ensure writes are flushed to device

    Ok(())
}

fn calculate_decibel(rms: f32) -> f32 {
    if rms <= 0.0 {
        return MIN_DB;
    }
    // Convert RMS to decibels (relative to full scale)
    // Using 20 * log10(rms) where rms is normalized to 0-1
    // Add a small epsilon to avoid log(0)
    let db = 20.0 * (rms.max(1e-10)).log10();
    db.max(MIN_DB).min(MAX_DB)
}

fn db_to_color(db: f32) -> (u8, u8, u8) {
    if db < MIN_DB {
        // No sound - black
        return (0, 0, 0);
    }

    if db < MID_LOW {
        // Low sound - fade in green
        let ratio = (db - MIN_DB) / (MID_LOW - MIN_DB);
        let green = (ratio * 255.0) as u8;
        return (0, green, 0);
    }

    if db < MID_HIGH {
        // Mid sound - transition from green to yellow
        let ratio = (db - MID_LOW) / (MID_HIGH - MID_LOW);
        let green = 255;
        let red = (ratio * 255.0) as u8;
        return (red, green, 0);
    }

    if db < FLASH_THRESHOLD {
        // High sound - transition from yellow to red
        let ratio = (db - MID_HIGH) / (FLASH_THRESHOLD - MID_HIGH);
        let red = 255;
        let green = ((1.0 - ratio) * 255.0) as u8;
        return (red, green, 0);
    }

    // At or above flash threshold - return red (flashing will be handled separately)
    (255, 0, 0)
}

fn flashing_sequence(fb: &mut File, running: &Arc<AtomicBool>) -> std::io::Result<()> {
    let start_time = Instant::now();
    let mut is_white = false;

    while running.load(Ordering::SeqCst) && start_time.elapsed() < FLASH_DURATION {
        if is_white {
            fill_fb(fb, (255, 255, 255))?; // White
        } else {
            fill_fb(fb, (255, 0, 0))?; // Red
        }
        is_white = !is_white;
        thread::sleep(Duration::from_millis(100)); // Flash every 100ms
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Sense HAT framebuffer is almost always /dev/fb1
    let mut fb = OpenOptions::new()
        .write(true)
        .read(true)
        .open("/dev/fb1")
        .expect("Could not open /dev/fb1. Are you running on a Sense HAT?");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Set up Ctrl+C handler
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    // Get default input device
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("No input device available")?;

    let config = device.default_input_config()?;
    println!("Default input config: {:?}", config);

    // Shared state for audio level (using AtomicU32 to store f32 bits)
    let current_db = Arc::new(AtomicU32::new(MIN_DB.to_bits()));
    let db_handle = current_db.clone();

    // Build the stream - must keep it alive!
    let _stream = match config.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if data.is_empty() {
                        return;
                    }
                    // Calculate RMS - F32 samples are already normalized to -1.0 to 1.0
                    let sum_squares: f32 = data.iter().map(|&sample| sample * sample).sum();
                    let rms = (sum_squares / data.len() as f32).sqrt();
                    let db = calculate_decibel(rms);
                    db_handle.store(db.to_bits(), Ordering::SeqCst);
                },
                |err| eprintln!("Error in audio stream: {}", err),
                None,
            )?
        }
        SampleFormat::I16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if data.is_empty() {
                        return;
                    }
                    // Convert i16 to f32 and calculate RMS
                    let sum_squares: f32 = data
                        .iter()
                        .map(|&sample| {
                            let normalized = sample as f32 / 32768.0;
                            normalized * normalized
                        })
                        .sum();
                    let rms = (sum_squares / data.len() as f32).sqrt();
                    let db = calculate_decibel(rms);
                    db_handle.store(db.to_bits(), Ordering::SeqCst);
                },
                |err| eprintln!("Error in audio stream: {}", err),
                None,
            )?
        }
        SampleFormat::U16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    if data.is_empty() {
                        return;
                    }
                    // Convert u16 to f32 and calculate RMS
                    let sum_squares: f32 = data
                        .iter()
                        .map(|&sample| {
                            let normalized = (sample as f32 - 32768.0) / 32768.0;
                            normalized * normalized
                        })
                        .sum();
                    let rms = (sum_squares / data.len() as f32).sqrt();
                    let db = calculate_decibel(rms);
                    db_handle.store(db.to_bits(), Ordering::SeqCst);
                },
                |err| eprintln!("Error in audio stream: {}", err),
                None,
            )?
        }
        _ => return Err("Unsupported sample format".into()),
    };

    _stream.play()?;
    println!("Audio stream started. Listening to microphone...");

    // Main visualization loop with smooth color transitions
    let mut last_flash_time = Instant::now();
    let flash_cooldown = Duration::from_secs(1); // Cooldown after flashing
    let mut last_debug_time = Instant::now();
    
    // Current displayed color (for smoothing)
    let mut current_color = (0u8, 0u8, 0u8);

    while running.load(Ordering::SeqCst) {
        let db = f32::from_bits(current_db.load(Ordering::SeqCst));

        // Debug output every second
        if last_debug_time.elapsed() >= Duration::from_secs(1) {
            println!("Current dB: {:.2}, RMS would be: {:.6}", db, 10.0_f32.powf(db / 20.0));
            last_debug_time = Instant::now();
        }

        // Check if we should enter flash mode (with cooldown to prevent rapid re-triggering)
        if db >= FLASH_THRESHOLD && last_flash_time.elapsed() >= flash_cooldown {
            println!("Flash threshold reached! dB: {:.2}", db);
            flashing_sequence(&mut fb, &running)?;
            last_flash_time = Instant::now(); // Update after flashing completes
            // Reset current color after flashing
            current_color = (255, 0, 0);
        } else {
            // Normal heat map mode with smooth transitions
            let target_color = db_to_color(db);
            
            // Smooth transition from current_color to target_color
            current_color = (
                ((current_color.0 as f32 * COLOR_SMOOTHING) + (target_color.0 as f32 * (1.0 - COLOR_SMOOTHING))) as u8,
                ((current_color.1 as f32 * COLOR_SMOOTHING) + (target_color.1 as f32 * (1.0 - COLOR_SMOOTHING))) as u8,
                ((current_color.2 as f32 * COLOR_SMOOTHING) + (target_color.2 as f32 * (1.0 - COLOR_SMOOTHING))) as u8,
            );
            
            fill_fb(&mut fb, current_color)?;
        }

        // Small delay to avoid excessive writes
        thread::sleep(Duration::from_millis(50));
    }

    // Ctrl+C pressed: clear the display before exiting
    clear_fb(&mut fb)?;

    Ok(())
}
