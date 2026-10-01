use crate::audio_visualizer::BAR_COUNT;
use crate::config::{NowPlayingStyle, ScreensaverStyle};
use crate::playlists::Track;
use crate::state::{
    AppState, GUEST_WEB_URL, MAIN_MENU_ITEMS, PlaybackSource, SettingKind, UiMode,
    format_dim_timeout, format_time,
};
use crate::text::TextRenderer;
use image::{DynamicImage, GenericImageView, ImageReader};
use qrcodegen::{QrCode, QrCodeEcc};
use std::path::Path;

pub const SCREEN_WIDTH: u32 = 428;
pub const SCREEN_HEIGHT: u32 = 142;
pub const TEXT_LEFT_MARGIN: i32 = 150;
const BG_COLOR: [u8; 3] = [0x0D, 0x0E, 0x10];
const ART_SIZE: u32 = 118;

/// Open artwork by inspecting its signature instead of trusting its filename.
/// Some media servers return PNG bytes for URLs advertised and cached as JPEG.
fn open_artwork(path: impl AsRef<Path>) -> image::ImageResult<DynamicImage> {
    ImageReader::open(path)?.with_guessed_format()?.decode()
}

/// Map a coordinate in a rendered image back into its cached source image.
///
/// Carousel artwork is normally cached at 110 px, while the single-cover idle
/// view deliberately renders it a little larger. Keeping the scaling here
/// makes the drawing routine safe for either size (and for future layouts).
fn scaled_source_coordinate(coordinate: u32, destination_size: u32, source_size: u32) -> u32 {
    debug_assert!(destination_size > 0);
    debug_assert!(source_size > 0);
    ((coordinate as u64 * source_size as u64) / destination_size as u64)
        .min((source_size - 1) as u64) as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BaseFrameKey {
    artist: String,
    album: String,
    album_art_path: String,
}

impl BaseFrameKey {
    fn from_state(state: &AppState) -> Self {
        Self::from_track(state.current_track())
    }

    fn from_track(track: &Track) -> Self {
        Self {
            artist: track.artist.clone(),
            album: track.album.clone(),
            album_art_path: track.album_art_path.clone(),
        }
    }
}

pub const FONT_SIZE_LARGE: f32 = 20.0;
pub const FONT_SIZE_MEDIUM: f32 = 14.0;
pub const FONT_SIZE_SMALL: f32 = 11.0;

pub struct Renderer {
    pub text_renderer: TextRenderer,
    pub text_renderer_mono: TextRenderer,
    // Cached album art (pre-resized) and accent color
    cached_art_path: String,
    cached_art: Option<DynamicImage>,
    cached_accent: [u8; 3],
    // Pre-rendered base frame (background + album art + static text)
    base_frame: Vec<u8>,
    base_frame_dirty: bool,
    base_frame_key: Option<BaseFrameKey>,
    // Cached picker art (pre-decoded and resized)
    cached_picker_art_path: String,
    cached_picker_art: Option<DynamicImage>,
    // Cached, resized images used by the idle album-art carousel.
    cached_screensaver_paths: Vec<String>,
    cached_screensaver_art: Vec<Option<DynamicImage>>,
    screensaver_preload_index: usize,
}

impl Renderer {
    pub fn new(font_path: &str, mono_font_path: &str, _assets_dir: &str) -> Self {
        // Use collection loader for .ttc files, regular loader for .ttf
        let text_renderer = if font_path.ends_with(".ttc") {
            TextRenderer::from_collection(font_path, 0)
        } else {
            TextRenderer::new(font_path)
        };

        Self {
            text_renderer,
            text_renderer_mono: TextRenderer::new(mono_font_path),
            cached_art_path: String::new(),
            cached_art: None,
            cached_accent: [0xFF, 0xFF, 0xFF],
            base_frame: vec![0u8; (SCREEN_WIDTH * SCREEN_HEIGHT * 4) as usize],
            base_frame_dirty: true,
            base_frame_key: None,
            cached_picker_art_path: String::new(),
            cached_picker_art: None,
            cached_screensaver_paths: Vec::new(),
            cached_screensaver_art: Vec::new(),
            screensaver_preload_index: 0,
        }
    }

    /// Load and cache the picker art if the path changed (only decodes once per scroll).
    fn ensure_picker_art_cached(&mut self, art_path: &str) {
        if art_path == self.cached_picker_art_path {
            return; // Already cached, nothing to do
        }

        self.cached_picker_art_path = art_path.to_string();

        if art_path.is_empty() {
            self.cached_picker_art = None;
            return;
        }

        match open_artwork(art_path) {
            Ok(img) => {
                self.cached_picker_art = Some(img.resize_exact(
                    ART_SIZE,
                    ART_SIZE,
                    image::imageops::FilterType::Triangle,
                ));
            }
            Err(error) => {
                eprintln!("[ART] Failed to decode picker artwork '{art_path}': {error}");
                self.cached_picker_art = None;
            }
        }
    }

    /// Load and cache the album art + accent color if the track changed.
    fn ensure_art_cached(&mut self, state: &AppState) {
        let base_frame_key = BaseFrameKey::from_state(state);
        if self.base_frame_key.as_ref() != Some(&base_frame_key) {
            self.base_frame_key = Some(base_frame_key);
            self.base_frame_dirty = true;
        }

        let art_file = &state.current_track().album_art_path;
        if art_file.is_empty() {
            // No art available
            if !self.cached_art_path.is_empty() {
                self.cached_art = None;
                self.cached_accent = [0xFF, 0xFF, 0xFF];
                self.cached_art_path.clear();
                self.base_frame_dirty = true;
            }
            return;
        }

        if self.cached_art_path == *art_file {
            return; // already cached
        }

        // album_art_path is now a full path from the Mopidy art cache
        let art_path = Path::new(art_file);
        match open_artwork(art_path) {
            Ok(img) => {
                // Cache resized art
                self.cached_art = Some(img.resize_exact(
                    ART_SIZE,
                    ART_SIZE,
                    image::imageops::FilterType::Lanczos3,
                ));
                // Cache accent color
                let tiny = img.resize_exact(1, 1, image::imageops::FilterType::Triangle);
                let pixel = tiny.get_pixel(0, 0);
                self.cached_accent = [pixel[0], pixel[1], pixel[2]];
            }
            Err(error) => {
                eprintln!(
                    "[ART] Failed to decode now-playing artwork '{}': {error}",
                    art_path.display()
                );
                self.cached_art = None;
                self.cached_accent = [0xFF, 0xFF, 0xFF];
            }
        }
        self.cached_art_path = art_file.to_string();
        self.base_frame_dirty = true; // need to rebuild the base frame
    }

    /// Rebuild the base frame: background + album art + static text (artist - album).
    /// Only called when the track changes.
    fn rebuild_base_frame(&mut self, state: &AppState) {
        let mut buffer = vec![0u8; (SCREEN_WIDTH * SCREEN_HEIGHT * 4) as usize];

        // Fill background
        self.fill_rect(
            &mut buffer,
            0,
            0,
            SCREEN_WIDTH as i32,
            SCREEN_HEIGHT as i32,
            BG_COLOR,
        );

        // Draw album art
        self.draw_album_art(&mut buffer);

        // Draw artist - album (static, doesn't change per frame)
        let title_max_width = SCREEN_WIDTH as i32 - TEXT_LEFT_MARGIN - 20;
        let subtitle = format!(
            "{} - {}",
            state.current_track().artist,
            state.current_track().album
        );
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &subtitle,
            TEXT_LEFT_MARGIN,
            50,
            title_max_width,
            FONT_SIZE_MEDIUM,
            [0x8E, 0x8E, 0x93],
        );

        self.base_frame = buffer;
        self.base_frame_dirty = false;
    }

    /// Render a complete frame into an RGBA buffer.
    /// Uses the cached base frame and only redraws dynamic elements.
    pub fn render_frame(&mut self, state: &AppState) -> Vec<u8> {
        self.preload_screensaver_art_step(&state.screensaver_art_paths);
        match state.ui_mode {
            UiMode::NowPlaying => self.render_now_playing(state),
            UiMode::PlaylistPicker => self.render_playlist_picker(state),
            UiMode::MainMenu => self.render_main_menu(state),
            UiMode::SettingEditor(setting) => self.render_setting_editor(state, setting),
            UiMode::Diagnostics => self.render_diagnostics(state),
            UiMode::GuestQr => self.render_guest_qr(),
            UiMode::Screensaver => match state.settings.screensaver_style {
                ScreensaverStyle::Albums => self.render_album_screensaver(state),
                ScreensaverStyle::Clock => self.render_clock_screensaver(state),
            },
            UiMode::Morning => self.render_morning_dashboard(state),
        }
    }

    /// Render the normal now-playing screen.
    fn render_now_playing(&mut self, state: &AppState) -> Vec<u8> {
        if state.settings.now_playing_style == NowPlayingStyle::TrackInfo {
            return self.render_track_info(state);
        }

        // Reload art only when track changes
        self.ensure_art_cached(state);

        if state.settings.now_playing_style == NowPlayingStyle::Visualizer
            && state.is_playing
            && state.visualizer_live()
        {
            return self.render_visualizer(state);
        }

        // Rebuild static base frame if needed (track changed)
        if self.base_frame_dirty {
            self.rebuild_base_frame(state);
        }

        // Start from the cached base (background + art + subtitle)
        let mut buffer = self.base_frame.clone();

        let source_color = if state.active_source == PlaybackSource::Spotifyd {
            [0x1D, 0xB9, 0x54]
        } else {
            [0x63, 0x63, 0x66]
        };
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            state.active_source.label(),
            TEXT_LEFT_MARGIN,
            6,
            9.0,
            source_color,
        );

        // Draw title (marquee) — dynamic, scrolls each frame
        let title_max_width = SCREEN_WIDTH as i32 - TEXT_LEFT_MARGIN - 20;
        self.text_renderer.draw_marquee_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &state.current_track().title,
            TEXT_LEFT_MARGIN,
            25,
            title_max_width,
            state.offset,
            FONT_SIZE_LARGE,
            [0xFF, 0xFF, 0xFF],
        );

        // Draw progress bar — dynamic, changes each frame
        self.draw_progress_bar(&mut buffer, state);

        // Draw progress stats — dynamic, changes each frame
        self.draw_progress_stats(&mut buffer, state);

        if let Some(next_track) = state.next_up_toast_track() {
            self.draw_next_up_toast(&mut buffer, state, next_track);
        }

        buffer
    }

    /// Metadata-focused now-playing layout for listeners who prefer detail to
    /// cover art or animation. It deliberately remains useful when artwork or
    /// live visualizer audio is unavailable.
    fn render_track_info(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let accent = if state.active_source == PlaybackSource::Spotifyd {
            [0x1D, 0xB9, 0x54]
        } else {
            [0x8A, 0xB4, 0xF8]
        };
        let status = if state.is_playing {
            "PLAYING"
        } else {
            "PAUSED"
        };
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            status,
            12,
            4,
            9.0,
            accent,
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            state.active_source.label(),
            330,
            4,
            9.0,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer.draw_marquee_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &state.current_track().title,
            12,
            24,
            SCREEN_WIDTH as i32 - 24,
            state.offset,
            21.0,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("ARTIST  {}", state.current_track().artist),
            12,
            52,
            SCREEN_WIDTH as i32 - 24,
            12.0,
            [0xC7, 0xCC, 0xD8],
        );
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("ALBUM   {}", state.current_track().album),
            12,
            72,
            SCREEN_WIDTH as i32 - 24,
            12.0,
            [0xC7, 0xCC, 0xD8],
        );

        let elapsed = format_time(state.current_time);
        let duration = format_time(state.current_track().duration.max(0.0));
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("{elapsed} / {duration}"),
            12,
            99,
            10.0,
            [0x8E, 0x8E, 0x93],
        );
        if let Some(next) = &state.next_track {
            self.text_renderer.draw_truncated_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &format!("NEXT  {} — {}", next.artist, next.title),
                154,
                99,
                SCREEN_WIDTH as i32 - 166,
                10.0,
                [0x8E, 0x8E, 0x93],
            );
        }
        self.fill_rounded_rect(
            &mut buffer,
            12,
            128,
            SCREEN_WIDTH as i32 - 24,
            5,
            2,
            [0x2C, 0x2C, 0x2E],
        );
        let progress = if state.current_track().duration > 0.0 {
            (state.current_time / state.current_track().duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let width = ((SCREEN_WIDTH as i32 - 24) as f64 * progress) as i32;
        if width > 0 {
            self.fill_rounded_rect(&mut buffer, 12, 128, width, 5, 2, accent);
        }
        buffer
    }

    fn render_visualizer(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let accent = match self.get_accent_color() {
            [red, green, blue] if u16::from(red) + u16::from(green) + u16::from(blue) < 100 => {
                [0x8A, 0xB4, 0xF8]
            }
            color => color,
        };

        self.text_renderer.draw_marquee_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &state.current_track().title,
            10,
            1,
            SCREEN_WIDTH as i32 - 20,
            state.offset,
            17.0,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!(
                "{} · {}",
                state.current_track().artist,
                state.current_track().album
            ),
            10,
            23,
            SCREEN_WIDTH as i32 - 20,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );

        const MARGIN: i32 = 9;
        const GAP: i32 = 3;
        const SEGMENTS: usize = 10;
        const SEGMENT_HEIGHT: i32 = 5;
        const SEGMENT_GAP: i32 = 2;
        const BASELINE: i32 = 107;
        let available = SCREEN_WIDTH as i32 - MARGIN * 2 - GAP * (BAR_COUNT as i32 - 1);
        let bar_width = available / BAR_COUNT as i32;

        for (index, level) in state.visualizer_levels.iter().enumerate() {
            let lit_segments = (level.clamp(0.0, 1.0) * SEGMENTS as f32).ceil() as usize;
            let x = MARGIN + index as i32 * (bar_width + GAP);
            for segment in 0..SEGMENTS {
                let y = BASELINE - (segment as i32 + 1) * (SEGMENT_HEIGHT + SEGMENT_GAP);
                let color = if segment < lit_segments {
                    let brightness = 0.58 + segment as f32 / (SEGMENTS - 1) as f32 * 0.42;
                    [
                        (accent[0] as f32 * brightness).min(255.0) as u8,
                        (accent[1] as f32 * brightness).min(255.0) as u8,
                        (accent[2] as f32 * brightness).min(255.0) as u8,
                    ]
                } else {
                    [0x20, 0x21, 0x24]
                };
                self.fill_rounded_rect(&mut buffer, x, y, bar_width, SEGMENT_HEIGHT, 2, color);
            }
        }

        let elapsed = format_time(state.current_time);
        let remaining = format_time((state.current_track().duration - state.current_time).max(0.0));
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &elapsed,
            10,
            112,
            9.0,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("-{remaining}"),
            385,
            112,
            9.0,
            [0x8E, 0x8E, 0x93],
        );
        self.fill_rounded_rect(
            &mut buffer,
            10,
            134,
            SCREEN_WIDTH as i32 - 20,
            3,
            1,
            [0x2C, 0x2C, 0x2E],
        );
        let progress = if state.current_track().duration > 0.0 {
            (state.current_time / state.current_track().duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let progress_width = ((SCREEN_WIDTH as i32 - 20) as f64 * progress) as i32;
        if progress_width > 0 {
            self.fill_rounded_rect(&mut buffer, 10, 134, progress_width, 3, 1, accent);
        }

        if let Some(next_track) = state.next_up_toast_track() {
            self.draw_next_up_toast(&mut buffer, state, next_track);
        }
        buffer
    }

    /// Render the playlist picker screen.
    fn render_playlist_picker(&mut self, state: &AppState) -> Vec<u8> {
        let mut buffer = vec![0u8; (SCREEN_WIDTH * SCREEN_HEIGHT * 4) as usize];

        // Dark background
        self.fill_rect(
            &mut buffer,
            0,
            0,
            SCREEN_WIDTH as i32,
            SCREEN_HEIGHT as i32,
            [0x0D, 0x0E, 0x10],
        );

        let entry = match state.current_picker_entry() {
            Some(e) => e,
            None => {
                // No playlists configured
                self.text_renderer.draw_text(
                    &mut buffer,
                    SCREEN_WIDTH,
                    SCREEN_HEIGHT,
                    "No playlists configured",
                    20,
                    60,
                    FONT_SIZE_LARGE,
                    [0x8E, 0x8E, 0x93],
                );
                return buffer;
            }
        };

        // Draw header "Select Playlist" in dim text
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Select Playlist",
            TEXT_LEFT_MARGIN,
            12,
            FONT_SIZE_SMALL,
            [0x63, 0x63, 0x66],
        );

        // Draw playlist name (large, centered area)
        let title_max_width = SCREEN_WIDTH as i32 - TEXT_LEFT_MARGIN - 20;
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &entry.name,
            TEXT_LEFT_MARGIN,
            45,
            title_max_width,
            FONT_SIZE_LARGE,
            [0xFF, 0xFF, 0xFF],
        );

        // Draw index indicator "2 / 5"
        let indicator = format!(
            "{} / {}",
            state.picker_index + 1,
            state.picker_playlists.len()
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &indicator,
            TEXT_LEFT_MARGIN,
            80,
            FONT_SIZE_MEDIUM,
            [0x8E, 0x8E, 0x93],
        );

        // Draw button hints at bottom
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B1: Next / Hold Prev",
            TEXT_LEFT_MARGIN,
            110,
            FONT_SIZE_SMALL,
            [0x48, 0x48, 0x4A],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B2: Select",
            TEXT_LEFT_MARGIN + 100,
            110,
            FONT_SIZE_SMALL,
            [0x48, 0x48, 0x4A],
        );

        // Update cached picker art if path changed
        self.ensure_picker_art_cached(&state.picker_art_path);

        // Draw playlist art on the left (or placeholder if unavailable)
        let art_y = (SCREEN_HEIGHT - ART_SIZE) / 2;
        if let Some(ref img) = self.cached_picker_art {
            // Draw cached pre-resized art with rounded corners
            let corner_radius = 8;
            for py in 0..ART_SIZE {
                for px in 0..ART_SIZE {
                    if !self.is_inside_rounded_rect(px, py, ART_SIZE, ART_SIZE, corner_radius) {
                        continue;
                    }
                    let pixel = img.get_pixel(px, py);
                    let bx = (art_y + px) as i32;
                    let by = (art_y + py) as i32;
                    if bx >= 0 && by >= 0 && bx < SCREEN_WIDTH as i32 && by < SCREEN_HEIGHT as i32 {
                        let idx = ((by as u32 * SCREEN_WIDTH + bx as u32) * 4) as usize;
                        buffer[idx] = pixel[0];
                        buffer[idx + 1] = pixel[1];
                        buffer[idx + 2] = pixel[2];
                        buffer[idx + 3] = 255;
                    }
                }
            }
        } else {
            // Placeholder: dark rounded rect with bars icon
            self.fill_rounded_rect(
                &mut buffer,
                art_y as i32,
                art_y as i32,
                ART_SIZE as i32,
                ART_SIZE as i32,
                8,
                [0x2C, 0x2C, 0x2E],
            );
            let icon_x = art_y as i32 + 30;
            let icon_y = art_y as i32 + 35;
            for i in 0..4 {
                let bar_y = icon_y + i * 12;
                let bar_width = if i % 2 == 0 { 58 } else { 45 };
                self.fill_rect(&mut buffer, icon_x, bar_y, bar_width, 4, [0x8E, 0x8E, 0x93]);
            }
        }

        buffer
    }

    fn render_main_menu(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();

        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Orpheus Menu",
            16,
            5,
            FONT_SIZE_MEDIUM,
            [0x8E, 0x8E, 0x93],
        );

        const VISIBLE_ROWS: usize = 5;
        let max_start = MAIN_MENU_ITEMS.len().saturating_sub(VISIBLE_ROWS);
        let start = state.menu_index.saturating_sub(2).min(max_start);

        for (row, item) in MAIN_MENU_ITEMS
            .iter()
            .enumerate()
            .skip(start)
            .take(VISIBLE_ROWS)
        {
            let visible_row = row - start;
            let y = 24 + visible_row as i32 * 20;
            let selected = row == state.menu_index;

            if selected {
                self.fill_rounded_rect(
                    &mut buffer,
                    12,
                    y - 3,
                    SCREEN_WIDTH as i32 - 24,
                    19,
                    4,
                    [0x2C, 0x2C, 0x2E],
                );
                self.fill_rounded_rect(&mut buffer, 12, y - 3, 4, 19, 2, self.cached_accent);
            }

            let text_color = if selected {
                [0xFF, 0xFF, 0xFF]
            } else {
                [0x8E, 0x8E, 0x93]
            };
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                item.label(),
                24,
                y,
                FONT_SIZE_MEDIUM,
                text_color,
            );

            let value = state.menu_item_value(*item);
            if !value.is_empty() {
                let value_width = self.text_renderer_mono.text_width(&value, FONT_SIZE_MEDIUM);
                self.text_renderer_mono.draw_text(
                    &mut buffer,
                    SCREEN_WIDTH,
                    SCREEN_HEIGHT,
                    &value,
                    SCREEN_WIDTH as i32 - 24 - value_width as i32,
                    y,
                    FONT_SIZE_MEDIUM,
                    text_color,
                );
            }
        }

        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B1 Move / Hold Back    B2 Select    Both Exit",
            16,
            124,
            FONT_SIZE_SMALL,
            [0x48, 0x48, 0x4A],
        );

        buffer
    }

    fn render_setting_editor(&self, state: &AppState, setting: SettingKind) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let label = AppState::setting_label(setting);
        let value = state.setting_value(setting);

        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            label,
            20,
            10,
            FONT_SIZE_MEDIUM,
            [0x8E, 0x8E, 0x93],
        );

        let value_size = 34.0;
        let value_width = self.text_renderer_mono.text_width(&value, value_size);
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &value,
            (SCREEN_WIDTH as f32 / 2.0 - value_width / 2.0) as i32,
            38,
            value_size,
            [0xFF, 0xFF, 0xFF],
        );

        let ratio = match setting {
            SettingKind::Volume => state.volume.unwrap_or(0) as f64 / 100.0,
            SettingKind::Brightness => state.settings.display_brightness,
            SettingKind::DimTimeout => {
                let timeout = state.settings.dim_timeout_seconds;
                match timeout {
                    0 => 0.0,
                    1..=5 => 0.2,
                    6..=10 => 0.4,
                    11..=30 => 0.6,
                    31..=60 => 0.8,
                    _ => 1.0,
                }
            }
            SettingKind::VisualizerDelay => state.settings.visualizer_delay_ms as f64 / 1_500.0,
            SettingKind::SpotifyVisualizerDelay => {
                state.settings.spotify_visualizer_extra_delay_ms as f64 / 1_500.0
            }
            SettingKind::CarouselSpeed => state.settings.carousel_speed / 80.0,
        };
        let bar_x = 40;
        let bar_width = SCREEN_WIDTH as i32 - 80;
        self.fill_rounded_rect(&mut buffer, bar_x, 82, bar_width, 7, 3, [0x2C, 0x2C, 0x2E]);
        let filled_width = (bar_width as f64 * ratio.clamp(0.0, 1.0)) as i32;
        if filled_width > 0 {
            self.fill_rounded_rect(
                &mut buffer,
                bar_x,
                82,
                filled_width,
                7,
                3,
                self.cached_accent,
            );
        }

        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B1 +    Hold B1 -",
            56,
            103,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B2 Done    Both Back",
            230,
            103,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );

        buffer
    }

    fn render_diagnostics(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Diagnostics",
            16,
            7,
            FONT_SIZE_MEDIUM,
            [0xFF, 0xFF, 0xFF],
        );

        let status = if state.mopidy_online {
            "Mopidy: Online"
        } else {
            "Mopidy: Offline"
        };
        let status_color = if state.mopidy_online {
            [0x5A, 0xD4, 0x78]
        } else {
            [0xE0, 0x5A, 0x5A]
        };
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            status,
            16,
            31,
            FONT_SIZE_MEDIUM,
            status_color,
        );

        let spotifyd_status = if state.spotifyd_available {
            "Spotifyd: Ready"
        } else {
            "Spotifyd: Offline"
        };
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            spotifyd_status,
            215,
            31,
            FONT_SIZE_MEDIUM,
            if state.spotifyd_available {
                [0x1D, 0xB9, 0x54]
            } else {
                [0x8E, 0x8E, 0x93]
            },
        );

        let volume = state
            .volume
            .map(|value| {
                format!(
                    "Volume: {value}% / {}%   HW: {:.1} dB",
                    state.settings.max_volume, state.settings.hardware_volume_ceiling_db
                )
            })
            .unwrap_or_else(|| "Volume: unavailable".to_string());
        let brightness = format!(
            "Display: {}%   Dim: {}",
            (state.settings.display_brightness * 100.0).round(),
            format_dim_timeout(state.settings.dim_timeout_seconds)
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &volume,
            16,
            55,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("Source: {}", state.active_source.label()),
            16,
            106,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        let visualizer = if state.visualizer_live() {
            "Visualizer: Live"
        } else {
            "Visualizer: Waiting"
        };
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            visualizer,
            215,
            55,
            FONT_SIZE_SMALL,
            if state.visualizer_live() {
                [0x5A, 0xD4, 0x78]
            } else {
                [0x8E, 0x8E, 0x93]
            },
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &brightness,
            16,
            72,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!(
                "Album covers: {}   Scroll: {:.0} px/s",
                state.screensaver_art_paths.len(),
                state.settings.carousel_speed
            ),
            16,
            89,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "B2 or Both: Back",
            16,
            121,
            FONT_SIZE_SMALL,
            [0x48, 0x48, 0x4A],
        );

        buffer
    }

    fn render_guest_qr(&self) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let Ok(qr) = QrCode::encode_text(GUEST_WEB_URL, QrCodeEcc::Medium) else {
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                "Guest QR unavailable",
                120,
                58,
                FONT_SIZE_MEDIUM,
                [0xE0, 0x5A, 0x5A],
            );
            return buffer;
        };

        const QUIET_ZONE: i32 = 4;
        const QR_MARGIN: i32 = 5;
        let module_count = qr.size() + QUIET_ZONE * 2;
        let scale = ((SCREEN_HEIGHT as i32 - QR_MARGIN * 2) / module_count).max(1);
        let qr_pixels = module_count * scale;
        let qr_x = 7;
        let qr_y = (SCREEN_HEIGHT as i32 - qr_pixels) / 2;
        self.fill_rect(
            &mut buffer,
            qr_x,
            qr_y,
            qr_pixels,
            qr_pixels,
            [0xFF, 0xFF, 0xFF],
        );
        for y in 0..qr.size() {
            for x in 0..qr.size() {
                if qr.get_module(x, y) {
                    self.fill_rect(
                        &mut buffer,
                        qr_x + (x + QUIET_ZONE) * scale,
                        qr_y + (y + QUIET_ZONE) * scale,
                        scale,
                        scale,
                        [0x00, 0x00, 0x00],
                    );
                }
            }
        }

        let text_x = qr_x + qr_pixels + 18;
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Join Orpheus",
            text_x,
            15,
            FONT_SIZE_LARGE,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "orpheus.local",
            text_x,
            49,
            17.0,
            [0x8A, 0xB4, 0xF8],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Scan to request music",
            text_x,
            78,
            FONT_SIZE_MEDIUM,
            [0xD8, 0xDE, 0xEC],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Any button: Back",
            text_x,
            116,
            FONT_SIZE_SMALL,
            [0x63, 0x63, 0x66],
        );

        buffer
    }

    fn render_album_screensaver(&mut self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let mut loaded_art: Vec<_> = state
            .screensaver_art_order
            .iter()
            .filter_map(|&index| self.cached_screensaver_art.get(index)?.as_ref())
            .collect();
        if loaded_art.is_empty() {
            loaded_art = self
                .cached_screensaver_art
                .iter()
                .filter_map(Option::as_ref)
                .collect();
        }

        if loaded_art.is_empty() {
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                "No cached album artwork",
                92,
                58,
                FONT_SIZE_MEDIUM,
                [0x63, 0x63, 0x66],
            );
            return buffer;
        }

        // A single known cover looks much better as one gently drifting hero
        // image than as a wall of identical tiles. Normally the background
        // album seeder will quickly move us into the multi-cover layout.
        if loaded_art.len() == 1 {
            const SINGLE_COVER_SIZE: i32 = 126;
            let drift_x = (state.screensaver_offset * 0.025).sin() * 8.0;
            let drift_y = (state.screensaver_offset * 0.018).cos() * 3.0;
            let x = (SCREEN_WIDTH as i32 - SINGLE_COVER_SIZE) / 2 + drift_x.round() as i32;
            let y = (SCREEN_HEIGHT as i32 - SINGLE_COVER_SIZE) / 2 + drift_y.round() as i32;
            self.draw_rounded_image(
                &mut buffer,
                loaded_art[0],
                x,
                y,
                SINGLE_COVER_SIZE as u32,
                10,
            );
            return buffer;
        }

        const COVER_SIZE: i32 = 110;
        const COVER_GAP: i32 = 12;
        const COVER_Y: i32 = 16;
        let stride = COVER_SIZE + COVER_GAP;
        let cycle_width = stride * loaded_art.len() as i32;
        let offset = (state.screensaver_offset as i32).rem_euclid(cycle_width);
        let repeats = SCREEN_WIDTH as i32 / cycle_width + 3;

        for repeat in -1..=repeats {
            for (index, image) in loaded_art.iter().enumerate() {
                let x = index as i32 * stride - offset + repeat * cycle_width;
                if x >= SCREEN_WIDTH as i32 || x + COVER_SIZE <= 0 {
                    continue;
                }
                self.draw_rounded_image(&mut buffer, image, x, COVER_Y, COVER_SIZE as u32, 8);
            }
        }

        buffer
    }

    fn render_clock_screensaver(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let (time, date) = local_clock_strings()
            .unwrap_or_else(|| ("--:--".to_string(), "Date unavailable".to_string()));

        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &time,
            16,
            25,
            48.0,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &date,
            20,
            91,
            17.0,
            [0x8E, 0x8E, 0x93],
        );
        self.fill_rounded_rect(&mut buffer, 241, 13, 2, 116, 1, [0x2C, 0x2C, 0x2E]);

        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &state.settings.weather_location,
            261,
            8,
            151,
            FONT_SIZE_MEDIUM,
            [0x8E, 0x8E, 0x93],
        );

        if let Some(weather) = &state.weather {
            let temperature = format!("{:.0}°", weather.temperature);
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &temperature,
                260,
                27,
                34.0,
                [0xFF, 0xFF, 0xFF],
            );
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &format!("Feels {:.0}°", weather.apparent_temperature),
                333,
                46,
                FONT_SIZE_SMALL,
                [0x8E, 0x8E, 0x93],
            );
            self.text_renderer.draw_truncated_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                weather.description(),
                261,
                70,
                151,
                FONT_SIZE_MEDIUM,
                [0xFF, 0xFF, 0xFF],
            );
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &format!(
                    "H {:.0}°  L {:.0}°",
                    weather.high_temperature, weather.low_temperature
                ),
                261,
                92,
                FONT_SIZE_SMALL,
                [0x8E, 0x8E, 0x93],
            );
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &format!("Wind {:.0} km/h", weather.wind_speed),
                261,
                108,
                FONT_SIZE_SMALL,
                [0x8E, 0x8E, 0x93],
            );
            let age_minutes = weather.fetched_at.elapsed().as_secs() / 60;
            let source = if age_minutes == 0 {
                "Open-Meteo · now".to_string()
            } else {
                format!("Open-Meteo · {age_minutes}m")
            };
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &source,
                261,
                125,
                9.0,
                [0x48, 0x48, 0x4A],
            );
        } else {
            let status = if state.weather_error.is_some() {
                "Weather unavailable"
            } else {
                "Loading weather..."
            };
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                status,
                261,
                57,
                FONT_SIZE_MEDIUM,
                [0x63, 0x63, 0x66],
            );
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                "Clock remains available offline",
                261,
                82,
                FONT_SIZE_SMALL,
                [0x48, 0x48, 0x4A],
            );
        }

        buffer
    }

    fn render_morning_dashboard(&self, state: &AppState) -> Vec<u8> {
        let mut buffer = self.blank_frame();
        let (time, date) =
            local_clock_strings().unwrap_or_else(|| ("--:--".to_string(), "Today".to_string()));
        let accent = [0x8A, 0xB4, 0xF8];

        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            "Good morning",
            14,
            7,
            18.0,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &date,
            146,
            11,
            170,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
        let time_width = self.text_renderer_mono.text_width(&time, 17.0);
        self.text_renderer_mono.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &time,
            SCREEN_WIDTH as i32 - 14 - time_width as i32,
            8,
            17.0,
            accent,
        );

        self.fill_rounded_rect(&mut buffer, 12, 34, 121, 68, 9, [0x18, 0x1A, 0x20]);
        self.fill_rounded_rect(&mut buffer, 143, 34, 273, 68, 9, [0x18, 0x1A, 0x20]);
        if let Some(weather) = &state.weather {
            self.text_renderer_mono.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                &format!("{:.0}°", weather.temperature),
                20,
                40,
                29.0,
                [0xFF, 0xFF, 0xFF],
            );
            self.text_renderer.draw_truncated_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                weather.description(),
                20,
                74,
                104,
                FONT_SIZE_SMALL,
                accent,
            );
        } else {
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                "--°",
                20,
                42,
                28.0,
                [0x63, 0x63, 0x66],
            );
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                if state.weather_error.is_some() {
                    "Unavailable"
                } else {
                    "Loading..."
                },
                20,
                75,
                FONT_SIZE_SMALL,
                [0x8E, 0x8E, 0x93],
            );
        }
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &state.settings.weather_location,
            20,
            89,
            104,
            9.0,
            [0x63, 0x63, 0x66],
        );

        let news_heading = state.news.as_ref().map_or_else(
            || "TODAY'S HEADLINES".to_string(),
            |news| {
                let age_minutes = news.fetched_at.elapsed().as_secs() / 60;
                if age_minutes == 0 {
                    "TODAY'S HEADLINES · NOW".to_string()
                } else {
                    format!("TODAY'S HEADLINES · {age_minutes}M")
                }
            },
        );
        self.text_renderer.draw_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &news_heading,
            153,
            40,
            9.0,
            [0x63, 0x63, 0x66],
        );
        if let Some(news) = &state.news {
            for (index, headline) in news.headlines.iter().take(3).enumerate() {
                let y = 55 + index as i32 * 15;
                self.fill_rounded_rect(&mut buffer, 153, y + 4, 4, 4, 2, accent);
                self.text_renderer.draw_truncated_text(
                    &mut buffer,
                    SCREEN_WIDTH,
                    SCREEN_HEIGHT,
                    headline,
                    164,
                    y,
                    241,
                    10.0,
                    [0xE4, 0xE7, 0xEC],
                );
            }
        } else {
            self.text_renderer.draw_text(
                &mut buffer,
                SCREEN_WIDTH,
                SCREEN_HEIGHT,
                if state.news_error.is_some() {
                    "Headlines unavailable"
                } else {
                    "Loading headlines..."
                },
                153,
                61,
                FONT_SIZE_SMALL,
                [0x8E, 0x8E, 0x93],
            );
        }

        self.fill_rounded_rect(&mut buffer, 12, 110, 404, 24, 8, [0x20, 0x27, 0x36]);
        self.text_renderer.draw_truncated_text(
            &mut buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("“{}”", state.morning_quote()),
            22,
            116,
            384,
            10.5,
            [0xCF, 0xE0, 0xFF],
        );
        buffer
    }

    fn preload_screensaver_art_step(&mut self, paths: &[String]) {
        if self.cached_screensaver_paths != paths {
            if paths.starts_with(&self.cached_screensaver_paths) {
                self.cached_screensaver_art
                    .resize_with(paths.len(), || None);
            } else {
                self.cached_screensaver_art.clear();
                self.cached_screensaver_art
                    .resize_with(paths.len(), || None);
                self.screensaver_preload_index = 0;
            }
            self.cached_screensaver_paths = paths.to_vec();
        }

        if self.screensaver_preload_index >= self.cached_screensaver_paths.len() {
            return;
        }

        let index = self.screensaver_preload_index;
        let path = &self.cached_screensaver_paths[index];
        self.cached_screensaver_art[index] = match open_artwork(path) {
            Ok(image) => Some(image.resize_exact(110, 110, image::imageops::FilterType::Triangle)),
            Err(error) => {
                eprintln!("[ART] Failed to decode carousel artwork '{path}': {error}");
                None
            }
        };
        self.screensaver_preload_index += 1;
    }

    fn draw_rounded_image(
        &self,
        buffer: &mut [u8],
        image: &DynamicImage,
        x: i32,
        y: i32,
        size: u32,
        radius: u32,
    ) {
        let (source_width, source_height) = image.dimensions();
        if size == 0 || source_width == 0 || source_height == 0 {
            return;
        }

        for py in 0..size {
            for px in 0..size {
                if !self.is_inside_rounded_rect(px, py, size, size, radius) {
                    continue;
                }
                let bx = x + px as i32;
                let by = y + py as i32;
                if bx < 0 || by < 0 || bx >= SCREEN_WIDTH as i32 || by >= SCREEN_HEIGHT as i32 {
                    continue;
                }
                let source_x = scaled_source_coordinate(px, size, source_width);
                let source_y = scaled_source_coordinate(py, size, source_height);
                let pixel = image.get_pixel(source_x, source_y);
                let index = ((by as u32 * SCREEN_WIDTH + bx as u32) * 4) as usize;
                buffer[index] = pixel[0];
                buffer[index + 1] = pixel[1];
                buffer[index + 2] = pixel[2];
                buffer[index + 3] = 255;
            }
        }
    }

    fn blank_frame(&self) -> Vec<u8> {
        let mut buffer = vec![0u8; (SCREEN_WIDTH * SCREEN_HEIGHT * 4) as usize];
        self.fill_rect(
            &mut buffer,
            0,
            0,
            SCREEN_WIDTH as i32,
            SCREEN_HEIGHT as i32,
            BG_COLOR,
        );
        buffer
    }

    /// Draw album art with rounded corners (from cache).
    fn draw_album_art(&self, buffer: &mut [u8]) {
        let border_offset = (SCREEN_HEIGHT - ART_SIZE) / 2;
        let corner_radius = 8;

        let img = match &self.cached_art {
            Some(img) => img,
            None => {
                self.fill_rounded_rect(
                    buffer,
                    border_offset as i32,
                    border_offset as i32,
                    ART_SIZE as i32,
                    ART_SIZE as i32,
                    corner_radius,
                    [0x2C, 0x2C, 0x2E],
                );
                return;
            }
        };

        // Paste image pixels with rounded corner mask
        for py in 0..ART_SIZE {
            for px in 0..ART_SIZE {
                if !self.is_inside_rounded_rect(px, py, ART_SIZE, ART_SIZE, corner_radius) {
                    continue;
                }

                let pixel = img.get_pixel(px, py);
                let bx = (border_offset + px) as i32;
                let by = (border_offset + py) as i32;

                if bx >= 0 && by >= 0 && bx < SCREEN_WIDTH as i32 && by < SCREEN_HEIGHT as i32 {
                    let idx = ((by as u32 * SCREEN_WIDTH + bx as u32) * 4) as usize;
                    buffer[idx] = pixel[0];
                    buffer[idx + 1] = pixel[1];
                    buffer[idx + 2] = pixel[2];
                    buffer[idx + 3] = 255;
                }
            }
        }
    }

    /// Draw the progress bar background and filled portion.
    fn draw_progress_bar(&self, buffer: &mut [u8], state: &AppState) {
        let bar_x_start = TEXT_LEFT_MARGIN;
        let bar_x_end = SCREEN_WIDTH as i32 - 20;
        let bar_y = 104;
        let bar_height = 4;
        let bar_radius = 2;

        // Background track
        self.fill_rounded_rect(
            buffer,
            bar_x_start,
            bar_y,
            bar_x_end - bar_x_start,
            bar_height,
            bar_radius,
            [0x2C, 0x2C, 0x2E],
        );

        // Filled portion
        let track = state.current_track();
        let progress = if track.duration > 0.0 {
            state.current_time / track.duration
        } else {
            0.0
        };
        let total_width = bar_x_end - bar_x_start;
        let filled_width = (total_width as f64 * progress) as i32;

        if filled_width > 0 {
            let accent = self.get_accent_color();
            self.fill_rounded_rect(
                buffer,
                bar_x_start,
                bar_y,
                filled_width,
                bar_height,
                bar_radius,
                accent,
            );
        }
    }

    /// Draw elapsed and remaining time.
    fn draw_progress_stats(&self, buffer: &mut [u8], state: &AppState) {
        let bar_x_start = TEXT_LEFT_MARGIN;
        let bar_x_end = SCREEN_WIDTH as i32 - 20;

        let elapsed = format_time(state.current_time);
        let remaining_secs = state.current_track().duration - state.current_time;
        let remaining = format!("-{}", format_time(remaining_secs.max(0.0)));

        // Draw elapsed (left-aligned)
        self.text_renderer_mono.draw_text(
            buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &elapsed,
            bar_x_start,
            116,
            FONT_SIZE_SMALL,
            [0x63, 0x63, 0x66],
        );

        // Draw remaining (right-aligned)
        let remaining_width = self
            .text_renderer_mono
            .text_width(&remaining, FONT_SIZE_SMALL) as i32;
        self.text_renderer_mono.draw_text(
            buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &remaining,
            bar_x_end - remaining_width,
            116,
            FONT_SIZE_SMALL,
            [0x63, 0x63, 0x66],
        );
    }

    fn draw_next_up_toast(&self, buffer: &mut [u8], state: &AppState, next_track: &Track) {
        const TOAST_X: i32 = 220;
        const TOAST_Y: i32 = 76;
        const TOAST_WIDTH: i32 = 200;
        const TOAST_HEIGHT: i32 = 59;

        self.fill_rounded_rect(
            buffer,
            TOAST_X - 2,
            TOAST_Y - 2,
            TOAST_WIDTH + 2,
            TOAST_HEIGHT + 2,
            7,
            [0x08, 0x09, 0x0A],
        );
        self.fill_rounded_rect(
            buffer,
            TOAST_X,
            TOAST_Y,
            TOAST_WIDTH,
            TOAST_HEIGHT,
            6,
            [0x20, 0x21, 0x24],
        );
        self.fill_rounded_rect(
            buffer,
            TOAST_X,
            TOAST_Y,
            4,
            TOAST_HEIGHT,
            2,
            self.get_accent_color(),
        );

        let remaining = (state.current_track().duration - state.current_time)
            .ceil()
            .clamp(1.0, 15.0) as u8;
        self.text_renderer_mono.draw_text(
            buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &format!("NEXT UP  {remaining}s"),
            TOAST_X + 13,
            TOAST_Y + 6,
            9.0,
            self.get_accent_color(),
        );
        self.text_renderer.draw_truncated_text(
            buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &next_track.title,
            TOAST_X + 13,
            TOAST_Y + 21,
            TOAST_WIDTH - 25,
            FONT_SIZE_MEDIUM,
            [0xFF, 0xFF, 0xFF],
        );
        self.text_renderer.draw_truncated_text(
            buffer,
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &next_track.artist,
            TOAST_X + 13,
            TOAST_Y + 40,
            TOAST_WIDTH - 25,
            FONT_SIZE_SMALL,
            [0x8E, 0x8E, 0x93],
        );
    }

    /// Get the cached accent color.
    fn get_accent_color(&self) -> [u8; 3] {
        self.cached_accent
    }

    /// Fill a rectangle with a solid color.
    fn fill_rect(&self, buffer: &mut [u8], x: i32, y: i32, w: i32, h: i32, color: [u8; 3]) {
        for py in y..(y + h) {
            for px in x..(x + w) {
                if px >= 0 && py >= 0 && px < SCREEN_WIDTH as i32 && py < SCREEN_HEIGHT as i32 {
                    let idx = ((py as u32 * SCREEN_WIDTH + px as u32) * 4) as usize;
                    buffer[idx] = color[0];
                    buffer[idx + 1] = color[1];
                    buffer[idx + 2] = color[2];
                    buffer[idx + 3] = 255;
                }
            }
        }
    }

    /// Fill a rounded rectangle.
    #[allow(clippy::too_many_arguments)]
    fn fill_rounded_rect(
        &self,
        buffer: &mut [u8],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: u32,
        color: [u8; 3],
    ) {
        for py in 0..h {
            for px in 0..w {
                if self.is_inside_rounded_rect(px as u32, py as u32, w as u32, h as u32, radius) {
                    let bx = x + px;
                    let by = y + py;
                    if bx >= 0 && by >= 0 && bx < SCREEN_WIDTH as i32 && by < SCREEN_HEIGHT as i32 {
                        let idx = ((by as u32 * SCREEN_WIDTH + bx as u32) * 4) as usize;
                        buffer[idx] = color[0];
                        buffer[idx + 1] = color[1];
                        buffer[idx + 2] = color[2];
                        buffer[idx + 3] = 255;
                    }
                }
            }
        }
    }

    /// Check if a point is inside a rounded rectangle (for masking).
    fn is_inside_rounded_rect(&self, px: u32, py: u32, w: u32, h: u32, radius: u32) -> bool {
        let r = radius as f32;
        let x = px as f32;
        let y = py as f32;
        let w = w as f32;
        let h = h as f32;

        // Check four corners
        // Top-left
        if x < r && y < r {
            let dx = x - r;
            let dy = y - r;
            return dx * dx + dy * dy <= r * r;
        }
        // Top-right
        if x >= w - r && y < r {
            let dx = x - (w - r);
            let dy = y - r;
            return dx * dx + dy * dy <= r * r;
        }
        // Bottom-left
        if x < r && y >= h - r {
            let dx = x - r;
            let dy = y - (h - r);
            return dx * dx + dy * dy <= r * r;
        }
        // Bottom-right
        if x >= w - r && y >= h - r {
            let dx = x - (w - r);
            let dy = y - (h - r);
            return dx * dx + dy * dy <= r * r;
        }

        true
    }

    /// Convert RGBA buffer to u32 buffer for minifb (0xAARRGGBB format).
    #[cfg(feature = "desktop")]
    pub fn rgba_to_u32(buffer: &[u8]) -> Vec<u32> {
        buffer
            .chunks_exact(4)
            .map(|px| {
                let r = px[0] as u32;
                let g = px[1] as u32;
                let b = px[2] as u32;
                (r << 16) | (g << 8) | b
            })
            .collect()
    }
}

fn local_clock_strings() -> Option<(String, String)> {
    let timestamp = unsafe { libc::time(std::ptr::null_mut()) };
    if timestamp < 0 {
        return None;
    }
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `timestamp` and `local` are valid pointers for the duration of
    // this call, and `localtime_r` initializes `local` before returning it.
    let result = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    // SAFETY: a non-null result from `localtime_r` means the struct was filled.
    let local = unsafe { local.assume_init() };

    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = WEEKDAYS.get(local.tm_wday as usize)?;
    let month = MONTHS.get(local.tm_mon as usize)?;

    Some((
        format!("{:02}:{:02}", local.tm_hour, local.tm_min),
        format!("{weekday} {} {month}", local.tm_mday),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    #[test]
    fn base_frame_key_changes_when_metadata_changes_with_same_art() {
        let first_track = Track {
            title: "First".into(),
            artist: "Artist A".into(),
            album: "Shared compilation".into(),
            duration: 10.0,
            album_art_path: "shared.jpg".into(),
        };
        let second_track = Track {
            title: "Second".into(),
            artist: "Artist B".into(),
            album: "Shared compilation".into(),
            duration: 10.0,
            album_art_path: "shared.jpg".into(),
        };

        assert_ne!(
            BaseFrameKey::from_track(&first_track),
            BaseFrameKey::from_track(&second_track)
        );
    }

    #[test]
    fn base_frame_key_changes_for_artless_album_updates() {
        let first_track = Track {
            title: "First".into(),
            artist: "Artist".into(),
            album: "Album A".into(),
            duration: 10.0,
            album_art_path: String::new(),
        };
        let second_track = Track {
            title: "Second".into(),
            artist: "Artist".into(),
            album: "Album B".into(),
            duration: 10.0,
            album_art_path: String::new(),
        };

        assert_ne!(
            BaseFrameKey::from_track(&first_track),
            BaseFrameKey::from_track(&second_track)
        );
    }

    #[test]
    fn local_clock_uses_fixed_width_time_and_a_date() {
        let (time, date) = local_clock_strings().unwrap();

        assert_eq!(time.len(), 5);
        assert_eq!(&time[2..3], ":");
        assert!(!date.is_empty());
    }

    #[test]
    fn guest_url_produces_a_scannable_qr_that_fits_the_display() {
        let qr = QrCode::encode_text(GUEST_WEB_URL, QrCodeEcc::Medium).unwrap();
        let modules_with_quiet_zone = qr.size() + 8;
        let scale = (SCREEN_HEIGHT as i32 - 10) / modules_with_quiet_zone;

        assert_eq!(GUEST_WEB_URL, "http://orpheus.local/");
        assert!(scale >= 3);
        assert!(modules_with_quiet_zone * scale <= SCREEN_HEIGHT as i32 - 10);
        assert!(qr.get_module(0, 0));
    }

    #[test]
    fn artwork_loader_uses_file_signature_when_extension_is_wrong() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-png-disguised-as-jpeg-{}-{unique}.jpg",
            std::process::id()
        ));
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([220, 20, 60, 255])));
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, ImageFormat::Png).unwrap();
        std::fs::write(&path, encoded.into_inner()).unwrap();

        let decoded = open_artwork(&path).unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.get_pixel(0, 0), Rgba([220, 20, 60, 255]));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cached_carousel_art_can_be_rendered_larger_without_exceeding_its_bounds() {
        assert_eq!(scaled_source_coordinate(0, 126, 110), 0);
        assert_eq!(scaled_source_coordinate(109, 126, 110), 95);
        assert_eq!(scaled_source_coordinate(125, 126, 110), 109);
    }
}
