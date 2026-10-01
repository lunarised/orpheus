use fontdue::{Font, FontSettings};

/// Holds loaded fonts and provides text measurement and rasterization.
pub struct TextRenderer {
    pub font: Font,
}

/// Metrics for a laid-out string of text.
#[allow(dead_code)]
pub struct TextMetrics {
    pub width: f32,
    pub height: f32,
}

/// A rasterized glyph ready to be blitted onto a pixel buffer.
pub struct RasterizedGlyph {
    pub bitmap: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub x_offset: i32,
    pub y_offset: i32,
}

impl TextRenderer {
    pub fn new(font_path: &str) -> Self {
        let font_data = std::fs::read(font_path)
            .unwrap_or_else(|_| panic!("Failed to load font: {}", font_path));
        let font = Font::from_bytes(font_data, FontSettings::default())
            .unwrap_or_else(|_| panic!("Failed to parse font: {}", font_path));
        Self { font }
    }

    /// Load a font from a TrueType Collection (.ttc) file using a specific index.
    pub fn from_collection(font_path: &str, index: u32) -> Self {
        let font_data = std::fs::read(font_path)
            .unwrap_or_else(|_| panic!("Failed to load font: {}", font_path));
        let settings = FontSettings {
            collection_index: index,
            ..FontSettings::default()
        };
        let font = Font::from_bytes(font_data, settings)
            .unwrap_or_else(|_| panic!("Failed to parse font (index {}): {}", index, font_path));
        Self { font }
    }

    /// Measure the total pixel width and height of a string at a given size.
    /// This is the equivalent of Pillow's `draw.textbbox()` / `draw.textlength()`.
    pub fn measure(&self, text: &str, size: f32) -> TextMetrics {
        let mut width: f32 = 0.0;

        for ch in text.chars() {
            let metrics = self.font.metrics(ch, size);
            width += metrics.advance_width;
        }

        // Use the font's line metrics for consistent height across all strings
        let height = match self.font.horizontal_line_metrics(size) {
            Some(lm) => lm.ascent - lm.descent,
            None => size,
        };

        TextMetrics { width, height }
    }

    /// Get the pixel width of a string (shorthand).
    pub fn text_width(&self, text: &str, size: f32) -> f32 {
        self.measure(text, size).width
    }

    /// Get the font's ascent for the given size (distance from top of line to baseline).
    fn ascent(&self, size: f32) -> f32 {
        match self.font.horizontal_line_metrics(size) {
            Some(lm) => lm.ascent,
            None => size,
        }
    }

    /// Rasterize a string and return individual glyphs with positioning info.
    pub fn rasterize_text(&self, text: &str, size: f32) -> Vec<(f32, RasterizedGlyph)> {
        let mut glyphs = Vec::new();
        let mut x_cursor: f32 = 0.0;

        // Use the font's global ascent for a stable baseline across all characters
        let ascent = self.ascent(size);

        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(ch, size);

            // fontdue's Metrics.ymin = bottom of glyph above baseline (Y-up).
            // On screen (Y-down): glyph top = baseline - (ymin + height)
            // From the top of the text area: y_offset = ascent - ymin - height
            let glyph = RasterizedGlyph {
                bitmap,
                width: metrics.width,
                height: metrics.height,
                x_offset: metrics.xmin,
                y_offset: (ascent as i32) - metrics.ymin - metrics.height as i32,
            };

            glyphs.push((x_cursor, glyph));
            x_cursor += metrics.advance_width;
        }

        glyphs
    }

    /// Draw text directly onto an RGBA pixel buffer.
    /// `color` is [r, g, b].
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &self,
        buffer: &mut [u8],
        buf_width: u32,
        buf_height: u32,
        text: &str,
        x: i32,
        y: i32,
        size: f32,
        color: [u8; 3],
    ) {
        let glyphs = self.rasterize_text(text, size);

        for (glyph_x, glyph) in &glyphs {
            for gy in 0..glyph.height {
                for gx in 0..glyph.width {
                    let px = x + *glyph_x as i32 + glyph.x_offset + gx as i32;
                    let py = y + glyph.y_offset + gy as i32;

                    if px < 0 || py < 0 || px >= buf_width as i32 || py >= buf_height as i32 {
                        continue;
                    }

                    let alpha = glyph.bitmap[gy * glyph.width + gx];
                    if alpha == 0 {
                        continue;
                    }

                    let idx = ((py as u32 * buf_width + px as u32) * 4) as usize;
                    if idx + 3 >= buffer.len() {
                        continue;
                    }

                    // Alpha blend onto existing pixel
                    let a = alpha as f32 / 255.0;
                    let inv_a = 1.0 - a;
                    buffer[idx] = (color[0] as f32 * a + buffer[idx] as f32 * inv_a) as u8;
                    buffer[idx + 1] = (color[1] as f32 * a + buffer[idx + 1] as f32 * inv_a) as u8;
                    buffer[idx + 2] = (color[2] as f32 * a + buffer[idx + 2] as f32 * inv_a) as u8;
                    buffer[idx + 3] = 255;
                }
            }
        }
    }

    /// Draw marquee text: scrolls if wider than max_width, pauses at loop start.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_marquee_text(
        &self,
        buffer: &mut [u8],
        buf_width: u32,
        buf_height: u32,
        text: &str,
        x: i32,
        y: i32,
        max_width: i32,
        offset: i32,
        size: f32,
        color: [u8; 3],
    ) {
        let text_width = self.text_width(text, size) as i32;

        if text_width <= max_width {
            self.draw_text(buffer, buf_width, buf_height, text, x, y, size, color);
            return;
        }

        let gap = 50i32;
        let pause_frames = 60i32;
        let scroll_distance = text_width + gap;
        let cycle_length = scroll_distance + pause_frames;

        let position = offset % cycle_length;
        let scroll_offset = if position < pause_frames {
            0
        } else {
            position - pause_frames
        };

        // Draw text twice (seamless looping) clipped to max_width
        // We offset the text and clip by only drawing within bounds
        let draw_x = x - scroll_offset;
        self.draw_text_clipped(
            buffer,
            buf_width,
            buf_height,
            text,
            draw_x,
            y,
            size,
            color,
            x,
            x + max_width,
        );

        let draw_x2 = draw_x + text_width + gap;
        self.draw_text_clipped(
            buffer,
            buf_width,
            buf_height,
            text,
            draw_x2,
            y,
            size,
            color,
            x,
            x + max_width,
        );
    }

    /// Draw text but only within horizontal clip bounds [clip_left, clip_right).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_clipped(
        &self,
        buffer: &mut [u8],
        buf_width: u32,
        buf_height: u32,
        text: &str,
        x: i32,
        y: i32,
        size: f32,
        color: [u8; 3],
        clip_left: i32,
        clip_right: i32,
    ) {
        let glyphs = self.rasterize_text(text, size);

        for (glyph_x, glyph) in &glyphs {
            for gy in 0..glyph.height {
                for gx in 0..glyph.width {
                    let px = x + *glyph_x as i32 + glyph.x_offset + gx as i32;
                    let py = y + glyph.y_offset + gy as i32;

                    // Horizontal clipping for marquee
                    if px < clip_left || px >= clip_right {
                        continue;
                    }
                    if px < 0 || py < 0 || px >= buf_width as i32 || py >= buf_height as i32 {
                        continue;
                    }

                    let alpha = glyph.bitmap[gy * glyph.width + gx];
                    if alpha == 0 {
                        continue;
                    }

                    let idx = ((py as u32 * buf_width + px as u32) * 4) as usize;
                    if idx + 3 >= buffer.len() {
                        continue;
                    }

                    let a = alpha as f32 / 255.0;
                    let inv_a = 1.0 - a;
                    buffer[idx] = (color[0] as f32 * a + buffer[idx] as f32 * inv_a) as u8;
                    buffer[idx + 1] = (color[1] as f32 * a + buffer[idx + 1] as f32 * inv_a) as u8;
                    buffer[idx + 2] = (color[2] as f32 * a + buffer[idx + 2] as f32 * inv_a) as u8;
                    buffer[idx + 3] = 255;
                }
            }
        }
    }

    /// Draw truncated text with ellipsis if it exceeds max_width.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_truncated_text(
        &self,
        buffer: &mut [u8],
        buf_width: u32,
        buf_height: u32,
        text: &str,
        x: i32,
        y: i32,
        max_width: i32,
        size: f32,
        color: [u8; 3],
    ) {
        let text_width = self.text_width(text, size) as i32;

        if text_width <= max_width {
            self.draw_text(buffer, buf_width, buf_height, text, x, y, size, color);
        } else {
            // Truncate character by character until text + "..." fits
            let mut truncated = String::new();

            for ch in text.chars() {
                let candidate = format!("{}{}...", truncated, ch);
                if self.text_width(&candidate, size) > max_width as f32 {
                    break;
                }
                truncated.push(ch);
            }

            let display = format!("{}...", truncated);
            self.draw_text(buffer, buf_width, buf_height, &display, x, y, size, color);
        }
    }
}
