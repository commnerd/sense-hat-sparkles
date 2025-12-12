use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rand::Rng;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

// LED matrix is 8x8
const WIDTH: usize = 8;
const HEIGHT: usize = 8;

// IS31FL3731 I²C address for Sense HAT
// The Sense HAT LED driver is typically at 0x46, but can also be at 0x74
// Try both addresses if one fails
const IS31FL3731_ADDR_PRIMARY: u8 = 0x46;
const IS31FL3731_ADDR_SECONDARY: u8 = 0x74;
const I2C_DEVICE: &str = "/dev/i2c-1";

// IS31FL3731 register addresses
const IS31FL3731_REG_PICTURE_DISPLAY: u8 = 0x01;
const IS31FL3731_REG_AUTOPLAY1: u8 = 0x02;
const IS31FL3731_REG_AUTOPLAY2: u8 = 0x03;
const IS31FL3731_REG_DISPLAY_OPTION: u8 = 0x05;
const IS31FL3731_REG_AUDIO_SYNC: u8 = 0x06;
const IS31FL3731_REG_FRAME_STATE: u8 = 0x07;
const IS31FL3731_REG_BREATH1: u8 = 0x08;
const IS31FL3731_REG_BREATH2: u8 = 0x09;
const IS31FL3731_REG_SHUTDOWN: u8 = 0x0A;
const IS31FL3731_REG_AUDIOSYNC: u8 = 0x06;

// Frame register base addresses (8 frames available)
const IS31FL3731_FRAME_REG_BASE: u8 = 0x0B;
const IS31FL3731_FRAME_SIZE: usize = 144; // 8x8x2 + 8 (color registers)

// Decibel thresholds - adjusted for real-world microphone input
// Typical microphone input ranges from -90dB (silence) to -20dB (very loud)
// Progression: Green (quiet) -> Yellow (medium) -> Red (loud) -> Flash (very loud)
const MIN_DB: f32 = -80.0; // Minimum threshold (silence) - below this, lights off
const MAX_DB: f32 = 0.0;   // Maximum threshold (loud)
const GREEN_FULL: f32 = -65.0;  // Full green reached (quiet sounds)
const YELLOW_START: f32 = -55.0; // Start transitioning from green to yellow
const YELLOW_MAX: f32 = -35.0;   // Maximum yellow (start transitioning to red)
const RED_START: f32 = -35.0;    // Start transitioning from yellow to red
const FLASH_THRESHOLD: f32 = -20.0; // Threshold for flashing sequence (very loud)

// Flashing sequence duration
const FLASH_DURATION: Duration = Duration::from_secs(5);

// Smoothing factor for color transitions (0.0 = no smoothing, 1.0 = no change)
// Lower values = faster transitions, higher values = slower/smoother transitions
const COLOR_SMOOTHING: f32 = 0.85;

// IS31FL3731 LED driver interface
struct SenseHatLED {
    i2c: File,
    address: u8,
}

impl SenseHatLED {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Check if I²C device exists
        if !std::path::Path::new(I2C_DEVICE).exists() {
            return Err(format!("I²C device {} not found. Is I²C enabled?", I2C_DEVICE).into());
        }

        // Try primary address first (0x46), then secondary (0x74)
        let addresses = [IS31FL3731_ADDR_PRIMARY, IS31FL3731_ADDR_SECONDARY];
        let mut last_error = None;

        for &addr in &addresses {
            let i2c = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(I2C_DEVICE)
            {
                Ok(f) => f,
                Err(e) => {
                    last_error = Some(format!("Failed to open {}: {}", I2C_DEVICE, e));
                    continue;
                }
            };

            // Set I²C slave address using ioctl
            let ioctl_result = unsafe {
                let fd = i2c.as_raw_fd();
                // I2C_SLAVE = 0x0703
                libc::ioctl(fd, 0x0703, addr as libc::c_ulong)
            };

            if ioctl_result < 0 {
                last_error = Some(format!(
                    "Failed to set I²C slave address 0x{:02X}: {}",
                    addr,
                    std::io::Error::last_os_error()
                ));
                continue;
            }

            // Try to initialize - if this works, we found the right address
            let mut led = SenseHatLED { i2c, address: addr };
            match led.init() {
                Ok(_) => {
                    println!("Successfully initialized Sense HAT LED at I²C address 0x{:02X}", addr);
                    return Ok(led);
                }
                Err(e) => {
                    last_error = Some(format!("Initialization failed at 0x{:02X}: {}", addr, e));
                    continue;
                }
            }
        }

        // If we get here, both addresses failed
        Err(format!(
            "Failed to initialize Sense HAT LED matrix. Tried addresses 0x{:02X} and 0x{:02X}.\nLast error: {}\n\nTroubleshooting:\n- Ensure I²C is enabled: sudo raspi-config\n- Check device exists: ls -l {}\n- Verify permissions: sudo usermod -a -G i2c $USER\n- Check if Sense HAT is connected: i2cdetect -y 1",
            IS31FL3731_ADDR_PRIMARY,
            IS31FL3731_ADDR_SECONDARY,
            last_error.unwrap_or_else(|| "Unknown error".to_string()),
            I2C_DEVICE
        ).into())
    }

    fn init(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Shutdown register - enable chip (0x01 = normal operation, 0x00 = shutdown)
        self.write_register(IS31FL3731_REG_SHUTDOWN, 0x01)
            .map_err(|e| format!("Failed to write shutdown register: {}", e))?;
        thread::sleep(Duration::from_millis(10));

        // Display option - use frame 0
        self.write_register(IS31FL3731_REG_DISPLAY_OPTION, 0x00)
            .map_err(|e| format!("Failed to write display option register: {}", e))?;

        // Picture display - show frame 0
        self.write_register(IS31FL3731_REG_PICTURE_DISPLAY, 0x00)
            .map_err(|e| format!("Failed to write picture display register: {}", e))?;

        // Clear frame 0
        self.clear_frame(0)
            .map_err(|e| format!("Failed to clear frame: {}", e))?;

        Ok(())
    }

    fn write_register(&mut self, reg: u8, value: u8) -> Result<(), Box<dyn std::error::Error>> {
        // I²C write: first byte is register address, second byte is value
        let buf = [reg, value];
        self.i2c.write_all(&buf)
            .map_err(|e| format!("I²C write failed (reg: 0x{:02X}, val: 0x{:02X}): {}", reg, value, e))?;
        // Small delay to ensure write completes
        thread::sleep(Duration::from_micros(100));
        Ok(())
    }

    fn select_frame(&mut self, frame: u8) -> Result<(), Box<dyn std::error::Error>> {
        // Frame select register (0xFD) must be written before accessing frame data
        self.write_register(0xFD, frame)?;
        Ok(())
    }

    fn clear_frame(&mut self, frame: u8) -> Result<(), Box<dyn std::error::Error>> {
        // Select frame
        self.select_frame(frame)?;

        // Clear all LED enable bits (first 18 bytes: 144 LEDs / 8 bits per byte)
        for i in 0..18 {
            self.write_register(IS31FL3731_FRAME_REG_BASE + i, 0x00)?;
        }

        // Clear all PWM data (next 144 bytes)
        for i in 18..162 {
            self.write_register(IS31FL3731_FRAME_REG_BASE + i, 0x00)?;
        }

        Ok(())
    }

    fn set_pixel(&mut self, x: usize, y: usize, r: u8, g: u8, b: u8) -> Result<(), Box<dyn std::error::Error>> {
        // Sense HAT LED matrix uses a specific mapping
        // The IS31FL3731 has 144 LEDs arranged in a specific pattern
        // Sense HAT maps these to an 8x8 grid with rotation
        
        // Sense HAT coordinate system: (0,0) is top-left when viewed normally
        // IS31FL3731 LED indices: The Sense HAT Python library uses a specific mapping
        // This is a simplified version - the actual mapping may need adjustment
        
        // Select frame 0 for writing
        self.select_frame(0)?;

        // Sense HAT LED mapping (simplified - may need calibration)
        // The actual mapping depends on how the Sense HAT hardware is wired
        // For now, use a direct mapping and adjust if needed
        let led_index = (y * 8 + x) as u8;

        // Enable the LED (first 18 bytes are enable registers)
        let enable_byte = (led_index / 8) as u8;
        let enable_reg = IS31FL3731_FRAME_REG_BASE + enable_byte;
        
        // We need to read-modify-write, but for simplicity, enable all bits in the byte
        // A full implementation would read, modify, then write
        self.write_register(enable_reg, 0xFF)?;

        // Set PWM value (brightness) - next 144 bytes are PWM data
        let pwm_reg = IS31FL3731_FRAME_REG_BASE + 18 + led_index;
        // Use average brightness for now (full RGB support requires color registers)
        let brightness = ((r as u16 + g as u16 + b as u16) / 3).min(255) as u8;
        self.write_register(pwm_reg, brightness)?;

        Ok(())
    }

    fn fill_all(&mut self, r: u8, g: u8, b: u8) -> Result<(), Box<dyn std::error::Error>> {
        // Select frame 0
        self.select_frame(0)?;

        // Enable all LEDs (first 18 bytes)
        for i in 0..18 {
            self.write_register(IS31FL3731_FRAME_REG_BASE + i, 0xFF)?;
        }

        // Set PWM for all LEDs (next 144 bytes)
        // Use average brightness (full RGB requires color register setup)
        let brightness = ((r as u16 + g as u16 + b as u16) / 3).min(255) as u8;
        for i in 18..162 {
            self.write_register(IS31FL3731_FRAME_REG_BASE + i, brightness)?;
        }

        Ok(())
    }

    fn clear(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.clear_frame(0)?;
        Ok(())
    }
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

    if db < GREEN_FULL {
        // Very quiet sound - fade in green from black
        let ratio = (db - MIN_DB) / (GREEN_FULL - MIN_DB);
        let green = (ratio * 255.0).min(255.0) as u8;
        return (0, green, 0);
    }

    if db < YELLOW_START {
        // Quiet sound - full green
        return (0, 255, 0);
    }

    if db < YELLOW_MAX {
        // Medium sound - transition from green to yellow
        let ratio = (db - YELLOW_START) / (YELLOW_MAX - YELLOW_START);
        let green = 255;
        let red = (ratio * 255.0).min(255.0) as u8;
        return (red, green, 0);
    }

    if db < FLASH_THRESHOLD {
        // Loud sound - transition from yellow to red
        let ratio = (db - RED_START) / (FLASH_THRESHOLD - RED_START);
        let red = 255;
        let green = ((1.0 - ratio) * 255.0).max(0.0) as u8;
        return (red, green, 0);
    }

    // At or above flash threshold - return red (flashing will be handled separately)
    (255, 0, 0)
}

fn flashing_sequence(led: &mut SenseHatLED, running: &Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    // Use the sparkles algorithm from main branch
    let start_time = Instant::now();
    let mut rng = rand::rng();

    while running.load(Ordering::SeqCst) && start_time.elapsed() < FLASH_DURATION {
        // Pick random x, y coordinate
        let x = rng.random_range(0..WIDTH);
        let y = rng.random_range(0..HEIGHT);

        // Pick random RGB color
        let r = rng.random_range(0..=255);
        let g = rng.random_range(0..=255);
        let b = rng.random_range(0..=255);

        led.set_pixel(x, y, r, g, b)?;

        thread::sleep(Duration::from_millis(1)); // Same timing as main branch
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize Sense HAT LED matrix via I²C
    let mut led = SenseHatLED::new()
        .map_err(|e| format!("Could not initialize Sense HAT LED matrix: {}\n\nTroubleshooting:\n- Ensure I²C is enabled: sudo raspi-config\n- Check device exists: ls -l {}\n- Verify permissions: sudo usermod -a -G i2c $USER\n- Check if Sense HAT is connected: i2cdetect -y 1", e, I2C_DEVICE))?;

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
    
    // Current displayed color (for smoothing) - start with green for quiet sounds
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
            flashing_sequence(&mut led, &running)?;
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
            
            led.fill_all(current_color.0, current_color.1, current_color.2)?;
        }

        // Small delay to avoid excessive writes
        thread::sleep(Duration::from_millis(50));
    }

    // Ctrl+C pressed: clear the display before exiting
    led.clear()?;

    Ok(())
}
