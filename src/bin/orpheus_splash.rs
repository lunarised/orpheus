//! Tiny, dependency-light Orpheus boot splash for the NV3007 display.
//!
//! This is deliberately a separate binary from Orpheus. It initializes
//! the panel, draws one frame, holds it briefly, and then drops every SPI/GPIO
//! handle before the main application starts.

use std::path::PathBuf;
use std::time::Duration;

const WIDTH: usize = 428;
const HEIGHT: usize = 142;
const DEFAULT_DURATION_MS: u64 = 3_000;
const MAX_DURATION_MS: u64 = 15_000;

#[cfg(feature = "hardware")]
#[path = "../display.rs"]
mod display;

#[derive(Debug, PartialEq, Eq)]
struct Options {
    duration_ms: u64,
    preview: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            duration_ms: DEFAULT_DURATION_MS,
            preview: None,
        }
    }
}

impl Options {
    fn parse<I>(arguments: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--duration-ms" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--duration-ms needs a value".to_string())?;
                    options.duration_ms = parse_duration(&value)?;
                }
                "--preview" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--preview needs a PNG path".to_string())?;
                    options.preview = Some(PathBuf::from(value));
                }
                "--help" | "-h" => return Err(usage().to_string()),
                _ => {
                    if let Some(value) = argument.strip_prefix("--duration-ms=") {
                        options.duration_ms = parse_duration(value)?;
                    } else if let Some(value) = argument.strip_prefix("--preview=") {
                        if value.is_empty() {
                            return Err("--preview needs a PNG path".to_string());
                        }
                        options.preview = Some(PathBuf::from(value));
                    } else {
                        return Err(format!("unknown argument: {argument}"));
                    }
                }
            }
        }
        Ok(options)
    }
}

fn parse_duration(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|duration| *duration <= MAX_DURATION_MS)
        .ok_or_else(|| format!("duration must be between 0 and {MAX_DURATION_MS} milliseconds"))
}

fn usage() -> &'static str {
    "Usage: orpheus-splash [--duration-ms 3000] [--preview splash.png]"
}

fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            if message != usage() {
                eprintln!("{}", usage());
            }
            std::process::exit(if message == usage() { 0 } else { 2 });
        }
    };

    let frame = render_splash();
    if let Some(path) = options.preview {
        save_preview(&frame, &path);
        return;
    }

    #[cfg(feature = "hardware")]
    {
        let mut panel = display::HardwareDisplay::new().unwrap_or_else(|error| {
            eprintln!("Orpheus splash could not initialize the display: {error}");
            std::process::exit(1);
        });
        panel.set_brightness(0.58);
        panel.push_frame(&frame).unwrap_or_else(|error| {
            eprintln!("Orpheus splash could not write its frame: {error}");
            std::process::exit(1);
        });
        println!(
            "Orpheus boot splash displayed for {} ms",
            options.duration_ms
        );
        std::thread::sleep(Duration::from_millis(options.duration_ms));
        // `panel` is dropped here, releasing SPI0 and GPIO 24/25 before the
        // systemd unit that follows us is allowed to start.
    }

    #[cfg(not(feature = "hardware"))]
    {
        let _ = (frame, options.duration_ms, Duration::ZERO);
        eprintln!("This binary needs the Cargo feature `hardware` unless --preview is used.");
        std::process::exit(1);
    }
}

fn save_preview(frame: &[u8], path: &std::path::Path) {
    let image = image::RgbaImage::from_raw(WIDTH as u32, HEIGHT as u32, frame.to_vec())
        .expect("splash frame dimensions should be valid");
    image.save(path).unwrap_or_else(|error| {
        eprintln!(
            "Could not save splash preview '{}': {error}",
            path.display()
        );
        std::process::exit(1);
    });
    println!("Saved Orpheus splash preview to {}", path.display());
}

fn render_splash() -> Vec<u8> {
    let mut frame = vec![0_u8; WIDTH * HEIGHT * 4];

    // A deep indigo backdrop with a gentle horizontal dawn-like glow.
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let horizontal = 1.0 - ((x as f32 / WIDTH as f32) - 0.36).abs().min(0.75);
            let vertical = 1.0 - ((y as f32 / HEIGHT as f32) - 0.48).abs().min(0.75);
            let glow = horizontal * vertical;
            put_pixel(
                &mut frame,
                x as i32,
                y as i32,
                [
                    (5.0 + 13.0 * glow) as u8,
                    (7.0 + 11.0 * glow) as u8,
                    (19.0 + 31.0 * glow) as u8,
                    255,
                ],
            );
        }
    }

    // Sparse fixed stars provide texture without looking noisy on RGB565.
    for (x, y, alpha) in [
        (18, 20, 44),
        (38, 119, 30),
        (111, 13, 26),
        (137, 126, 42),
        (286, 18, 25),
        (326, 128, 36),
        (391, 23, 46),
        (410, 105, 30),
    ] {
        blend_pixel(&mut frame, x, y, [155, 178, 255, alpha]);
    }

    // Orpheus mark: an orbital ring surrounding a seven-band sound glyph.
    for radius in 38..=41 {
        draw_circle(&mut frame, 73, 70, radius, [80, 76, 192, 42]);
    }
    draw_circle(&mut frame, 73, 70, 35, [126, 118, 255, 205]);
    draw_arc_accents(&mut frame, 73, 70, 42);

    let bands = [14_i32, 25, 38, 54, 38, 25, 14];
    for (index, height) in bands.into_iter().enumerate() {
        let x = 49 + index as i32 * 8;
        let top = 70 - height / 2;
        let t = index as f32 / 6.0;
        let color = [(69.0 + 92.0 * t) as u8, (221.0 - 72.0 * t) as u8, 238, 255];
        fill_rounded_rect(&mut frame, x, top, 4, height, 2, color);
    }

    // Built-in pixel lettering keeps early boot independent of font files.
    draw_text(&mut frame, 145, 38, "ORPHEUS", 5, [237, 239, 255, 255]);
    draw_text(&mut frame, 180, 91, "MUSIC SYSTEM", 2, [144, 154, 196, 255]);

    // A restrained boot indicator grounds the mark without suggesting a
    // precise percentage that systemd cannot actually know.
    fill_rounded_rect(&mut frame, 145, 119, 205, 3, 1, [42, 45, 78, 255]);
    for x in 0..132 {
        let t = x as f32 / 131.0;
        fill_rect(
            &mut frame,
            145 + x,
            119,
            1,
            3,
            [(66.0 + 81.0 * t) as u8, (219.0 - 54.0 * t) as u8, 235, 255],
        );
    }

    frame
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        _ => [0; 7],
    }
}

fn draw_text(frame: &mut [u8], x: i32, y: i32, text: &str, scale: i32, color: [u8; 4]) {
    let mut cursor = x;
    for character in text.chars() {
        let rows = glyph(character);
        for (row, bits) in rows.into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    fill_rect(
                        frame,
                        cursor + column * scale,
                        y + row as i32 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
        cursor += 6 * scale;
    }
}

fn draw_arc_accents(frame: &mut [u8], cx: i32, cy: i32, radius: i32) {
    for degrees in 205..326 {
        if degrees % 2 == 0 {
            let radians = (degrees as f32).to_radians();
            let x = cx + (radians.cos() * radius as f32).round() as i32;
            let y = cy + (radians.sin() * radius as f32).round() as i32;
            blend_pixel(frame, x, y, [72, 220, 234, 210]);
        }
    }
    for degrees in 18..112 {
        if degrees % 2 == 0 {
            let radians = (degrees as f32).to_radians();
            let x = cx + (radians.cos() * radius as f32).round() as i32;
            let y = cy + (radians.sin() * radius as f32).round() as i32;
            blend_pixel(frame, x, y, [154, 107, 255, 210]);
        }
    }
}

fn draw_circle(frame: &mut [u8], cx: i32, cy: i32, radius: i32, color: [u8; 4]) {
    let mut x = radius;
    let mut y = 0;
    let mut decision = 1 - radius;
    while x >= y {
        for (dx, dy) in [
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            blend_pixel(frame, cx + dx, cy + dy, color);
        }
        y += 1;
        if decision <= 0 {
            decision += 2 * y + 1;
        } else {
            x -= 1;
            decision += 2 * (y - x) + 1;
        }
    }
}

fn fill_rounded_rect(
    frame: &mut [u8],
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
    color: [u8; 4],
) {
    for py in 0..height {
        for px in 0..width {
            let dx = if px < radius {
                radius - px
            } else if px >= width - radius {
                px - (width - radius - 1)
            } else {
                0
            };
            let dy = if py < radius {
                radius - py
            } else if py >= height - radius {
                py - (height - radius - 1)
            } else {
                0
            };
            if dx == 0 || dy == 0 || dx * dx + dy * dy <= radius * radius {
                blend_pixel(frame, x + px, y + py, color);
            }
        }
    }
}

fn fill_rect(frame: &mut [u8], x: i32, y: i32, width: i32, height: i32, color: [u8; 4]) {
    for py in y..y + height {
        for px in x..x + width {
            blend_pixel(frame, px, py, color);
        }
    }
}

fn blend_pixel(frame: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
        return;
    }
    let index = (y as usize * WIDTH + x as usize) * 4;
    let alpha = u16::from(color[3]);
    let inverse = 255 - alpha;
    for channel in 0..3 {
        frame[index + channel] = ((u16::from(color[channel]) * alpha
            + u16::from(frame[index + channel]) * inverse)
            / 255) as u8;
    }
    frame[index + 3] = 255;
}

fn put_pixel(frame: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
        return;
    }
    let index = (y as usize * WIDTH + x as usize) * 4;
    frame[index..index + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splash_is_an_opaque_panel_sized_frame() {
        let frame = render_splash();
        assert_eq!(frame.len(), WIDTH * HEIGHT * 4);
        assert!(frame.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert!(frame.chunks_exact(4).any(|pixel| pixel[1] > 180));
        assert!(frame.chunks_exact(4).any(|pixel| pixel[0] > 220));
    }

    #[test]
    fn duration_is_bounded_and_preview_is_optional() {
        assert_eq!(
            Options::parse(["--duration-ms=2750".to_string()]).unwrap(),
            Options {
                duration_ms: 2_750,
                preview: None,
            }
        );
        assert!(Options::parse(["--duration-ms=15001".to_string()]).is_err());
        assert!(Options::parse(["--unknown".to_string()]).is_err());
    }
}
