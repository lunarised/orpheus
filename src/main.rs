mod audio_visualizer;
mod buttons;
mod config;
#[cfg(feature = "hardware")]
mod display;
mod history;
mod mopidy;
mod news;
mod playlists;
mod renderer;
mod spotifyd;
mod state;
mod text;
mod weather;
mod web_config;

#[cfg(feature = "hardware")]
use buttons::ButtonAction;
use buttons::{Button, ButtonHandler, apply_action};
use config::{Config, Settings};
use mopidy::MopidyClient;
use renderer::{Renderer, SCREEN_HEIGHT, SCREEN_WIDTH};
use state::{AppState, UiMode};
use std::time::Instant;

#[cfg(feature = "hardware")]
use std::thread;

#[cfg(feature = "desktop")]
use minifb::{Key, Window, WindowOptions};

const FPS: u64 = 30;

// Font paths
// Noto Sans CJK (bundled in project) — index 0 = Japanese variant (covers Latin + CJK)
const FONT_PATH: &str = "fonts/NotoSansCJK-Regular.ttc";
const MONO_FONT_PATH: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";

fn main() {
    // Mopidy connection settings (default: localhost:6680)
    let mopidy_host = std::env::var("MOPIDY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mopidy_port: u16 = std::env::var("MOPIDY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6680);

    // Project root directory (for fonts, config, etc.)
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // Default: look for images in the parent project directory
    let assets_dir = std::env::var("ORPHEUS_ASSETS").unwrap_or_else(|_| {
        std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string()
    });

    // Album art cache directory (tmpfs — avoids SD card write fatigue)
    // Keep album artwork separate from playlist thumbnails. The album carousel
    // scans only this directory, so playlist icons can never leak into it.
    let art_cache_dir = "/tmp/orpheus_art/albums".to_string();

    println!("Loading assets from: {}", assets_dir);
    println!("Connecting to Mopidy at {}:{}", mopidy_host, mopidy_port);

    // Initialize Mopidy client
    let queue_state_path = std::path::PathBuf::from(manifest_dir).join("queue-state.toml");
    let mopidy = MopidyClient::new(&mopidy_host, mopidy_port, &art_cache_dir, queue_state_path);
    let mopidy_library = mopidy.library_client();

    // Load playlist config
    let config_path = std::path::PathBuf::from(manifest_dir).join("playlists.toml");
    let config = Config::load(&config_path.to_string_lossy());
    let settings_path = std::path::PathBuf::from(manifest_dir).join("settings.toml");
    let mut settings = Settings::load(&settings_path.to_string_lossy());
    match settings.ensure_web_settings_password(&settings_path) {
        Ok(true) => println!(
            "[AUTH] Generated an initial configuration password in {}",
            settings_path.display()
        ),
        Ok(false) => {}
        Err(error) => eprintln!("[AUTH] Could not initialize configuration password: {error}"),
    }
    let web_bind =
        std::env::var("ORPHEUS_WEB_BIND").unwrap_or_else(|_| "127.0.0.1:6681".to_string());
    let web_config = web_config::spawn(
        &web_bind,
        settings_path.clone(),
        config_path,
        mopidy_library,
    );
    let visualizer_bind =
        std::env::var("ORPHEUS_VISUALIZER_BIND").unwrap_or_else(|_| "127.0.0.1:5568".to_string());
    let visualizer_updates = audio_visualizer::spawn_worker(&visualizer_bind);

    // Resolve font path (bundled in project)
    let font_path = format!("{}/{}", manifest_dir, FONT_PATH);

    // Initialize renderer with fonts
    let mut renderer = Renderer::new(&font_path, MONO_FONT_PATH, &assets_dir);

    // Initialize state (polls Mopidy immediately)
    let mut state = AppState::new(
        mopidy,
        config,
        settings,
        settings_path,
        web_config,
        visualizer_updates,
    );
    let mut button_handler = ButtonHandler::new();

    // Save a static preview PNG
    {
        let frame = renderer.render_frame(&state);
        let preview_path = format!("{}/orpheus-preview.png", assets_dir);
        match save_png(&frame, SCREEN_WIDTH, SCREEN_HEIGHT, &preview_path) {
            Ok(()) => println!("Saved orpheus-preview.png"),
            Err(error) => eprintln!("[PREVIEW] Could not save optional preview: {error}"),
        }
    }

    #[cfg(feature = "hardware")]
    run_hardware(&mut renderer, &mut state, &mut button_handler);

    #[cfg(feature = "desktop")]
    run_desktop(&mut renderer, &mut state, &mut button_handler);
}

// ---------- Hardware display loop (Pi 5) ----------

#[cfg(feature = "hardware")]
fn run_hardware(renderer: &mut Renderer, state: &mut AppState, button_handler: &mut ButtonHandler) {
    use display::HardwareDisplay;
    use rppal::gpio::{Gpio, Level};
    use std::time::Duration;

    let mut hw = HardwareDisplay::new().expect("Failed to initialize NV3007 display");
    println!("NV3007 display ready — entering render loop");

    // Setup GPIO buttons with internal pull-ups (active low)
    let gpio = Gpio::new().expect("Failed to init GPIO for buttons");
    let btn1_pin = gpio
        .get(17)
        .expect("Failed to get GPIO 17")
        .into_input_pullup();
    let btn2_pin = gpio
        .get(22)
        .expect("Failed to get GPIO 22")
        .into_input_pullup();
    println!("Buttons: GPIO 17 (pin 11), GPIO 22 (pin 15) — active low");

    let mut btn1_was_low = false;
    let mut btn2_was_low = false;

    let frame_duration = Duration::from_micros(1_000_000 / FPS);
    let mut last_frame = Instant::now();
    let mut was_blocked = false;

    loop {
        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f64();
        last_frame = now;

        let t0 = Instant::now();
        state.advance_time(dt);
        let t_advance = t0.elapsed();

        let t1 = Instant::now();
        let frame_rgba = renderer.render_frame(state);
        let t_render = t1.elapsed();

        // Update hardware backlight brightness
        hw.set_brightness(state.brightness);

        let t2 = Instant::now();
        hw.push_frame(&frame_rgba)
            .expect("Failed to push frame to display");
        let t_push = t2.elapsed();

        let total_blocked = t0.elapsed();
        if total_blocked > Duration::from_millis(100) {
            if !was_blocked {
                println!(
                    "[PERF] Buttons BLOCKED — advance={:?} render={:?} push={:?} total={:?}",
                    t_advance, t_render, t_push, total_blocked
                );
                was_blocked = true;
            }
        } else if was_blocked {
            println!(
                "[PERF] Buttons responsive again (frame took {:?})",
                total_blocked
            );
            was_blocked = false;
        }

        // --- GPIO button handling ---
        let btn1_low = btn1_pin.read() == Level::Low;
        let btn2_low = btn2_pin.read() == Level::Low;
        let btn1_pressed = btn1_low && !btn1_was_low;
        let btn2_pressed = btn2_low && !btn2_was_low;
        let woke_screensaver =
            state.ui_mode == UiMode::Screensaver && (btn1_pressed || btn2_pressed);

        if woke_screensaver {
            state.wake_screensaver();
            button_handler.cancel_all();
        }

        // Detect press (transition high → low)
        if btn1_pressed && !woke_screensaver {
            println!("[BTN] Btn1 PRESSED (mode={:?})", state.ui_mode);
            button_handler.on_press(Button::Btn1);
        }
        if btn2_pressed && !woke_screensaver {
            println!("[BTN] Btn2 PRESSED (mode={:?})", state.ui_mode);
            button_handler.on_press(Button::Btn2);
        }

        // Detect release (transition low → high)
        if !btn1_low && btn1_was_low {
            let action = button_handler.on_release(Button::Btn1);
            println!(
                "[BTN] Btn1 RELEASED -> {:?} (mode={:?})",
                action, state.ui_mode
            );
            apply_action(action, state);
        }
        if !btn2_low && btn2_was_low {
            let action = button_handler.on_release(Button::Btn2);
            println!(
                "[BTN] Btn2 RELEASED -> {:?} (mode={:?})",
                action, state.ui_mode
            );
            apply_action(action, state);
        }

        btn1_was_low = btn1_low;
        btn2_was_low = btn2_low;

        // Check for both-buttons held combo
        let both_action = button_handler.check_both_held();
        if both_action != ButtonAction::None {
            println!(
                "[BTN] Both held -> {:?} (mode={:?})",
                both_action, state.ui_mode
            );
        }
        apply_action(both_action, state);

        // Frame rate limiting
        let elapsed = last_frame.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }
}

// ---------- Desktop preview loop (minifb window) ----------

#[cfg(feature = "desktop")]
fn run_desktop(renderer: &mut Renderer, state: &mut AppState, button_handler: &mut ButtonHandler) {
    let mut window = Window::new(
        "Orpheus Preview (Rust)",
        SCREEN_WIDTH as usize,
        SCREEN_HEIGHT as usize,
        WindowOptions::default(),
    )
    .expect("Failed to create window");

    // Limit update rate
    window.set_target_fps(FPS as usize);

    let mut last_frame = Instant::now();

    // Track key states for press/release detection
    let mut key1_was_down = false;
    let mut key2_was_down = false;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f64();
        last_frame = now;

        // --- Advance state ---
        state.advance_time(dt);

        // --- Render ---
        let frame_rgba = renderer.render_frame(state);
        let frame_u32 = Renderer::rgba_to_u32(&frame_rgba);

        window
            .update_with_buffer(&frame_u32, SCREEN_WIDTH as usize, SCREEN_HEIGHT as usize)
            .expect("Failed to update window");

        // --- Button input handling (after update so key states are fresh) ---
        let key1_down = window.is_key_down(Key::Key1);
        let key2_down = window.is_key_down(Key::Key2);
        let key1_pressed = key1_down && !key1_was_down;
        let key2_pressed = key2_down && !key2_was_down;
        let woke_screensaver =
            state.ui_mode == UiMode::Screensaver && (key1_pressed || key2_pressed);

        if woke_screensaver {
            state.wake_screensaver();
            button_handler.cancel_all();
        }

        // Detect press (transition from up to down)
        if key1_pressed && !woke_screensaver {
            button_handler.on_press(Button::Btn1);
        }
        if key2_pressed && !woke_screensaver {
            button_handler.on_press(Button::Btn2);
        }

        // Detect release (transition from down to up)
        if !key1_down && key1_was_down {
            let action = button_handler.on_release(Button::Btn1);
            apply_action(action, state);
        }
        if !key2_down && key2_was_down {
            let action = button_handler.on_release(Button::Btn2);
            apply_action(action, state);
        }

        key1_was_down = key1_down;
        key2_was_down = key2_down;

        // Check for both-buttons held combo
        let both_action = button_handler.check_both_held();
        apply_action(both_action, state);
    }
}

/// Save an RGBA buffer as a PNG file.
fn save_png(buffer: &[u8], width: u32, height: u32, path: &str) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(width, height, buffer.to_vec())
        .ok_or_else(|| "rendered buffer has the wrong dimensions".to_string())?;
    img.save(path)
        .map_err(|error| format!("could not write '{path}': {error}"))
}
