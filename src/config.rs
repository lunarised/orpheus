use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreensaverStyle {
    #[default]
    Albums,
    Clock,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NowPlayingStyle {
    #[default]
    Artwork,
    Visualizer,
    TrackInfo,
}

impl NowPlayingStyle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Artwork => "Artwork",
            Self::Visualizer => "Visualizer",
            Self::TrackInfo => "Track info",
        }
    }
}

impl ScreensaverStyle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Albums => "Albums",
            Self::Clock => "Clock",
        }
    }
}

/// A saved playlist entry from the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistEntry {
    /// Display name for the playlist
    pub name: String,
    /// Mopidy URI (e.g. "spotify:playlist:xxx" or "jellyfin://playlist/xxx")
    pub uri: String,
    /// Optional art image URI (will be fetched from Mopidy if not provided)
    pub art_uri: Option<String>,
}

/// Top-level config structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub playlists: Vec<PlaylistEntry>,
}

impl Config {
    /// Load config from a TOML file. Returns default config if file doesn't exist.
    pub fn load(path: &str) -> Self {
        let config_path = Path::new(path);
        if !config_path.exists() {
            println!("Config file not found at '{}', using empty config", path);
            return Self::default();
        }

        match std::fs::read_to_string(config_path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(config) => {
                    println!("Loaded {} playlists from config", config.playlists.len());
                    config
                }
                Err(e) => {
                    eprintln!("Failed to parse config '{}': {}", path, e);
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!("Failed to read config '{}': {}", path, e);
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let serialized = toml::to_string_pretty(self)
            .map_err(|error| format!("failed to serialize playlists: {error}"))?;
        atomic_write(path, &serialized)
    }
}

/// User-adjustable settings persisted separately from playlist definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Normal display backlight level, from 0.1 to 1.0.
    pub display_brightness: f64,
    /// Seconds of inactivity before dimming. Zero disables automatic dimming.
    pub dim_timeout_seconds: u64,
    /// Backlight level while dimmed, from 0.0 to 1.0.
    pub dim_brightness: f64,
    /// Show cached album artwork instead of only dimming while idle.
    pub screensaver_enabled: bool,
    /// Use the heavy dim level while the album-art screensaver is active.
    pub screensaver_heavy_dim: bool,
    /// Visual shown while the screensaver is active.
    pub screensaver_style: ScreensaverStyle,
    /// Backlight level while the album-art screensaver is active.
    pub screensaver_brightness: f64,
    /// Randomize the cached album order whenever the screensaver starts.
    pub shuffle_screensaver_art: bool,
    /// Album-carousel movement speed in pixels per second. Zero pauses it.
    pub carousel_speed: f64,
    /// Shuffle playlist tracks before starting playback.
    pub shuffle_playlists: bool,
    /// Percentage-point change made by each volume adjustment.
    pub volume_step: u8,
    /// Shared volume mirrored to every playback backend.
    pub playback_volume: u8,
    /// Highest volume either playback backend may accept.
    pub max_volume: u8,
    /// Final HiFiBerry Digital mixer ceiling in decibels.
    pub hardware_volume_ceiling_db: f64,
    /// Layout used while a track is playing.
    pub now_playing_style: NowPlayingStyle,
    /// Delay applied to visualizer frames so they can be aligned by ear.
    pub visualizer_delay_ms: u64,
    /// Additional visualizer delay used only for Spotify Connect playback.
    pub spotify_visualizer_extra_delay_ms: u64,
    /// Label shown with the weather forecast.
    pub weather_location: String,
    /// Latitude used for weather forecasts.
    pub weather_latitude: f64,
    /// Longitude used for weather forecasts.
    pub weather_longitude: f64,
    /// Show the daily morning dashboard during its configured time window.
    pub morning_mode_enabled: bool,
    /// Local hour at which the morning dashboard becomes available.
    pub morning_start_hour: u8,
    /// Local hour at which the morning dashboard closes.
    pub morning_end_hour: u8,
    /// RSS feed used for morning news headlines. Empty disables news fetching.
    pub morning_news_feed_url: String,
    /// Password used to enter the configuration page. Never rendered back to browsers.
    pub web_settings_password: String,
    /// Restrict unauthenticated browsers to rate-limited queue additions.
    pub guest_mode_enabled: bool,
    /// Minimum wait between guest queue additions from one client.
    pub guest_queue_cooldown_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display_brightness: 1.0,
            dim_timeout_seconds: 10,
            dim_brightness: 0.01,
            screensaver_enabled: true,
            screensaver_heavy_dim: false,
            screensaver_style: ScreensaverStyle::Albums,
            screensaver_brightness: 0.25,
            shuffle_screensaver_art: true,
            carousel_speed: 28.0,
            shuffle_playlists: true,
            volume_step: 5,
            playback_volume: 50,
            // Spotify Connect applies phone-originated volume changes before
            // external observers can react. Keep its native range intact and
            // use the downstream hardware ceiling for instantaneous safety.
            max_volume: 100,
            hardware_volume_ceiling_db: -7.0,
            now_playing_style: NowPlayingStyle::Artwork,
            visualizer_delay_ms: 0,
            spotify_visualizer_extra_delay_ms: 0,
            weather_location: "Location not configured".to_string(),
            weather_latitude: 0.0,
            weather_longitude: 0.0,
            morning_mode_enabled: false,
            morning_start_hour: 7,
            morning_end_hour: 10,
            morning_news_feed_url: String::new(),
            web_settings_password: String::new(),
            guest_mode_enabled: false,
            guest_queue_cooldown_seconds: 120,
        }
    }
}

impl Settings {
    pub fn load(path: &str) -> Self {
        let settings_path = Path::new(path);
        if !settings_path.exists() {
            println!("Settings file not found at '{}', using defaults", path);
            return Self::default();
        }

        match std::fs::read_to_string(settings_path) {
            Ok(contents) => match toml::from_str::<Self>(&contents) {
                Ok(mut settings) => {
                    settings.normalize();
                    println!("Loaded settings from {}", path);
                    settings
                }
                Err(e) => {
                    eprintln!("Failed to parse settings '{}': {}", path, e);
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!("Failed to read settings '{}': {}", path, e);
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let serialized = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize settings: {e}"))?;
        atomic_write(path, &serialized)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    format!(
                        "failed to protect settings file '{}': {error}",
                        path.display()
                    )
                },
            )?;
        }
        Ok(())
    }

    /// Generate a strong first-run web password and persist it locally.
    pub fn ensure_web_settings_password(&mut self, path: &Path) -> Result<bool, String> {
        if !self.web_settings_password.trim().is_empty() {
            return Ok(false);
        }
        self.web_settings_password = generate_password()?;
        self.save(path)?;
        Ok(true)
    }

    pub fn normalize(&mut self) {
        self.display_brightness = self.display_brightness.clamp(0.1, 1.0);
        self.dim_brightness = self.dim_brightness.clamp(0.0, self.display_brightness);
        self.screensaver_brightness = self
            .screensaver_brightness
            .clamp(0.05, self.display_brightness);
        self.volume_step = self.volume_step.clamp(1, 25);
        self.max_volume = self.max_volume.clamp(10, 100);
        self.playback_volume = self.playback_volume.min(self.max_volume);
        if !self.hardware_volume_ceiling_db.is_finite() {
            self.hardware_volume_ceiling_db = Self::default().hardware_volume_ceiling_db;
        }
        self.hardware_volume_ceiling_db = self.hardware_volume_ceiling_db.clamp(-40.0, 0.0);
        self.visualizer_delay_ms = self.visualizer_delay_ms.min(1_500);
        self.spotify_visualizer_extra_delay_ms = self.spotify_visualizer_extra_delay_ms.min(1_500);
        self.guest_queue_cooldown_seconds = self.guest_queue_cooldown_seconds.clamp(30, 3_600);
        if !self.carousel_speed.is_finite() {
            self.carousel_speed = Self::default().carousel_speed;
        }
        self.carousel_speed = self.carousel_speed.clamp(0.0, 80.0);
        if self.weather_location.trim().is_empty() {
            self.weather_location = "Local".to_string();
        }
        if !self.weather_latitude.is_finite() {
            self.weather_latitude = Self::default().weather_latitude;
        }
        if !self.weather_longitude.is_finite() {
            self.weather_longitude = Self::default().weather_longitude;
        }
        self.weather_latitude = self.weather_latitude.clamp(-90.0, 90.0);
        self.weather_longitude = self.weather_longitude.clamp(-180.0, 180.0);
        self.morning_start_hour = self.morning_start_hour.min(23);
        self.morning_end_hour = self.morning_end_hour.clamp(1, 24);
        if self.morning_start_hour >= self.morning_end_hour {
            self.morning_start_hour = Self::default().morning_start_hour;
            self.morning_end_hour = Self::default().morning_end_hour;
        }
        self.morning_news_feed_url = self.morning_news_feed_url.trim().to_string();
        self.web_settings_password = self.web_settings_password.trim().to_string();
    }
}

fn generate_password() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut random = [0_u8; 18];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("could not generate settings password: {error}"))?;
    Ok(random
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect())
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("orpheus-config");
    let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(".{file_name}.{}.{nonce}.tmp", std::process::id()));
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("failed to write '{}': {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "failed to replace configuration '{}': {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_settings_gain_a_safe_carousel_speed_default() {
        let settings: Settings = toml::from_str("display_brightness = 0.8\n").unwrap();

        assert_eq!(settings.display_brightness, 0.8);
        assert_eq!(settings.carousel_speed, 28.0);
        assert_eq!(settings.playback_volume, 50);
        assert_eq!(settings.max_volume, 100);
        assert_eq!(settings.hardware_volume_ceiling_db, -7.0);
        assert_eq!(settings.spotify_visualizer_extra_delay_ms, 0);
        assert!(!settings.guest_mode_enabled);
        assert_eq!(settings.guest_queue_cooldown_seconds, 120);
    }

    #[test]
    fn carousel_speed_is_normalized_to_the_supported_range() {
        let mut settings = Settings {
            carousel_speed: 500.0,
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.carousel_speed, 80.0);

        settings.carousel_speed = f64::NAN;
        settings.normalize();
        assert_eq!(settings.carousel_speed, 28.0);
    }

    #[test]
    fn spotify_visualizer_offset_is_bounded() {
        let mut settings = Settings {
            spotify_visualizer_extra_delay_ms: 9_000,
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.spotify_visualizer_extra_delay_ms, 1_500);
    }

    #[test]
    fn guest_queue_cooldown_is_kept_within_household_safe_bounds() {
        let mut settings = Settings {
            guest_queue_cooldown_seconds: 1,
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.guest_queue_cooldown_seconds, 30);

        settings.guest_queue_cooldown_seconds = 10_000;
        settings.normalize();
        assert_eq!(settings.guest_queue_cooldown_seconds, 3_600);
    }

    #[test]
    fn volume_safety_settings_are_normalized() {
        let mut settings = Settings {
            playback_volume: 95,
            max_volume: 60,
            hardware_volume_ceiling_db: 4.0,
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.playback_volume, 60);
        assert_eq!(settings.max_volume, 60);
        assert_eq!(settings.hardware_volume_ceiling_db, 0.0);

        settings.max_volume = 1;
        settings.hardware_volume_ceiling_db = f64::NAN;
        settings.normalize();
        assert_eq!(settings.max_volume, 10);
        assert_eq!(settings.playback_volume, 10);
        assert_eq!(settings.hardware_volume_ceiling_db, -7.0);
    }

    #[test]
    fn invalid_morning_window_is_restored_to_safe_defaults() {
        let mut settings = Settings {
            morning_start_hour: 18,
            morning_end_hour: 8,
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.morning_start_hour, 7);
        assert_eq!(settings.morning_end_hour, 10);
    }

    #[test]
    fn generated_web_password_is_long_and_easy_to_type() {
        let password = generate_password().unwrap();
        assert_eq!(password.len(), 18);
        assert!(
            password
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
    }
}
