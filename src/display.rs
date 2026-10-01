//! NV3007 display driver for Raspberry Pi 5 over SPI.
//!
//! Ported from the Python driver (driver.py). Drives a 142×428 RGB565 panel
//! using SPI0 at 40 MHz with GPIO pins for DC and RST.

use rppal::gpio::{Gpio, OutputPin};
use rppal::spi::{Bus, Mode, SlaveSelect, Spi};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

// Backlight PWM via sysfs (GPIO 12, pin 32, pwmchip0/pwm0)
const PWM_CHIP: &str = "/sys/class/pwm/pwmchip0";
const PWM_CHANNEL: u32 = 0;
const PWM_PERIOD_NS: u64 = 1_000_000; // 1ms = 1kHz

// Physical display dimensions (portrait native)
const DISPLAY_COLS: u32 = 142;
const DISPLAY_ROWS: u32 = 428;

// Addressing offsets (from the working Python driver)
const X_OFFSET: u8 = 12;
const Y_OFFSET: u8 = 0;

// GPIO pin numbers (BCM)
const DC_PIN: u8 = 24;
const RST_PIN: u8 = 25;

/// NV3007 initialization sequence.
/// Each entry: (command, &[data_bytes])
const INIT_SEQUENCE: &[(u8, &[u8])] = &[
    (0xFF, &[0xA5]),
    (0x9A, &[0x08]),
    (0x9B, &[0x08]),
    (0x9C, &[0xB0]),
    (0x9D, &[0x16]),
    (0x9E, &[0xC4]),
    (0x8F, &[0x55, 0x04]),
    (0x84, &[0x90]),
    (0x83, &[0x7B]),
    (0x85, &[0x33]),
    (0x60, &[0x00]),
    (0x70, &[0x00]),
    (0x61, &[0x02]),
    (0x71, &[0x02]),
    (0x62, &[0x04]),
    (0x72, &[0x04]),
    (0x6C, &[0x29]),
    (0x7C, &[0x29]),
    (0x6D, &[0x31]),
    (0x7D, &[0x31]),
    (0x6E, &[0x0F]),
    (0x7E, &[0x0F]),
    (0x66, &[0x21]),
    (0x76, &[0x21]),
    (0x68, &[0x3A]),
    (0x78, &[0x3A]),
    (0x63, &[0x07]),
    (0x73, &[0x07]),
    (0x64, &[0x05]),
    (0x74, &[0x05]),
    (0x65, &[0x02]),
    (0x75, &[0x02]),
    (0x67, &[0x23]),
    (0x77, &[0x23]),
    (0x69, &[0x08]),
    (0x79, &[0x08]),
    (0x6A, &[0x13]),
    (0x7A, &[0x13]),
    (0x6B, &[0x13]),
    (0x7B, &[0x13]),
    (0x6F, &[0x00]),
    (0x7F, &[0x00]),
    (0x50, &[0x00]),
    (0x52, &[0xD6]),
    (0x53, &[0x08]),
    (0x54, &[0x08]),
    (0x55, &[0x1E]),
    (0x56, &[0x1C]),
    (0xA0, &[0x2B, 0x24, 0x00]),
    (0xA1, &[0x87]),
    (0xA2, &[0x86]),
    (0xA5, &[0x00]),
    (0xA6, &[0x00]),
    (0xA7, &[0x00]),
    (0xA8, &[0x36]),
    (0xA9, &[0x7E]),
    (0xAA, &[0x7E]),
    (0xB9, &[0x85]),
    (0xBA, &[0x84]),
    (0xBB, &[0x83]),
    (0xBC, &[0x82]),
    (0xBD, &[0x81]),
    (0xBE, &[0x80]),
    (0xBF, &[0x01]),
    (0xC0, &[0x02]),
    (0xC1, &[0x00]),
    (0xC2, &[0x00]),
    (0xC3, &[0x00]),
    (0xC4, &[0x33]),
    (0xC5, &[0x7E]),
    (0xC6, &[0x7E]),
    (0xC8, &[0x33, 0x33]),
    (0xC9, &[0x68]),
    (0xCA, &[0x69]),
    (0xCB, &[0x6A]),
    (0xCC, &[0x6B]),
    (0xCD, &[0x33, 0x33]),
    (0xCE, &[0x6C]),
    (0xCF, &[0x6D]),
    (0xD0, &[0x6E]),
    (0xD1, &[0x6F]),
    (0xAB, &[0x03, 0x67]),
    (0xAC, &[0x03, 0x6B]),
    (0xAD, &[0x03, 0x68]),
    (0xAE, &[0x03, 0x6C]),
    (0xB3, &[0x00]),
    (0xB4, &[0x00]),
    (0xB5, &[0x00]),
    (0xB6, &[0x32]),
    (0xB7, &[0x7E]),
    (0xB8, &[0x7E]),
    (0xE0, &[0x00]),
    (0xE1, &[0x03, 0x0F]),
    (0xE2, &[0x04]),
    (0xE3, &[0x01]),
    (0xE4, &[0x0E]),
    (0xE5, &[0x01]),
    (0xE6, &[0x19]),
    (0xE7, &[0x10]),
    (0xE8, &[0x10]),
    (0xEA, &[0x12]),
    (0xEB, &[0xD0]),
    (0xEC, &[0x04]),
    (0xED, &[0x07]),
    (0xEE, &[0x07]),
    (0xEF, &[0x09]),
    (0xF0, &[0xD0]),
    (0xF1, &[0x0E]),
    (0xF9, &[0x17]),
    (0xF2, &[0x2C, 0x1B, 0x0B, 0x20]),
    (0xE9, &[0x29]),
    (0xEC, &[0x04]),
    (0x35, &[0x00]),
    (0x44, &[0x00, 0x10]),
    (0x46, &[0x10]),
    (0xFF, &[0x00]),
    (0x3A, &[0x05]), // RGB565 pixel format
    (0x11, &[]),     // Sleep Out
    (0x29, &[]),     // Display On
];

/// Hardware display driver for the NV3007 panel on Pi 5.
pub struct HardwareDisplay {
    spi: Spi,
    dc: OutputPin,
    rst: OutputPin,
    /// Pre-allocated RGB565 buffer for the full display (142×428×2 bytes)
    pixel_buf: Vec<u8>,
    /// Sysfs path for backlight PWM duty_cycle
    pwm_duty_path: String,
    pwm_period_ns: u64,
}

impl HardwareDisplay {
    /// Initialize the display hardware (SPI + GPIO) and run the init sequence.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // SPI0, CE0, Mode 0 (CPOL=0, CPHA=0), 40 MHz
        let spi = Spi::new(Bus::Spi0, SlaveSelect::Ss0, 40_000_000, Mode::Mode0)?;

        let gpio = Gpio::new()?;
        let dc = gpio.get(DC_PIN)?.into_output();
        let rst = gpio.get(RST_PIN)?.into_output();

        let pixel_buf = vec![0u8; (DISPLAY_COLS * DISPLAY_ROWS * 2) as usize];

        // Initialize backlight PWM via sysfs
        let pwm_path = format!("{}/pwm{}", PWM_CHIP, PWM_CHANNEL);
        if !Path::new(&pwm_path).exists() {
            fs::write(format!("{}/export", PWM_CHIP), PWM_CHANNEL.to_string())?;
            // Give sysfs time to create the directory
            thread::sleep(Duration::from_millis(50));
        }
        // Configure period and duty cycle (100% = fully on)
        fs::write(format!("{}/period", pwm_path), PWM_PERIOD_NS.to_string())?;
        fs::write(
            format!("{}/duty_cycle", pwm_path),
            PWM_PERIOD_NS.to_string(),
        )?;
        fs::write(format!("{}/enable", pwm_path), "1")?;
        let pwm_duty_path = format!("{}/duty_cycle", pwm_path);
        println!("Backlight PWM initialized via sysfs ({})", pwm_path);

        let mut display = Self {
            spi,
            dc,
            rst,
            pixel_buf,
            pwm_duty_path,
            pwm_period_ns: PWM_PERIOD_NS,
        };

        display.init()?;
        Ok(display)
    }

    /// Hardware reset pulse.
    fn reset(&mut self) {
        self.rst.set_high();
        thread::sleep(Duration::from_millis(100));
        self.rst.set_low();
        thread::sleep(Duration::from_millis(100));
        self.rst.set_high();
        thread::sleep(Duration::from_millis(200));
    }

    /// Write a command byte (DC low), optionally followed by data bytes (DC high).
    fn write_cmd(&mut self, cmd: u8, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.dc.set_low();
        self.spi.write(&[cmd])?;
        if !data.is_empty() {
            self.dc.set_high();
            self.spi.write(data)?;
        }
        Ok(())
    }

    /// Run the full initialization sequence.
    fn init(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.reset();

        for &(cmd, data) in INIT_SEQUENCE {
            self.write_cmd(cmd, data)?;

            // Timing requirements from the datasheet / working driver
            if cmd == 0x11 {
                thread::sleep(Duration::from_millis(200));
            }
            if cmd == 0x29 {
                thread::sleep(Duration::from_millis(150));
            }
        }

        println!("NV3007: initialization complete");
        Ok(())
    }

    /// Set the display write window to the full screen area.
    fn set_window(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // CASET: column address set [X_OFFSET .. 141 + X_OFFSET]
        let x_end = 141 + X_OFFSET as u16;
        self.write_cmd(
            0x2A,
            &[0x00, X_OFFSET, (x_end >> 8) as u8, (x_end & 0xFF) as u8],
        )?;

        // RASET: row address set [Y_OFFSET .. 427 + Y_OFFSET]
        let y_end = 427 + Y_OFFSET as u16;
        self.write_cmd(
            0x2B,
            &[0x00, Y_OFFSET, (y_end >> 8) as u8, (y_end & 0xFF) as u8],
        )?;

        Ok(())
    }

    /// Push a full frame to the display.
    ///
    /// Takes the renderer's RGBA buffer (428×142, landscape) and writes it to
    /// the panel (142×428, portrait) with a 90° clockwise rotation and
    /// RGBA→RGB565 conversion.
    pub fn push_frame(&mut self, rgba: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        // Convert RGBA (428w × 142h) → RGB565 (142 cols × 428 rows) with CW90 rotation.
        //
        // For each display row `r` (0..428) and column `c` (0..142):
        //   source pixel = renderer (x=r, y=141-c)
        //   buffer index = ((141 - c) * SCREEN_WIDTH + r) * 4
        self.convert_frame_rgb565(rgba);

        // Set the address window and stream pixels
        self.set_window()?;

        // RAMWR command
        self.dc.set_low();
        self.spi.write(&[0x2C])?;

        // Stream pixel data
        self.dc.set_high();

        // Send in large chunks to minimize syscall overhead
        // rppal SPI max transfer is typically 4096 bytes; use 4K chunks
        const CHUNK_SIZE: usize = 4096;
        let total = (DISPLAY_ROWS * DISPLAY_COLS * 2) as usize;
        let mut offset = 0;
        while offset < total {
            let end = (offset + CHUNK_SIZE).min(total);
            self.spi.write(&self.pixel_buf[offset..end])?;
            offset = end;
        }

        Ok(())
    }

    /// Convert the renderer's RGBA landscape buffer into the pre-allocated
    /// RGB565 portrait buffer with 90° clockwise rotation.
    fn convert_frame_rgb565(&mut self, rgba: &[u8]) {
        // The landscape renderer width is the panel's native row count.
        // Keeping this relationship here allows small boot-stage binaries to
        // reuse the driver without depending on Orpheus's renderer.
        let src_w = DISPLAY_ROWS as usize;
        let dst_cols = DISPLAY_COLS as usize; // 142
        let dst_rows = DISPLAY_ROWS as usize; // 428

        let mut dst_idx = 0;
        for row in 0..dst_rows {
            for col in 0..dst_cols {
                // 90° CW: display(col, row) ← renderer(x=row, y=141-col)
                let src_x = row;
                let src_y = (dst_cols - 1) - col;
                let src_idx = (src_y * src_w + src_x) * 4;

                let r = rgba[src_idx] as u16;
                let g = rgba[src_idx + 1] as u16;
                let b = rgba[src_idx + 2] as u16;

                // RGB565: RRRRR GGGGGG BBBBB (big-endian for the display)
                let pixel = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);
                self.pixel_buf[dst_idx] = (pixel >> 8) as u8;
                self.pixel_buf[dst_idx + 1] = (pixel & 0xFF) as u8;
                dst_idx += 2;
            }
        }
    }

    /// Set the backlight brightness via hardware PWM.
    /// `brightness` is 0.0 (off) to 1.0 (full).
    pub fn set_brightness(&mut self, brightness: f64) {
        let duty = brightness.clamp(0.0, 1.0);
        let duty_ns = (duty * self.pwm_period_ns as f64) as u64;
        fs::write(&self.pwm_duty_path, duty_ns.to_string()).ok();
    }

    /// Fill the entire screen with a single RGB565 color (useful for testing).
    #[allow(dead_code)]
    pub fn fill_screen(&mut self, color_565: u16) -> Result<(), Box<dyn std::error::Error>> {
        let high = (color_565 >> 8) as u8;
        let low = (color_565 & 0xFF) as u8;

        // Fill the pixel buffer
        for i in 0..(DISPLAY_COLS * DISPLAY_ROWS) as usize {
            self.pixel_buf[i * 2] = high;
            self.pixel_buf[i * 2 + 1] = low;
        }

        self.set_window()?;

        self.dc.set_low();
        self.spi.write(&[0x2C])?;
        self.dc.set_high();

        let row_bytes = (DISPLAY_COLS * 2) as usize;
        for row in 0..DISPLAY_ROWS as usize {
            let start = row * row_bytes;
            let end = start + row_bytes;
            self.spi.write(&self.pixel_buf[start..end])?;
        }

        Ok(())
    }
}
