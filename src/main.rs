use rand::Rng;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

// LED matrix is 8x8, each pixel is 16 bits (RGB565)
const WIDTH: usize = 8;
const HEIGHT: usize = 8;
const BPP: usize = 2; // bytes per pixel

fn clear_fb(fb: &mut File) -> std::io::Result<()> {
    // Turn all pixels off (black)
    let black_pixel: [u8; 2] = 0u16.to_le_bytes();
    fb.seek(SeekFrom::Start(0))?;

    for _ in 0..(WIDTH * HEIGHT) {
        fb.write_all(&black_pixel)?;
    }

    Ok(())
}

fn main() -> std::io::Result<()> {
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
        // Signal the main loop to stop
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut rng = rand::rng();

    // Main loop runs until Ctrl+C is pressed
    while running.load(Ordering::SeqCst) {
        let x = rng.random_range(0..WIDTH);
        let y = rng.random_range(0..HEIGHT);

        let r = rng.random_range(0..=255);
        let g = rng.random_range(0..=255);
        let b = rng.random_range(0..=255);

        // Convert RGB888 -> RGB565 (Sense HAT format)
        let r5 = (r >> 3) as u16;
        let g6 = (g >> 2) as u16;
        let b5 = (b >> 3) as u16;

        let pixel: u16 = (r5 << 11) | (g6 << 5) | b5;

        // Framebuffer offset formula:
        // offset = (y * width + x) * bytes_per_pixel
        let offset = ((y * WIDTH) + x) * BPP;

        fb.seek(SeekFrom::Start(offset as u64))?;
        fb.write_all(&pixel.to_le_bytes())?;

        thread::sleep(Duration::from_millis(1));
    }

    // Ctrl+C pressed: clear the display before exiting
    clear_fb(&mut fb)?;

    Ok(())
}
