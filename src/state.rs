use crate::audio_visualizer::{BAR_COUNT, SpectrumFrame};
use crate::config::{Config, NowPlayingStyle, PlaylistEntry, ScreensaverStyle, Settings};
use crate::history::{HistoryEntry, HistoryStore};
use crate::mopidy::{MopidyClient, MopidyCommand, MopidySnapshot, PlaybackState};
use crate::news::{NewsData, NewsUpdate};
use crate::playlists::Track;
use crate::sleep_timer::SleepTimer;
use crate::spotifyd::{SpotifydCommand, SpotifydPlaybackState, SpotifydSnapshot, SpotifydUpdate};
use crate::weather::{WeatherData, WeatherUpdate};
use crate::web_config::{
    WebConfigBridge, WebConfigUpdate, WebPlaybackAction, WebPlaybackStatus, WebPlaylist,
    WebQueueAction,
};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often to poll Mopidy for state updates (in seconds).
const POLL_INTERVAL: f64 = 0.2;
const OFFLINE_POLL_INTERVAL: f64 = 2.0;
const DIM_TIMEOUT_OPTIONS: [u64; 6] = [0, 5, 10, 30, 60, 300];
const MAX_SCREENSAVER_ART: usize = 64;
const CAROUSEL_PRELOAD_TARGET: usize = 12;
const CAROUSEL_SPEED_STEP: f64 = 5.0;
const MAX_CAROUSEL_SPEED: f64 = 80.0;
const VISUALIZER_DELAY_STEP_MS: u64 = 50;
const MAX_VISUALIZER_DELAY_MS: u64 = 1_500;
const MAX_PENDING_VISUALIZER_FRAMES: usize = 128;
const WEB_PLAYLIST_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const VOLUME_COMMAND_GRACE: Duration = Duration::from_secs(2);
const VOLUME_SAVE_DELAY: Duration = Duration::from_secs(1);
const HARDWARE_MIXER_CARD: &str = "sndrpihifiberry";
const HARDWARE_MIXER_CONTROL: &str = "Digital";
pub const GUEST_WEB_URL: &str = "http://orpheus.local/";
const MORNING_QUOTES: [&str; 14] = [
    "Small steps still move you forward.",
    "You are allowed to begin again today.",
    "Do the next kind and useful thing.",
    "Progress matters more than perfection.",
    "Make today steady, gentle, and yours.",
    "Your best can be quiet and still be enough.",
    "Give yourself the patience you give others.",
    "A difficult day cannot erase your progress.",
    "Keep going; you are building something good.",
    "There is strength in taking your time.",
    "Start where you are and use what you have.",
    "You can meet today one moment at a time.",
    "Leave room for something good to happen.",
    "Be proud of every honest effort you make.",
];

fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn mopidy_poll_interval(online: bool) -> f64 {
    if online {
        POLL_INTERVAL
    } else {
        OFFLINE_POLL_INTERVAL
    }
}

fn spotify_playback_started(
    previous: Option<SpotifydPlaybackState>,
    current: SpotifydPlaybackState,
) -> bool {
    current == SpotifydPlaybackState::Playing && previous != Some(SpotifydPlaybackState::Playing)
}

fn local_day_and_hour() -> Option<(i32, u8)> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        return None;
    }
    let day = local
        .tm_year
        .saturating_mul(400)
        .saturating_add(local.tm_yday);
    Some((day, u8::try_from(local.tm_hour).ok()?))
}

fn take_ready_visualizer_frame(
    pending: &mut VecDeque<(Instant, SpectrumFrame)>,
    now: Instant,
    delay: std::time::Duration,
) -> Option<SpectrumFrame> {
    let cutoff = now.checked_sub(delay).unwrap_or(now);
    let mut latest = None;
    while pending
        .front()
        .is_some_and(|(captured_at, _)| *captured_at <= cutoff)
    {
        latest = pending.pop_front().map(|(_, frame)| frame);
    }
    latest
}

fn web_artwork_id(path: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone)]
struct CarouselArtSeed {
    path: String,
    album_key: String,
}

/// What mode the UI is in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiMode {
    /// Normal now-playing display.
    NowPlaying,
    /// Playlist picker overlay.
    PlaylistPicker,
    /// Top-level menu.
    MainMenu,
    /// Editor for one numeric setting.
    SettingEditor(SettingKind),
    /// Runtime status information.
    Diagnostics,
    /// Scannable link to the guest web player.
    GuestQr,
    /// Idle carousel of cached album artwork.
    Screensaver,
    /// Once-per-day weather, headlines, and encouragement dashboard.
    Morning,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingKind {
    Volume,
    Brightness,
    DimTimeout,
    VisualizerDelay,
    SpotifyVisualizerDelay,
    CarouselSpeed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuItem {
    Playlists,
    Volume,
    Brightness,
    DimTimeout,
    IdleDisplay,
    ScreensaverStyle,
    ScreensaverShuffle,
    CarouselSpeed,
    PlaylistShuffle,
    NowPlayingStyle,
    VisualizerDelay,
    SpotifyVisualizerDelay,
    GuestQr,
    Diagnostics,
    Exit,
}

pub const MAIN_MENU_ITEMS: [MenuItem; 15] = [
    MenuItem::Playlists,
    MenuItem::Volume,
    MenuItem::Brightness,
    MenuItem::DimTimeout,
    MenuItem::IdleDisplay,
    MenuItem::ScreensaverStyle,
    MenuItem::ScreensaverShuffle,
    MenuItem::CarouselSpeed,
    MenuItem::PlaylistShuffle,
    MenuItem::NowPlayingStyle,
    MenuItem::VisualizerDelay,
    MenuItem::SpotifyVisualizerDelay,
    MenuItem::GuestQr,
    MenuItem::Diagnostics,
    MenuItem::Exit,
];

impl MenuItem {
    pub fn label(self) -> &'static str {
        match self {
            Self::Playlists => "Playlists",
            Self::Volume => "Volume",
            Self::Brightness => "Brightness",
            Self::DimTimeout => "Dim timeout",
            Self::IdleDisplay => "Idle display",
            Self::ScreensaverStyle => "Saver style",
            Self::ScreensaverShuffle => "Shuffle saver art",
            Self::CarouselSpeed => "Carousel speed",
            Self::PlaylistShuffle => "Shuffle playlists",
            Self::NowPlayingStyle => "Now playing",
            Self::VisualizerDelay => "Visualizer delay",
            Self::SpotifyVisualizerDelay => "Spotify vis offset",
            Self::GuestQr => "Guest QR code",
            Self::Diagnostics => "Diagnostics",
            Self::Exit => "Exit menu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IdleDisplayMode {
    Dim,
    Screensaver,
    DimmedScreensaver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSource {
    Mopidy,
    Spotifyd,
}

impl PlaybackSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mopidy => "Mopidy",
            Self::Spotifyd => "Spotify Connect",
        }
    }
}

fn visualizer_delay_for_source(settings: &Settings, source: PlaybackSource) -> Duration {
    let extra = match source {
        PlaybackSource::Mopidy => 0,
        PlaybackSource::Spotifyd => settings.spotify_visualizer_extra_delay_ms,
    };
    Duration::from_millis(settings.visualizer_delay_ms.saturating_add(extra))
}

impl IdleDisplayMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dim => "Dim",
            Self::Screensaver => "Saver",
            Self::DimmedScreensaver => "Saver + dim",
        }
    }
}

pub struct AppState {
    pub current_track: Track,
    pub next_track: Option<Track>,
    pub current_time: f64,
    pub is_playing: bool,
    pub offset: i32,
    pub brightness: f64,
    pub volume: Option<u8>,
    pub settings: Settings,
    settings_path: Option<PathBuf>,
    last_volume_command: Option<(u8, Instant)>,
    volume_save_due: Option<Instant>,
    sleep_timer: Option<SleepTimer>,
    sleep_timer_last_reported_seconds: Option<u64>,

    // UI mode
    pub ui_mode: UiMode,
    pub menu_index: usize,
    pub picker_index: usize,
    pub picker_playlists: Vec<PlaylistEntry>,
    pub picker_art_path: String,
    picker_last_scroll: Instant,
    pub screensaver_art_paths: Vec<String>,
    pub screensaver_art_order: Vec<usize>,
    screensaver_art_album_keys: Vec<Option<String>>,
    screensaver_art_fingerprints: Vec<Option<ArtworkFingerprint>>,
    pub screensaver_offset: f64,
    screensaver_preview: bool,
    screensaver_shuffle_nonce: u64,
    pub weather: Option<WeatherData>,
    pub weather_error: Option<String>,
    weather_updates: Receiver<WeatherUpdate>,
    pub news: Option<NewsData>,
    pub news_error: Option<String>,
    news_updates: Receiver<NewsUpdate>,
    morning_dismissed_day: Option<i32>,
    web_updates: Receiver<WebConfigUpdate>,
    web_playback_status: Arc<Mutex<WebPlaybackStatus>>,
    web_playlist_catalog: Arc<Mutex<Vec<WebPlaylist>>>,
    web_queue: Arc<Mutex<Vec<crate::mopidy::MopidyQueueTrack>>>,
    web_history: Arc<Mutex<Vec<HistoryEntry>>>,
    history_store: Option<HistoryStore>,
    web_playlist_updates: Receiver<Option<Vec<WebPlaylist>>>,
    web_playlist_update_tx: Sender<Option<Vec<WebPlaylist>>>,
    web_playlist_refresh_in_flight: bool,
    last_web_playlist_refresh: Instant,
    visualizer_updates: Receiver<SpectrumFrame>,
    visualizer_pending: VecDeque<(Instant, SpectrumFrame)>,
    pub visualizer_levels: SpectrumFrame,
    last_visualizer_update: Option<Instant>,

    // Preloaded art paths per-playlist (populated by background preload thread at startup)
    picker_art_cache: Vec<Option<String>>,
    picker_preload_rx: Receiver<(usize, String)>,
    carousel_preload_rx: Receiver<CarouselArtSeed>,
    picker_base_url: String,
    picker_rpc_url: String,
    picker_art_cache_dir: PathBuf,
    album_art_cache_dir: PathBuf,

    // Mopidy integration
    mopidy_commands: Sender<MopidyCommand>,
    mopidy_updates: Receiver<Option<MopidySnapshot>>,
    last_mopidy_snapshot: Option<MopidySnapshot>,
    poll_in_flight: bool,
    last_poll: Instant,
    last_track_uri: Option<String>,
    idle_since: Option<Instant>,
    pub mopidy_online: bool,
    pub last_mopidy_update: Option<Instant>,

    // Spotify Connect integration. A playing Spotifyd session takes priority;
    // explicitly starting Mopidy yields it until Spotify starts again.
    spotifyd_commands: Sender<SpotifydCommand>,
    spotifyd_updates: Receiver<SpotifydUpdate>,
    spotifyd_yielded: bool,
    last_spotifyd_playback_state: Option<SpotifydPlaybackState>,
    pub active_source: PlaybackSource,
    pub spotifyd_available: bool,
    pub last_spotifyd_update: Option<Instant>,
}

impl AppState {
    pub fn new(
        mopidy: MopidyClient,
        config: Config,
        settings: Settings,
        settings_path: PathBuf,
        web_config: WebConfigBridge,
        visualizer_updates: Receiver<SpectrumFrame>,
    ) -> Self {
        let WebConfigBridge {
            updates: web_updates,
            playback_status: web_playback_status,
            playlist_catalog: web_playlist_catalog,
            queue: web_queue,
            history: web_history,
        } = web_config;
        let history_path = settings_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("playback-history.toml");
        let history_store = HistoryStore::load(history_path);
        if let Ok(mut shared_history) = web_history.lock() {
            *shared_history = history_store.entries_newest_first();
        }
        let initial_web_playlists = merge_web_playlist_catalog(&config.playlists, Vec::new());
        if let Ok(mut catalog) = web_playlist_catalog.lock() {
            *catalog = initial_web_playlists;
        }
        let (web_playlist_update_tx, web_playlist_updates) = mpsc::channel();
        let (preload_tx, preload_rx) = mpsc::channel();
        let num_playlists = config.playlists.len();
        let base_url = mopidy.base_url().to_string();
        let rpc_url = mopidy.rpc_url().to_string();
        let album_art_cache_dir = mopidy.art_cache_dir().to_path_buf();
        let cache_root = album_art_cache_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| album_art_cache_dir.clone());
        let picker_art_cache_dir = cache_root.join("playlists");
        std::fs::create_dir_all(&picker_art_cache_dir).ok();
        let screensaver_art_paths = scan_cached_art(&album_art_cache_dir);
        let screensaver_art_fingerprints = screensaver_art_paths
            .iter()
            .map(|path| artwork_file_fingerprint(Path::new(path)))
            .collect();
        let screensaver_art_album_keys = vec![None; screensaver_art_paths.len()];
        let screensaver_art_order = (0..screensaver_art_paths.len()).collect();
        let (carousel_preload_tx, carousel_preload_rx) = mpsc::channel();
        let weather_updates =
            crate::weather::spawn_worker(settings.weather_latitude, settings.weather_longitude);
        let news_updates = crate::news::spawn_worker(settings.morning_news_feed_url.clone());
        let worker = mopidy.spawn_worker();
        let spotifyd_worker = crate::spotifyd::spawn_worker(album_art_cache_dir.clone());
        let initial_volume = settings.playback_volume.min(settings.max_volume);

        let mut state = Self {
            current_track: Track::idle(),
            next_track: None,
            current_time: 0.0,
            is_playing: false,
            offset: 0,
            brightness: settings.display_brightness,
            volume: Some(initial_volume),
            settings,
            settings_path: Some(settings_path),
            last_volume_command: None,
            volume_save_due: None,
            sleep_timer: None,
            sleep_timer_last_reported_seconds: None,
            ui_mode: UiMode::NowPlaying,
            menu_index: 0,
            picker_index: 0,
            picker_playlists: config.playlists,
            picker_art_path: String::new(),
            picker_last_scroll: Instant::now(),
            screensaver_art_paths,
            screensaver_art_order,
            screensaver_art_album_keys,
            screensaver_art_fingerprints,
            screensaver_offset: 0.0,
            screensaver_preview: false,
            screensaver_shuffle_nonce: 0,
            weather: None,
            weather_error: None,
            weather_updates,
            news: None,
            news_error: None,
            news_updates,
            morning_dismissed_day: None,
            web_updates,
            web_playback_status,
            web_playlist_catalog,
            web_queue,
            web_history,
            history_store: Some(history_store),
            web_playlist_updates,
            web_playlist_update_tx,
            web_playlist_refresh_in_flight: false,
            last_web_playlist_refresh: Instant::now()
                .checked_sub(WEB_PLAYLIST_REFRESH_INTERVAL)
                .unwrap_or_else(Instant::now),
            visualizer_updates,
            visualizer_pending: VecDeque::new(),
            visualizer_levels: [0.0; BAR_COUNT],
            last_visualizer_update: None,
            picker_art_cache: vec![None; num_playlists],
            picker_preload_rx: preload_rx,
            carousel_preload_rx,
            picker_base_url: base_url.clone(),
            picker_rpc_url: rpc_url.clone(),
            picker_art_cache_dir: picker_art_cache_dir.clone(),
            album_art_cache_dir: album_art_cache_dir.clone(),
            mopidy_commands: worker.commands,
            mopidy_updates: worker.updates,
            last_mopidy_snapshot: None,
            poll_in_flight: false,
            last_poll: Instant::now(),
            last_track_uri: None,
            idle_since: Some(Instant::now()),
            mopidy_online: false,
            last_mopidy_update: None,
            spotifyd_commands: spotifyd_worker.commands,
            spotifyd_updates: spotifyd_worker.updates,
            spotifyd_yielded: false,
            last_spotifyd_playback_state: None,
            active_source: PlaybackSource::Mopidy,
            spotifyd_available: false,
            last_spotifyd_update: None,
        };

        // Establish the safety ceiling and synchronize both playback engines
        // before accepting their first (potentially stale) volume snapshots.
        state.apply_hardware_volume_ceiling();
        state.dispatch_volume(initial_volume);

        // Queue an initial poll without delaying display startup.
        state.poll_mopidy();
        state.publish_web_playback_status();

        // Preload all playlist art in background
        state.preload_all_picker_art(
            preload_tx,
            base_url.clone(),
            rpc_url.clone(),
            picker_art_cache_dir,
        );
        state.preload_carousel_album_art(
            carousel_preload_tx,
            base_url,
            rpc_url,
            album_art_cache_dir,
        );
        state
    }

    /// Preload all playlist images in background threads at startup.
    fn preload_all_picker_art(
        &self,
        preload_tx: Sender<(usize, String)>,
        base_url: String,
        rpc_url: String,
        art_cache_dir: std::path::PathBuf,
    ) {
        let playlists = self.picker_playlists.clone();

        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build();

            println!("[ART] Preloading {} playlist images...", playlists.len());
            for (idx, entry) in playlists.iter().enumerate() {
                // Try explicit art_uri first
                if let Some(art_uri) = &entry.art_uri
                    && let Some(path) =
                        fetch_art_blocking(&agent, &base_url, &art_cache_dir, art_uri)
                {
                    println!("[ART] Preloaded: {} (explicit)", entry.name);
                    if preload_tx.send((idx, path)).is_err() {
                        return;
                    }
                    continue;
                }
                // Ask Mopidy for the image
                if let Some(image_uri) = get_image_uri_blocking(&agent, &rpc_url, &entry.uri)
                    && let Some(path) =
                        fetch_art_blocking(&agent, &base_url, &art_cache_dir, &image_uri)
                {
                    println!("[ART] Preloaded: {}", entry.name);
                    if preload_tx.send((idx, path)).is_err() {
                        return;
                    }
                    continue;
                }
                println!("[ART] No art found for: {}", entry.name);
            }
            println!("[ART] Preload complete");
        });
    }

    /// Seed the idle carousel with real album covers without delaying startup.
    /// Queue and history entries are preferred, then configured playlists are
    /// sampled in round-robin order so one large playlist cannot dominate.
    fn preload_carousel_album_art(
        &self,
        preload_tx: Sender<CarouselArtSeed>,
        base_url: String,
        rpc_url: String,
        album_art_cache_dir: PathBuf,
    ) {
        let playlists = self.picker_playlists.clone();

        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build();
            std::fs::create_dir_all(&album_art_cache_dir).ok();

            let mut seen_albums = HashSet::new();
            let mut seen_fingerprints: Vec<ArtworkFingerprint> =
                scan_cached_art(&album_art_cache_dir)
                    .iter()
                    .filter_map(|path| artwork_file_fingerprint(Path::new(path)))
                    .collect();
            let mut seeded = 0;

            // Avoid touching Mopidy at all when the persisted cache is already
            // ready. This keeps restarts cheap and prevents redundant library
            // lookups from competing with normal playback polling.
            if seen_fingerprints.len() >= CAROUSEL_PRELOAD_TARGET {
                println!(
                    "[ART] Carousel ready with {} distinct covers (cache hit)",
                    seen_fingerprints.len()
                );
                return;
            }

            let candidates = collect_album_seed_tracks_blocking(&agent, &rpc_url, &playlists);

            for track in candidates {
                if seen_fingerprints.len() >= CAROUSEL_PRELOAD_TARGET {
                    break;
                }
                let Some((track_uri, album_key)) = track_album_identity(&track) else {
                    continue;
                };
                if !seen_albums.insert(album_key.clone()) {
                    continue;
                }
                let Some(image_uri) = get_image_uri_blocking(&agent, &rpc_url, &track_uri) else {
                    continue;
                };
                let Some(path) =
                    fetch_art_blocking(&agent, &base_url, &album_art_cache_dir, &image_uri)
                else {
                    continue;
                };
                let Some(fingerprint) = artwork_file_fingerprint(Path::new(&path)) else {
                    continue;
                };
                if seen_fingerprints
                    .iter()
                    .any(|known| artwork_fingerprints_match(*known, fingerprint))
                {
                    continue;
                }
                seen_fingerprints.push(fingerprint);
                if preload_tx
                    .send(CarouselArtSeed { path, album_key })
                    .is_err()
                {
                    return;
                }
                seeded += 1;
            }

            println!(
                "[ART] Carousel ready with {} distinct covers ({seeded} fetched)",
                seen_fingerprints.len()
            );
        });
    }

    pub fn current_track(&self) -> &Track {
        &self.current_track
    }

    /// Queue a Mopidy state poll. Network I/O happens on the worker thread.
    pub fn poll_mopidy(&mut self) {
        if self.poll_in_flight {
            return;
        }

        match self.mopidy_commands.send(MopidyCommand::Poll) {
            Ok(()) => {
                self.poll_in_flight = true;
                self.last_poll = Instant::now();
            }
            Err(e) => eprintln!("Mopidy worker is unavailable: {}", e),
        }
    }

    /// Apply a completed worker snapshot to UI state.
    fn apply_mopidy_snapshot(&mut self, snapshot: MopidySnapshot) {
        let MopidySnapshot {
            playback_state,
            time_position_ms,
            volume,
            track,
            next_track,
            album_art_path,
            queue: _,
        } = snapshot;

        self.is_playing = playback_state == PlaybackState::Playing;
        self.current_time = time_position_ms as f64 / 1000.0;
        if let Some(volume) = volume {
            self.observe_backend_volume(PlaybackSource::Mopidy, volume);
        }
        if !album_art_path.is_empty() {
            let album_key = track
                .as_ref()
                .map(|track| normalized_album_key(&track.artist, &track.album));
            self.add_screensaver_art(album_art_path.clone(), album_key);
        }
        if self.is_playing && self.ui_mode == UiMode::Screensaver && !self.screensaver_preview {
            self.ui_mode = UiMode::NowPlaying;
            self.screensaver_offset = 0.0;
        }

        self.next_track = next_track.map(|track_info| Track {
            title: track_info.title,
            artist: track_info.artist,
            album: track_info.album,
            duration: track_info.duration_ms as f64 / 1000.0,
            album_art_path: String::new(),
        });

        if let Some(track_info) = track {
            let track_changed = self.last_track_uri.as_deref() != Some(track_info.uri.as_str());
            let history_uri = track_info.uri.clone();

            self.current_track = Track {
                title: track_info.title.clone(),
                artist: track_info.artist,
                album: track_info.album,
                duration: track_info.duration_ms as f64 / 1000.0,
                album_art_path,
            };

            if track_changed {
                self.offset = 0;
                self.last_track_uri = Some(track_info.uri);
                println!(
                    "Now playing: {} - {}",
                    self.current_track.artist, self.current_track.title
                );
                if let Some(next_track) = &self.next_track {
                    println!("Next up: {} - {}", next_track.artist, next_track.title);
                }
            }
            // A backend may expose a newly selected track while it is still
            // paused. Recording on every playing snapshot lets the first
            // subsequent resume reach the history store; the store itself
            // suppresses poll duplicates.
            if self.is_playing {
                self.record_history(history_uri.clone(), Some(history_uri), "Mopidy".to_string());
            }
        } else {
            if self.last_track_uri.take().is_some() {
                self.current_track = Track::idle();
                self.offset = 0;
            }
        }
    }

    fn apply_spotifyd_snapshot(&mut self, snapshot: SpotifydSnapshot) {
        let SpotifydSnapshot {
            playback_state,
            time_position_ms,
            volume,
            track,
            album_art_path,
        } = snapshot;

        self.is_playing = playback_state == SpotifydPlaybackState::Playing;
        self.current_time = time_position_ms as f64 / 1000.0;
        if let Some(volume) = volume {
            self.observe_backend_volume(PlaybackSource::Spotifyd, volume);
        }
        if !album_art_path.is_empty() {
            let album_key = track
                .as_ref()
                .map(|track| normalized_album_key(&track.artist, &track.album));
            self.add_screensaver_art(album_art_path.clone(), album_key);
        }
        if self.is_playing && self.ui_mode == UiMode::Screensaver && !self.screensaver_preview {
            self.ui_mode = UiMode::NowPlaying;
            self.screensaver_offset = 0.0;
        }

        self.next_track = None;
        if let Some(track_info) = track {
            let source_track_id = format!("spotifyd:{}", track_info.id);
            let track_changed = self.last_track_uri.as_deref() != Some(source_track_id.as_str());
            let art_path = if album_art_path.is_empty() && !track_changed {
                self.current_track.album_art_path.clone()
            } else {
                album_art_path
            };
            self.current_track = Track {
                title: track_info.title,
                artist: track_info.artist,
                album: track_info.album,
                duration: track_info.duration_ms as f64 / 1000.0,
                album_art_path: art_path,
            };
            if track_changed {
                self.offset = 0;
                self.last_track_uri = Some(source_track_id.clone());
                println!(
                    "[SPOTIFYD] Now playing: {} - {}",
                    self.current_track.artist, self.current_track.title
                );
            }
            if self.is_playing {
                self.record_history(source_track_id, None, "Spotify Connect".to_string());
            }
        } else if self
            .last_track_uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with("spotifyd:"))
        {
            self.last_track_uri = None;
            self.current_track = Track::idle();
            self.offset = 0;
        }
    }

    fn switch_to_mopidy(&mut self) {
        self.active_source = PlaybackSource::Mopidy;
        if let Some(snapshot) = self.last_mopidy_snapshot.clone() {
            self.apply_mopidy_snapshot(snapshot);
        }
    }

    fn yield_spotifyd_to_mopidy(&mut self) {
        self.spotifyd_yielded = true;
        if self.active_source == PlaybackSource::Spotifyd {
            self.spotifyd_commands.send(SpotifydCommand::Pause).ok();
        }
        self.switch_to_mopidy();
    }

    fn active_playback_online(&self) -> bool {
        match self.active_source {
            PlaybackSource::Mopidy => self.mopidy_online,
            PlaybackSource::Spotifyd => self.spotifyd_available,
        }
    }

    pub fn next_up_toast_track(&self) -> Option<&Track> {
        let duration = self.current_track.duration;
        let remaining = duration - self.current_time;
        if self.is_playing
            && self.current_time > 0.0
            && duration > 0.0
            && remaining > 0.0
            && remaining <= 15.0
        {
            self.next_track.as_ref()
        } else {
            None
        }
    }

    fn add_screensaver_art(&mut self, path: String, album_key: Option<String>) -> bool {
        if path.is_empty()
            || self
                .screensaver_art_paths
                .iter()
                .any(|known| known == &path)
        {
            return false;
        }

        let album_key = album_key.filter(|key| !key.is_empty());
        if album_key.as_ref().is_some_and(|key| {
            self.screensaver_art_album_keys
                .iter()
                .flatten()
                .any(|known| known == key)
        }) {
            return false;
        }

        let fingerprint = artwork_file_fingerprint(Path::new(&path));
        if fingerprint.is_some_and(|fingerprint| {
            self.screensaver_art_fingerprints
                .iter()
                .flatten()
                .any(|known| artwork_fingerprints_match(*known, fingerprint))
        }) {
            return false;
        }

        self.screensaver_art_paths.push(path);
        self.screensaver_art_album_keys.push(album_key);
        self.screensaver_art_fingerprints.push(fingerprint);
        if self.screensaver_art_paths.len() > MAX_SCREENSAVER_ART {
            self.screensaver_art_paths.remove(0);
            self.screensaver_art_album_keys.remove(0);
            self.screensaver_art_fingerprints.remove(0);
            self.reset_screensaver_art_order();
        } else {
            self.screensaver_art_order
                .push(self.screensaver_art_paths.len() - 1);
        }
        true
    }

    fn reset_screensaver_art_order(&mut self) {
        self.screensaver_art_order = (0..self.screensaver_art_paths.len()).collect();
    }

    fn prepare_screensaver_art_order(&mut self) {
        let art_count = self.screensaver_art_paths.len();
        let mut valid_order = self.screensaver_art_order.len() == art_count;
        if valid_order {
            let mut seen = vec![false; art_count];
            for &index in &self.screensaver_art_order {
                if index >= art_count || seen[index] {
                    valid_order = false;
                    break;
                }
                seen[index] = true;
            }
        }
        if !valid_order || !self.settings.shuffle_screensaver_art {
            self.reset_screensaver_art_order();
        }

        if !self.settings.shuffle_screensaver_art || art_count < 2 {
            return;
        }

        let previous_order = self.screensaver_art_order.clone();
        self.screensaver_shuffle_nonce = self.screensaver_shuffle_nonce.wrapping_add(1);
        let time_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let seed = time_seed
            ^ self
                .screensaver_shuffle_nonce
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ art_count as u64;
        shuffle_indices(&mut self.screensaver_art_order, seed);

        // Make each activation visibly different, even if the shuffle happens
        // to produce the same permutation as the previous run.
        if self.screensaver_art_order == previous_order {
            self.screensaver_art_order.rotate_left(1);
        }
    }

    /// Called each frame to advance animations and poll Mopidy periodically.
    pub fn advance_time(&mut self, dt: f64) {
        let visualizer_now = Instant::now();
        let mut received_visualizer_input = false;
        while let Ok(frame) = self.visualizer_updates.try_recv() {
            self.visualizer_pending.push_back((visualizer_now, frame));
            received_visualizer_input = true;
        }
        while self.visualizer_pending.len() > MAX_PENDING_VISUALIZER_FRAMES {
            self.visualizer_pending.pop_front();
        }
        if received_visualizer_input {
            self.last_visualizer_update = Some(visualizer_now);
        }
        if let Some(frame) = take_ready_visualizer_frame(
            &mut self.visualizer_pending,
            visualizer_now,
            visualizer_delay_for_source(&self.settings, self.active_source),
        ) {
            self.visualizer_levels = frame;
        }
        if self
            .last_visualizer_update
            .is_some_and(|updated| updated.elapsed() > std::time::Duration::from_millis(120))
        {
            for level in &mut self.visualizer_levels {
                *level *= 0.86;
            }
        }

        while let Ok(update) = self.web_updates.try_recv() {
            self.apply_web_config_update(update);
        }

        while let Ok(catalog) = self.web_playlist_updates.try_recv() {
            self.web_playlist_refresh_in_flight = false;
            self.last_web_playlist_refresh = Instant::now();
            if let Some(catalog) = catalog {
                let count = catalog.len();
                if let Ok(mut shared) = self.web_playlist_catalog.lock() {
                    *shared = catalog;
                }
                println!("[WEB] Loaded {count} playlists for the remote player");
            } else {
                eprintln!("[WEB] Could not refresh the Mopidy playlist catalog");
            }
        }

        if self.mopidy_online
            && !self.web_playlist_refresh_in_flight
            && self.last_web_playlist_refresh.elapsed() >= WEB_PLAYLIST_REFRESH_INTERVAL
        {
            self.refresh_web_playlist_catalog();
        }

        while let Ok(update) = self.weather_updates.try_recv() {
            match update {
                Ok(weather) => {
                    println!(
                        "[WEATHER] {}: {:.1} C, {}",
                        self.settings.weather_location,
                        weather.temperature,
                        weather.description()
                    );
                    self.weather = Some(weather);
                    self.weather_error = None;
                }
                Err(error) => {
                    eprintln!("[WEATHER] {error}");
                    self.weather_error = Some(error);
                }
            }
        }

        while let Ok(update) = self.news_updates.try_recv() {
            match update {
                Ok(news) => {
                    println!("[NEWS] Loaded {} morning headlines", news.headlines.len());
                    self.news = Some(news);
                    self.news_error = None;
                }
                Err(error) => {
                    eprintln!("[NEWS] {error}");
                    self.news_error = Some(error);
                }
            }
        }

        // Apply completed Mopidy work without blocking on the network.
        let mut playback_status_changed = false;
        while let Ok(snapshot) = self.mopidy_updates.try_recv() {
            self.poll_in_flight = false;
            if let Some(snapshot) = snapshot {
                self.mopidy_online = true;
                self.last_mopidy_update = Some(Instant::now());
                self.publish_web_queue(&snapshot.queue);
                self.last_mopidy_snapshot = Some(snapshot.clone());
                if self.active_source == PlaybackSource::Mopidy {
                    self.apply_mopidy_snapshot(snapshot);
                } else if let Some(reported) = snapshot.volume
                    && self.volume.is_some_and(|master| reported != master)
                {
                    // An inactive backend must not retain a louder value that
                    // could surprise the listener when sources switch.
                    if let Some(master) = self.volume {
                        self.mopidy_commands
                            .send(MopidyCommand::SetVolume(master))
                            .ok();
                    }
                }
            } else {
                self.mopidy_online = false;
                // The failed request has already waited for its network
                // timeout. Add a quiet retry gap instead of immediately
                // starting another request against an unavailable server.
                self.last_poll = Instant::now();
            }
            playback_status_changed = true;
        }

        while let Ok(update) = self.spotifyd_updates.try_recv() {
            self.spotifyd_available = update.available;
            self.last_spotifyd_update = Some(Instant::now());
            match update.snapshot {
                Some(snapshot) => {
                    let spotify_is_playing =
                        snapshot.playback_state == SpotifydPlaybackState::Playing;
                    let spotify_started = spotify_playback_started(
                        self.last_spotifyd_playback_state,
                        snapshot.playback_state,
                    );
                    self.last_spotifyd_playback_state = Some(snapshot.playback_state);
                    if spotify_started {
                        self.spotifyd_yielded = false;
                    }
                    let should_show_spotify = self.active_source == PlaybackSource::Spotifyd
                        || spotify_is_playing
                        || (!self.spotifyd_yielded && !self.is_playing);
                    if should_show_spotify {
                        let source_changed = self.active_source != PlaybackSource::Spotifyd;
                        self.active_source = PlaybackSource::Spotifyd;
                        if source_changed && spotify_is_playing {
                            // Never toggle here: an already-paused Mopidy must
                            // remain paused when Spotify Connect takes over.
                            self.mopidy_commands.send(MopidyCommand::Pause).ok();
                        }
                        self.apply_spotifyd_snapshot(snapshot);
                    } else if let Some(reported) = snapshot.volume
                        && self.volume.is_some_and(|master| reported != master)
                    {
                        // Keep the parked Spotify session aligned with the
                        // current master volume as well.
                        if let Some(master) = self.volume {
                            self.spotifyd_commands
                                .send(SpotifydCommand::SetVolume(master))
                                .ok();
                        }
                    }
                }
                None if self.active_source == PlaybackSource::Spotifyd => {
                    self.last_spotifyd_playback_state = None;
                    self.switch_to_mopidy();
                }
                None => self.last_spotifyd_playback_state = None,
            }
            playback_status_changed = true;
        }
        if playback_status_changed {
            self.publish_web_playback_status();
        }

        // Playlist thumbnails are picker-only and deliberately never enter the
        // album carousel.
        while let Ok((idx, path)) = self.picker_preload_rx.try_recv() {
            if idx < self.picker_art_cache.len() {
                self.picker_art_cache[idx] = Some(path);
            }
            // If we're currently viewing this index in the picker, update immediately
            if self.ui_mode == UiMode::PlaylistPicker && idx == self.picker_index {
                self.resolve_picker_art();
            }
        }

        let mut carousel_changed = false;
        while let Ok(seed) = self.carousel_preload_rx.try_recv() {
            carousel_changed |= self.add_screensaver_art(seed.path, Some(seed.album_key));
        }
        if carousel_changed {
            self.publish_web_playback_status();
        }

        // Network work is asynchronous, so every UI screen can stay up to date.
        if self.last_poll.elapsed().as_secs_f64() >= mopidy_poll_interval(self.mopidy_online) {
            self.poll_mopidy();
        }

        // Advance local time estimate between polls (smooth playback bar)
        if self.is_playing {
            self.current_time += dt;
        }

        self.update_sleep_timer_at(Instant::now());

        // Advance marquee scroll
        self.offset += 2;
        if self.ui_mode == UiMode::Screensaver {
            self.screensaver_offset += dt * self.settings.carousel_speed;
            if self.screensaver_offset > 1_000_000.0 {
                self.screensaver_offset = 0.0;
            }
        }

        if self
            .volume_save_due
            .is_some_and(|changed_at| changed_at.elapsed() >= VOLUME_SAVE_DELAY)
        {
            self.volume_save_due = None;
            self.persist_settings();
        }

        self.update_morning_mode();

        // Track idle state and update brightness
        self.update_brightness();
    }

    pub fn visualizer_live(&self) -> bool {
        self.last_visualizer_update
            .is_some_and(|updated| updated.elapsed() < std::time::Duration::from_millis(500))
    }

    fn apply_web_config_update(&mut self, update: WebConfigUpdate) {
        if self.ui_mode == UiMode::Morning {
            self.dismiss_morning_mode();
        }
        match update {
            WebConfigUpdate::Settings(settings) => {
                let weather_location_changed = self.settings.weather_latitude
                    != settings.weather_latitude
                    || self.settings.weather_longitude != settings.weather_longitude;
                let news_feed_changed =
                    self.settings.morning_news_feed_url != settings.morning_news_feed_url;
                let hardware_ceiling_changed =
                    self.settings.hardware_volume_ceiling_db != settings.hardware_volume_ceiling_db;
                self.settings = settings;
                if hardware_ceiling_changed {
                    self.apply_hardware_volume_ceiling();
                }
                if self
                    .volume
                    .is_some_and(|volume| volume > self.settings.max_volume)
                {
                    self.set_volume(self.settings.max_volume);
                }
                if weather_location_changed {
                    self.weather_updates = crate::weather::spawn_worker(
                        self.settings.weather_latitude,
                        self.settings.weather_longitude,
                    );
                    self.weather = None;
                    self.weather_error = None;
                }
                if news_feed_changed {
                    self.news_updates =
                        crate::news::spawn_worker(self.settings.morning_news_feed_url.clone());
                    self.news = None;
                    self.news_error = None;
                }
                self.update_brightness();
                self.publish_web_playback_status();
                println!("[WEB] Settings applied");
            }
            WebConfigUpdate::Playlists(config) => {
                self.picker_playlists = config.playlists;
                self.picker_index = 0;
                self.picker_art_path.clear();
                if self.picker_playlists.is_empty() && self.ui_mode == UiMode::PlaylistPicker {
                    self.ui_mode = UiMode::MainMenu;
                }
                self.restart_picker_preload();
                self.restart_carousel_preload();
                self.publish_configured_web_playlists();
                self.last_web_playlist_refresh = Instant::now()
                    .checked_sub(WEB_PLAYLIST_REFRESH_INTERVAL)
                    .unwrap_or_else(Instant::now);
                println!(
                    "[WEB] Reloaded {} configured playlists",
                    self.picker_playlists.len()
                );
            }
            WebConfigUpdate::PlayPlaylist(entry) => self.start_playlist(entry),
            WebConfigUpdate::QueueTrack {
                uri,
                placement,
                requested_by,
            } => {
                if placement == crate::mopidy::QueuePlacement::Now {
                    self.yield_spotifyd_to_mopidy();
                }
                if let Err(error) = self.mopidy_commands.send(MopidyCommand::QueueTrack {
                    uri,
                    placement,
                    requested_by,
                }) {
                    eprintln!("Failed to send queue command: {error}");
                }
            }
            WebConfigUpdate::QueueEdit(action) => {
                let command = match action {
                    WebQueueAction::Remove(tlid) => MopidyCommand::RemoveQueueTrack(tlid),
                    WebQueueAction::MoveUp(tlid) => MopidyCommand::MoveQueueTrack {
                        tlid,
                        direction: -1,
                    },
                    WebQueueAction::MoveDown(tlid) => {
                        MopidyCommand::MoveQueueTrack { tlid, direction: 1 }
                    }
                    WebQueueAction::Play(tlid) => {
                        self.yield_spotifyd_to_mopidy();
                        MopidyCommand::PlayQueueTrack(tlid)
                    }
                    WebQueueAction::Clear => MopidyCommand::ClearQueue,
                };
                if let Err(error) = self.mopidy_commands.send(command) {
                    eprintln!("Failed to send queue edit: {error}");
                }
            }
            WebConfigUpdate::QueueVote(tlid) => {
                if let Err(error) = self
                    .mopidy_commands
                    .send(MopidyCommand::VoteQueueTrack(tlid))
                {
                    eprintln!("Failed to send queue vote: {error}");
                }
            }
            WebConfigUpdate::Playback(action) => match action {
                WebPlaybackAction::Previous => self.previous_track(),
                WebPlaybackAction::TogglePlayPause => self.toggle_play_pause(),
                WebPlaybackAction::Next => self.skip_track(),
                WebPlaybackAction::Seek(position_seconds) => self.seek_to(position_seconds),
                WebPlaybackAction::SetVolume(volume) => self.set_volume(volume),
                WebPlaybackAction::ToggleScreensaver => self.toggle_screensaver_preview(),
                WebPlaybackAction::StartSleepTimer(minutes) => self.start_sleep_timer(minutes),
                WebPlaybackAction::CancelSleepTimer => self.cancel_sleep_timer(),
            },
        }
    }

    fn publish_web_playback_status(&self) {
        let Ok(mut status) = self.web_playback_status.lock() else {
            eprintln!("[WEB] Playback status lock is unavailable");
            return;
        };
        let artwork_path = (!self.current_track.album_art_path.is_empty())
            .then(|| self.current_track.album_art_path.clone());
        let artwork_id = artwork_path.as_deref().map(web_artwork_id);
        *status = WebPlaybackStatus {
            title: self.current_track.title.clone(),
            artist: self.current_track.artist.clone(),
            album: self.current_track.album.clone(),
            is_playing: self.is_playing,
            online: self.active_playback_online(),
            source: self.active_source.label().to_string(),
            mopidy_online: self.mopidy_online,
            spotifyd_available: self.spotifyd_available,
            volume: self.volume,
            max_volume: self.settings.max_volume,
            position_seconds: finite_nonnegative(self.current_time),
            duration_seconds: finite_nonnegative(self.current_track.duration),
            next_title: self.next_track.as_ref().map(|track| track.title.clone()),
            next_artist: self.next_track.as_ref().map(|track| track.artist.clone()),
            artwork_id,
            artwork_path,
            screensaver_active: self.ui_mode == UiMode::Screensaver,
            carousel_cover_count: self.screensaver_art_paths.len(),
            carousel_speed: self.settings.carousel_speed,
            sleep_timer_remaining_seconds: self
                .sleep_timer
                .as_ref()
                .map(|timer| timer.remaining_seconds(Instant::now())),
            sleep_timer_fading: self
                .sleep_timer
                .as_ref()
                .is_some_and(|timer| timer.is_fading(Instant::now())),
        };
    }

    fn publish_web_queue(&self, queue: &[crate::mopidy::MopidyQueueTrack]) {
        if let Ok(mut shared) = self.web_queue.lock() {
            *shared = queue.to_vec();
        }
    }

    fn record_history(&mut self, id: String, uri: Option<String>, source: String) {
        let Some(history) = self.history_store.as_mut() else {
            return;
        };
        let added = history.record(
            id,
            uri,
            self.current_track.title.clone(),
            self.current_track.artist.clone(),
            self.current_track.album.clone(),
            source,
            (self.current_track.duration.max(0.0) * 1_000.0) as u64,
        );
        if added && let Ok(mut shared) = self.web_history.lock() {
            *shared = history.entries_newest_first();
        }
    }

    fn restart_picker_preload(&mut self) {
        let (preload_tx, preload_rx) = mpsc::channel();
        self.picker_preload_rx = preload_rx;
        self.picker_art_cache = vec![None; self.picker_playlists.len()];
        self.preload_all_picker_art(
            preload_tx,
            self.picker_base_url.clone(),
            self.picker_rpc_url.clone(),
            self.picker_art_cache_dir.clone(),
        );
    }

    fn restart_carousel_preload(&mut self) {
        let (preload_tx, preload_rx) = mpsc::channel();
        self.carousel_preload_rx = preload_rx;
        self.preload_carousel_album_art(
            preload_tx,
            self.picker_base_url.clone(),
            self.picker_rpc_url.clone(),
            self.album_art_cache_dir.clone(),
        );
    }

    fn publish_configured_web_playlists(&self) {
        let discovered = self
            .web_playlist_catalog
            .lock()
            .map(|catalog| {
                catalog
                    .iter()
                    .filter(|playlist| !playlist.favorite)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let merged = merge_web_playlist_catalog(&self.picker_playlists, discovered);
        if let Ok(mut catalog) = self.web_playlist_catalog.lock() {
            *catalog = merged;
        }
    }

    fn refresh_web_playlist_catalog(&mut self) {
        if self.web_playlist_refresh_in_flight {
            return;
        }
        self.web_playlist_refresh_in_flight = true;
        let rpc_url = self.picker_rpc_url.clone();
        let configured = self.picker_playlists.clone();
        let updates = self.web_playlist_update_tx.clone();
        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(5))
                .build();
            let catalog = rpc_call_blocking(&agent, &rpc_url, "core.playlists.as_list", None)
                .map(|value| parse_web_playlist_refs(&value))
                .map(|discovered| merge_web_playlist_catalog(&configured, discovered));
            updates.send(catalog).ok();
        });
    }

    /// Update idle tracking and brightness level.
    fn update_brightness(&mut self) {
        if self.ui_mode == UiMode::Morning {
            self.brightness = self.settings.display_brightness;
            return;
        }
        if self.ui_mode == UiMode::Screensaver {
            if !self.screensaver_preview && (self.is_playing || !self.settings.screensaver_enabled)
            {
                self.wake_screensaver();
            } else {
                self.brightness = self.active_screensaver_brightness();
            }
            return;
        }

        // Never dim while the user is interacting with a menu.
        if self.ui_mode != UiMode::NowPlaying {
            self.brightness = self.settings.display_brightness;
            return;
        }

        if self.is_playing {
            // Playing — reset idle timer and restore brightness
            self.idle_since = None;
            self.brightness = self.settings.display_brightness;
        } else {
            // Not playing — start or continue idle timer
            let idle_start = self.idle_since.get_or_insert(Instant::now());
            let idle_secs = idle_start.elapsed().as_secs_f64();

            if self.settings.dim_timeout_seconds > 0
                && idle_secs >= self.settings.dim_timeout_seconds as f64
            {
                let screensaver_available = match self.settings.screensaver_style {
                    ScreensaverStyle::Albums => !self.screensaver_art_paths.is_empty(),
                    ScreensaverStyle::Clock => true,
                };
                if self.settings.screensaver_enabled && screensaver_available {
                    self.ui_mode = UiMode::Screensaver;
                    self.screensaver_offset = 0.0;
                    self.screensaver_preview = false;
                    if self.settings.screensaver_style == ScreensaverStyle::Albums {
                        self.prepare_screensaver_art_order();
                    }
                    self.brightness = self.active_screensaver_brightness();
                    println!(
                        "{} screensaver started (heavy_dim={})",
                        self.settings.screensaver_style.label(),
                        self.settings.screensaver_heavy_dim
                    );
                    self.publish_web_playback_status();
                } else {
                    self.brightness = self.settings.dim_brightness;
                }
            } else {
                self.brightness = self.settings.display_brightness;
            }
        }
    }

    fn active_screensaver_brightness(&self) -> f64 {
        let brightness = if self.settings.screensaver_heavy_dim {
            self.settings.dim_brightness
        } else {
            self.settings.screensaver_brightness
        };
        brightness.min(self.settings.display_brightness)
    }

    pub fn wake_screensaver(&mut self) {
        if self.ui_mode != UiMode::Screensaver {
            return;
        }
        self.ui_mode = UiMode::NowPlaying;
        self.screensaver_offset = 0.0;
        self.screensaver_preview = false;
        self.idle_since = if self.is_playing {
            None
        } else {
            Some(Instant::now())
        };
        self.brightness = self.settings.display_brightness;
        self.publish_web_playback_status();
        println!("Screensaver dismissed");
    }

    fn update_morning_mode(&mut self) {
        if let Some((day, hour)) = local_day_and_hour() {
            self.update_morning_mode_at(day, hour);
        }
    }

    fn update_morning_mode_at(&mut self, day: i32, hour: u8) {
        let in_window = self.settings.morning_mode_enabled
            && hour >= self.settings.morning_start_hour
            && hour < self.settings.morning_end_hour;
        if !in_window {
            if self.ui_mode == UiMode::Morning {
                self.ui_mode = UiMode::NowPlaying;
                self.idle_since = if self.is_playing {
                    None
                } else {
                    Some(Instant::now())
                };
                self.publish_web_playback_status();
            }
            return;
        }
        if self.morning_dismissed_day == Some(day)
            || !matches!(self.ui_mode, UiMode::NowPlaying | UiMode::Screensaver)
        {
            return;
        }
        self.ui_mode = UiMode::Morning;
        self.screensaver_preview = false;
        self.brightness = self.settings.display_brightness;
        self.publish_web_playback_status();
        println!("Good morning dashboard started");
    }

    pub fn dismiss_morning_mode(&mut self) {
        if self.ui_mode != UiMode::Morning {
            return;
        }
        if let Some((day, _)) = local_day_and_hour() {
            self.morning_dismissed_day = Some(day);
        }
        self.ui_mode = UiMode::NowPlaying;
        self.idle_since = if self.is_playing {
            None
        } else {
            Some(Instant::now())
        };
        self.brightness = self.settings.display_brightness;
        self.publish_web_playback_status();
        println!("Good morning dashboard dismissed");
    }

    pub fn morning_quote(&self) -> &'static str {
        let day = local_day_and_hour().map_or(0, |(day, _)| day as usize);
        MORNING_QUOTES[day % MORNING_QUOTES.len()]
    }

    pub fn toggle_screensaver_preview(&mut self) {
        if self.ui_mode == UiMode::Screensaver {
            self.wake_screensaver();
            return;
        }

        if self.settings.screensaver_style == ScreensaverStyle::Albums
            && self.screensaver_art_paths.is_empty()
        {
            println!("Cannot preview album screensaver: no artwork is cached yet");
            return;
        }

        self.ui_mode = UiMode::Screensaver;
        self.screensaver_offset = 0.0;
        self.screensaver_preview = true;
        if self.settings.screensaver_style == ScreensaverStyle::Albums {
            self.prepare_screensaver_art_order();
        }
        self.brightness = self.active_screensaver_brightness();
        self.publish_web_playback_status();
        println!(
            "{} screensaver preview started",
            self.settings.screensaver_style.label()
        );
    }

    // --- Active playback-source control wrappers ---

    pub fn toggle_play_pause(&mut self) {
        let sent = match self.active_source {
            PlaybackSource::Mopidy => self
                .mopidy_commands
                .send(MopidyCommand::TogglePlayPause)
                .map_err(|error| error.to_string()),
            PlaybackSource::Spotifyd => self
                .spotifyd_commands
                .send(SpotifydCommand::TogglePlayPause)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = sent {
            eprintln!(
                "{} worker is unavailable: {error}",
                self.active_source.label()
            );
            return;
        }

        // Immediately flip local state for responsive UI
        self.is_playing = !self.is_playing;
        self.publish_web_playback_status();
        let status = if self.is_playing { "Playing" } else { "Paused" };
        println!("{}", status);
    }

    pub fn skip_track(&mut self) {
        let sent = match self.active_source {
            PlaybackSource::Mopidy => self
                .mopidy_commands
                .send(MopidyCommand::NextTrack)
                .map_err(|error| error.to_string()),
            PlaybackSource::Spotifyd => self
                .spotifyd_commands
                .send(SpotifydCommand::NextTrack)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = sent {
            eprintln!(
                "{} worker is unavailable: {error}",
                self.active_source.label()
            );
            return;
        }
        self.current_time = 0.0;
        self.offset = 0;
        self.publish_web_playback_status();
        println!("Skipping to next track...");
    }

    pub fn previous_track(&mut self) {
        let sent = match self.active_source {
            PlaybackSource::Mopidy => self
                .mopidy_commands
                .send(MopidyCommand::PreviousTrack)
                .map_err(|error| error.to_string()),
            PlaybackSource::Spotifyd => self
                .spotifyd_commands
                .send(SpotifydCommand::PreviousTrack)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = sent {
            eprintln!(
                "{} worker is unavailable: {error}",
                self.active_source.label()
            );
            return;
        }
        self.current_time = 0.0;
        self.offset = 0;
        self.publish_web_playback_status();
        println!("Going to previous track...");
    }

    pub fn seek_to(&mut self, position_seconds: u64) {
        let duration = finite_nonnegative(self.current_track.duration);
        let position_seconds = if duration > 0.0 {
            position_seconds.min(duration.floor() as u64)
        } else {
            position_seconds
        };
        let position_ms = position_seconds.saturating_mul(1000);
        let sent = match self.active_source {
            PlaybackSource::Mopidy => self
                .mopidy_commands
                .send(MopidyCommand::Seek(position_ms))
                .map_err(|error| error.to_string()),
            PlaybackSource::Spotifyd => self
                .spotifyd_commands
                .send(SpotifydCommand::Seek(position_ms))
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = sent {
            eprintln!(
                "{} worker is unavailable: {error}",
                self.active_source.label()
            );
            return;
        }
        self.current_time = position_seconds as f64;
        self.publish_web_playback_status();
        println!("Seek: {position_seconds}s");
    }

    pub fn set_volume(&mut self, volume: u8) {
        let volume = volume.min(self.settings.max_volume);
        if let Some(timer) = self.sleep_timer.as_mut() {
            timer.set_base_volume(volume, Instant::now());
        }
        self.dispatch_volume(volume);
        self.volume = Some(volume);
        self.settings.playback_volume = volume;
        self.volume_save_due = Some(Instant::now());
        self.publish_web_playback_status();
        println!(
            "Master volume: {volume}% (maximum {}%)",
            self.settings.max_volume
        );
    }

    pub fn start_sleep_timer(&mut self, minutes: u64) {
        let minutes = minutes.clamp(1, 12 * 60);
        let base_volume = self.volume.unwrap_or(self.settings.playback_volume);
        self.sleep_timer = Some(SleepTimer::new(
            Duration::from_secs(minutes.saturating_mul(60)),
            base_volume,
            Instant::now(),
        ));
        self.sleep_timer_last_reported_seconds = Some(minutes.saturating_mul(60));
        self.publish_web_playback_status();
        println!("Sleep timer started for {minutes} minutes");
    }

    pub fn cancel_sleep_timer(&mut self) {
        let Some(timer) = self.sleep_timer.take() else {
            return;
        };
        let restore_volume = timer.base_volume().min(self.settings.max_volume);
        if self.volume != Some(restore_volume) {
            self.dispatch_volume(restore_volume);
            self.volume = Some(restore_volume);
        }
        self.sleep_timer_last_reported_seconds = None;
        self.publish_web_playback_status();
        println!("Sleep timer cancelled");
    }

    fn update_sleep_timer_at(&mut self, now: Instant) {
        let Some(mut timer) = self.sleep_timer.take() else {
            return;
        };
        let remaining = timer.remaining_seconds(now);
        let tick = timer.tick(now);

        if tick.expired {
            self.pause_for_sleep_timer();
            let restore_volume = timer.base_volume().min(self.settings.max_volume);
            if self.volume != Some(restore_volume) {
                // The pause and restore commands use the same per-backend
                // queues, preserving their order and avoiding a loud tail.
                self.dispatch_volume(restore_volume);
                self.volume = Some(restore_volume);
            }
            self.sleep_timer_last_reported_seconds = None;
            self.publish_web_playback_status();
            println!("Sleep timer elapsed; playback paused");
            return;
        }

        if let Some(volume) = tick.volume
            && self.volume != Some(volume)
        {
            // Timer fades are deliberately transient: they do not overwrite
            // the listener's persisted master volume.
            self.dispatch_volume(volume);
            self.volume = Some(volume);
        }
        self.sleep_timer = Some(timer);
        if self.sleep_timer_last_reported_seconds != Some(remaining) || tick.volume.is_some() {
            self.sleep_timer_last_reported_seconds = Some(remaining);
            self.publish_web_playback_status();
        }
    }

    fn pause_for_sleep_timer(&mut self) {
        if !self.is_playing {
            return;
        }
        let result = match self.active_source {
            PlaybackSource::Mopidy => self
                .mopidy_commands
                .send(MopidyCommand::Pause)
                .map_err(|error| error.to_string()),
            PlaybackSource::Spotifyd => self
                .spotifyd_commands
                .send(SpotifydCommand::Pause)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = result {
            eprintln!(
                "Could not pause {} for sleep timer: {error}",
                self.active_source.label()
            );
            return;
        }
        self.is_playing = false;
    }

    fn dispatch_volume(&mut self, volume: u8) {
        let volume = volume.min(self.settings.max_volume);
        if let Err(error) = self.mopidy_commands.send(MopidyCommand::SetVolume(volume)) {
            eprintln!("Mopidy worker is unavailable: {error}");
        }
        if let Err(error) = self
            .spotifyd_commands
            .send(SpotifydCommand::SetVolume(volume))
        {
            eprintln!("Spotify Connect worker is unavailable: {error}");
        }
        self.last_volume_command = Some((volume, Instant::now()));
    }

    fn observe_backend_volume(&mut self, source: PlaybackSource, reported: u8) {
        let safe_volume = reported.min(self.settings.max_volume);
        if let Some((expected, sent_at)) = self.last_volume_command {
            if reported == expected {
                self.last_volume_command = None;
                self.volume = Some(expected);
                return;
            }
            if sent_at.elapsed() < VOLUME_COMMAND_GRACE {
                // A poll can return the old value while a just-issued command
                // is still crossing the backend. Keep the safe master value.
                return;
            }
            self.last_volume_command = None;
        }

        if reported != safe_volume {
            // A configured software cap can only correct Spotify after the
            // phone has applied its value. The default native 0-100 range and
            // downstream hardware ceiling avoid that transient entirely.
            self.set_volume(safe_volume);
            return;
        }

        if self.volume != Some(safe_volume) {
            // This is an external change from the active source. Accept it as
            // the new master and update only the other backend. Echoing the
            // same value to Spotifyd can fight a phone while its slider moves.
            self.volume = Some(safe_volume);
            self.settings.playback_volume = safe_volume;
            if let Some(timer) = self.sleep_timer.as_mut() {
                timer.set_base_volume(safe_volume, Instant::now());
            }
            self.volume_save_due = Some(Instant::now());
            match source {
                PlaybackSource::Mopidy => {
                    self.spotifyd_commands
                        .send(SpotifydCommand::SetVolume(safe_volume))
                        .ok();
                }
                PlaybackSource::Spotifyd => {
                    self.mopidy_commands
                        .send(MopidyCommand::SetVolume(safe_volume))
                        .ok();
                }
            }
            self.publish_web_playback_status();
            println!("Master volume: {safe_volume}% (from {})", source.label());
        }
    }

    fn apply_hardware_volume_ceiling(&self) {
        let ceiling = format!("{:.1}dB", self.settings.hardware_volume_ceiling_db);
        match std::process::Command::new("amixer")
            .args([
                "-q",
                "-c",
                HARDWARE_MIXER_CARD,
                "sset",
                HARDWARE_MIXER_CONTROL,
                "--",
                &ceiling,
            ])
            .status()
        {
            Ok(status) if status.success() => println!(
                "Hardware volume ceiling: {} {}",
                HARDWARE_MIXER_CONTROL, ceiling
            ),
            Ok(status) => {
                eprintln!("Could not apply hardware volume ceiling (amixer exited with {status})")
            }
            Err(error) => eprintln!("Could not run amixer for hardware volume ceiling: {error}"),
        }
    }

    // --- Main menu and settings ---

    pub fn enter_menu(&mut self) {
        self.ui_mode = UiMode::MainMenu;
        self.menu_index = 0;
        self.brightness = self.settings.display_brightness;
        println!("Entering main menu");
    }

    pub fn exit_menu(&mut self) {
        self.ui_mode = UiMode::NowPlaying;
        println!("Exited main menu");
    }

    pub fn menu_next(&mut self) {
        self.menu_index = (self.menu_index + 1) % MAIN_MENU_ITEMS.len();
    }

    pub fn menu_previous(&mut self) {
        self.menu_index = if self.menu_index == 0 {
            MAIN_MENU_ITEMS.len() - 1
        } else {
            self.menu_index - 1
        };
    }

    pub fn current_menu_item(&self) -> MenuItem {
        MAIN_MENU_ITEMS[self.menu_index.min(MAIN_MENU_ITEMS.len() - 1)]
    }

    pub fn menu_item_value(&self, item: MenuItem) -> String {
        match item {
            MenuItem::Playlists => self.picker_playlists.len().to_string(),
            MenuItem::Volume => self
                .volume
                .map(|volume| format!("{volume}%"))
                .unwrap_or_else(|| "--".to_string()),
            MenuItem::Brightness => {
                format!("{}%", (self.settings.display_brightness * 100.0).round())
            }
            MenuItem::DimTimeout => format_dim_timeout(self.settings.dim_timeout_seconds),
            MenuItem::IdleDisplay => self.idle_display_mode().label().to_string(),
            MenuItem::ScreensaverStyle => self.settings.screensaver_style.label().to_string(),
            MenuItem::ScreensaverShuffle => {
                if self.settings.shuffle_screensaver_art {
                    "On".to_string()
                } else {
                    "Off".to_string()
                }
            }
            MenuItem::CarouselSpeed => {
                if self.settings.carousel_speed == 0.0 {
                    "Paused".to_string()
                } else {
                    format!("{:.0} px/s", self.settings.carousel_speed)
                }
            }
            MenuItem::PlaylistShuffle => {
                if self.settings.shuffle_playlists {
                    "On".to_string()
                } else {
                    "Off".to_string()
                }
            }
            MenuItem::NowPlayingStyle => self.settings.now_playing_style.label().to_string(),
            MenuItem::VisualizerDelay => format!("{} ms", self.settings.visualizer_delay_ms),
            MenuItem::SpotifyVisualizerDelay => {
                format!("+{} ms", self.settings.spotify_visualizer_extra_delay_ms)
            }
            MenuItem::GuestQr | MenuItem::Diagnostics | MenuItem::Exit => String::new(),
        }
    }

    pub fn menu_select(&mut self) {
        match self.current_menu_item() {
            MenuItem::Playlists => self.enter_picker(),
            MenuItem::Volume => self.ui_mode = UiMode::SettingEditor(SettingKind::Volume),
            MenuItem::Brightness => self.ui_mode = UiMode::SettingEditor(SettingKind::Brightness),
            MenuItem::DimTimeout => self.ui_mode = UiMode::SettingEditor(SettingKind::DimTimeout),
            MenuItem::IdleDisplay => {
                self.cycle_idle_display_mode();
                self.persist_settings();
            }
            MenuItem::ScreensaverStyle => {
                self.settings.screensaver_style = match self.settings.screensaver_style {
                    ScreensaverStyle::Albums => ScreensaverStyle::Clock,
                    ScreensaverStyle::Clock => ScreensaverStyle::Albums,
                };
                self.persist_settings();
            }
            MenuItem::ScreensaverShuffle => {
                self.settings.shuffle_screensaver_art = !self.settings.shuffle_screensaver_art;
                self.persist_settings();
            }
            MenuItem::CarouselSpeed => {
                self.ui_mode = UiMode::SettingEditor(SettingKind::CarouselSpeed)
            }
            MenuItem::PlaylistShuffle => {
                self.settings.shuffle_playlists = !self.settings.shuffle_playlists;
                self.persist_settings();
            }
            MenuItem::NowPlayingStyle => {
                self.settings.now_playing_style = match self.settings.now_playing_style {
                    NowPlayingStyle::Artwork => NowPlayingStyle::Visualizer,
                    NowPlayingStyle::Visualizer => NowPlayingStyle::TrackInfo,
                    NowPlayingStyle::TrackInfo => NowPlayingStyle::Artwork,
                };
                self.persist_settings();
            }
            MenuItem::VisualizerDelay => {
                self.ui_mode = UiMode::SettingEditor(SettingKind::VisualizerDelay)
            }
            MenuItem::SpotifyVisualizerDelay => {
                self.ui_mode = UiMode::SettingEditor(SettingKind::SpotifyVisualizerDelay)
            }
            MenuItem::GuestQr => self.ui_mode = UiMode::GuestQr,
            MenuItem::Diagnostics => self.ui_mode = UiMode::Diagnostics,
            MenuItem::Exit => self.exit_menu(),
        }
    }

    pub fn idle_display_mode(&self) -> IdleDisplayMode {
        if !self.settings.screensaver_enabled {
            IdleDisplayMode::Dim
        } else if self.settings.screensaver_heavy_dim {
            IdleDisplayMode::DimmedScreensaver
        } else {
            IdleDisplayMode::Screensaver
        }
    }

    fn cycle_idle_display_mode(&mut self) {
        match self.idle_display_mode() {
            IdleDisplayMode::Dim => {
                self.settings.screensaver_enabled = true;
                self.settings.screensaver_heavy_dim = false;
            }
            IdleDisplayMode::Screensaver => {
                self.settings.screensaver_enabled = true;
                self.settings.screensaver_heavy_dim = true;
            }
            IdleDisplayMode::DimmedScreensaver => {
                self.settings.screensaver_enabled = false;
                self.settings.screensaver_heavy_dim = false;
            }
        }
    }

    pub fn adjust_setting(&mut self, direction: i32) {
        let UiMode::SettingEditor(setting) = self.ui_mode else {
            return;
        };
        let direction = direction.signum();
        if direction == 0 {
            return;
        }

        match setting {
            SettingKind::Volume => {
                let current = self.volume.unwrap_or(50) as i32;
                let step = self.settings.volume_step as i32;
                let volume = (current + direction * step).clamp(0, 100) as u8;
                self.set_volume(volume);
            }
            SettingKind::Brightness => {
                let current_step = (self.settings.display_brightness * 10.0).round() as i32;
                let next_step = (current_step + direction).clamp(1, 10);
                self.settings.display_brightness = next_step as f64 / 10.0;
                self.settings.dim_brightness = self
                    .settings
                    .dim_brightness
                    .min(self.settings.display_brightness);
                self.brightness = self.settings.display_brightness;
            }
            SettingKind::DimTimeout => {
                let current_index = DIM_TIMEOUT_OPTIONS
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, value)| self.settings.dim_timeout_seconds.abs_diff(**value))
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                let next_index = if direction > 0 {
                    (current_index + 1) % DIM_TIMEOUT_OPTIONS.len()
                } else if current_index == 0 {
                    DIM_TIMEOUT_OPTIONS.len() - 1
                } else {
                    current_index - 1
                };
                self.settings.dim_timeout_seconds = DIM_TIMEOUT_OPTIONS[next_index];
            }
            SettingKind::VisualizerDelay => {
                let current = self.settings.visualizer_delay_ms as i64;
                self.settings.visualizer_delay_ms =
                    (current + i64::from(direction) * VISUALIZER_DELAY_STEP_MS as i64)
                        .clamp(0, MAX_VISUALIZER_DELAY_MS as i64) as u64;
            }
            SettingKind::SpotifyVisualizerDelay => {
                let current = self.settings.spotify_visualizer_extra_delay_ms as i64;
                self.settings.spotify_visualizer_extra_delay_ms =
                    (current + i64::from(direction) * VISUALIZER_DELAY_STEP_MS as i64)
                        .clamp(0, MAX_VISUALIZER_DELAY_MS as i64) as u64;
            }
            SettingKind::CarouselSpeed => {
                self.settings.carousel_speed = (self.settings.carousel_speed
                    + f64::from(direction) * CAROUSEL_SPEED_STEP)
                    .clamp(0.0, MAX_CAROUSEL_SPEED);
            }
        }
    }

    pub fn setting_value(&self, setting: SettingKind) -> String {
        match setting {
            SettingKind::Volume => self
                .volume
                .map(|volume| format!("{volume}%"))
                .unwrap_or_else(|| "--".to_string()),
            SettingKind::Brightness => {
                format!("{}%", (self.settings.display_brightness * 100.0).round())
            }
            SettingKind::DimTimeout => format_dim_timeout(self.settings.dim_timeout_seconds),
            SettingKind::VisualizerDelay => {
                format!("{} ms", self.settings.visualizer_delay_ms)
            }
            SettingKind::SpotifyVisualizerDelay => {
                format!("+{} ms", self.settings.spotify_visualizer_extra_delay_ms)
            }
            SettingKind::CarouselSpeed => {
                if self.settings.carousel_speed == 0.0 {
                    "Paused".to_string()
                } else {
                    format!("{:.0} px/s", self.settings.carousel_speed)
                }
            }
        }
    }

    pub fn setting_label(setting: SettingKind) -> &'static str {
        match setting {
            SettingKind::Volume => "Volume",
            SettingKind::Brightness => "Brightness",
            SettingKind::DimTimeout => "Dim timeout",
            SettingKind::VisualizerDelay => "Visualizer delay",
            SettingKind::SpotifyVisualizerDelay => "Spotify vis offset",
            SettingKind::CarouselSpeed => "Carousel speed",
        }
    }

    pub fn leave_submenu(&mut self) {
        if matches!(
            self.ui_mode,
            UiMode::SettingEditor(
                SettingKind::Brightness
                    | SettingKind::DimTimeout
                    | SettingKind::VisualizerDelay
                    | SettingKind::SpotifyVisualizerDelay
                    | SettingKind::CarouselSpeed
            )
        ) {
            self.persist_settings();
        }
        self.ui_mode = UiMode::MainMenu;
    }

    fn persist_settings(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        if let Err(e) = self.settings.save(path) {
            eprintln!("Failed to save settings: {e}");
        }
    }

    // --- Playlist picker ---

    /// Enter the playlist picker mode.
    pub fn enter_picker(&mut self) {
        if self.picker_playlists.is_empty() {
            println!("No playlists configured in playlists.toml");
            return;
        }
        self.ui_mode = UiMode::PlaylistPicker;
        self.picker_index = 0;
        self.brightness = self.settings.display_brightness;
        self.resolve_picker_art();
        self.picker_last_scroll = Instant::now();
        println!(
            "Entering playlist picker ({} playlists)",
            self.picker_playlists.len()
        );
    }

    /// Scroll to the next playlist in the picker.
    pub fn picker_scroll(&mut self) {
        if self.picker_playlists.is_empty() {
            return;
        }
        self.picker_index = (self.picker_index + 1) % self.picker_playlists.len();
        println!(
            "Picker: scrolled to {} ({}/{})",
            self.picker_playlists[self.picker_index].name,
            self.picker_index + 1,
            self.picker_playlists.len()
        );
        self.resolve_picker_art();
        self.picker_last_scroll = Instant::now();
    }

    pub fn picker_previous(&mut self) {
        if self.picker_playlists.is_empty() {
            return;
        }
        self.picker_index = if self.picker_index == 0 {
            self.picker_playlists.len() - 1
        } else {
            self.picker_index - 1
        };
        self.resolve_picker_art();
        self.picker_last_scroll = Instant::now();
    }

    /// Resolve picker art from the preloaded cache. No network requests.
    /// If the preload hasn't finished yet for this index, art_path stays empty (placeholder).
    fn resolve_picker_art(&mut self) {
        self.picker_art_path.clear();

        if let Some(Some(path)) = self.picker_art_cache.get(self.picker_index)
            && std::path::Path::new(path).exists()
        {
            self.picker_art_path = path.clone();
        }
    }

    /// Select the current playlist and start playing it. Returns to NowPlaying.
    pub fn picker_select(&mut self) {
        if self.picker_playlists.is_empty() {
            self.ui_mode = UiMode::NowPlaying;
            return;
        }

        let entry = self.picker_playlists[self.picker_index].clone();
        self.start_playlist(entry);
    }

    fn start_playlist(&mut self, entry: PlaylistEntry) {
        self.yield_spotifyd_to_mopidy();
        let name = entry.name;
        let uri = entry.uri;
        println!("Selected playlist: {} ({})", name, uri);
        if let Err(e) = self.mopidy_commands.send(MopidyCommand::PlayPlaylist {
            uri,
            display_name: name,
            shuffle: self.settings.shuffle_playlists,
        }) {
            eprintln!("Mopidy worker is unavailable: {}", e);
            return;
        }
        self.ui_mode = UiMode::NowPlaying;
        self.offset = 0;
        self.current_time = 0.0;
    }

    /// Exit the picker without selecting.
    pub fn exit_picker(&mut self) {
        self.ui_mode = UiMode::MainMenu;
        println!("Exited playlist picker");
    }

    /// Get the currently highlighted playlist entry (for rendering).
    pub fn current_picker_entry(&self) -> Option<&PlaylistEntry> {
        self.picker_playlists.get(self.picker_index)
    }
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds as u32;
    let m = total / 60;
    let s = total % 60;
    format!("{}:{:02}", m, s)
}

pub fn format_dim_timeout(seconds: u64) -> String {
    match seconds {
        0 => "Off".to_string(),
        1..=59 => format!("{seconds}s"),
        _ if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
        _ => format!("{seconds}s"),
    }
}

fn shuffle_indices(indices: &mut [usize], mut state: u64) {
    if state == 0 {
        state = 0xA076_1D64_78BD_642F;
    }
    for upper in (1..indices.len()).rev() {
        // Xorshift64 is sufficient here: this is visual variety, not security.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let other = (state as usize) % (upper + 1);
        indices.swap(upper, other);
    }
}

// --- Background art fetch helpers (run in spawned threads) ---

use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static BG_REQUEST_ID: AtomicU64 = AtomicU64::new(10000);

fn rpc_call_blocking(
    agent: &ureq::Agent,
    rpc_url: &str,
    method: &str,
    params: Option<Value>,
) -> Option<Value> {
    let id = BG_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    });
    if let Some(params) = params {
        body["params"] = params;
    }

    let response = agent
        .post(rpc_url)
        .set("Content-Type", "application/json")
        .send_json(&body)
        .ok()?;
    response.into_json::<Value>().ok()?.get("result").cloned()
}

/// Gather album-bearing tracks in useful order: the active queue, recent
/// history, then a round-robin sample of configured playlists.
fn collect_album_seed_tracks_blocking(
    agent: &ureq::Agent,
    rpc_url: &str,
    playlists: &[PlaylistEntry],
) -> Vec<Value> {
    let mut tracks = Vec::new();

    if let Some(queue) = rpc_call_blocking(agent, rpc_url, "core.tracklist.get_tl_tracks", None)
        .and_then(|value| value.as_array().cloned())
    {
        tracks.extend(
            queue
                .into_iter()
                .filter_map(|tl_track| tl_track.get("track").cloned()),
        );
    }

    let history_uris: Vec<String> =
        rpc_call_blocking(agent, rpc_url, "core.history.get_history", None)
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry
                    .as_array()?
                    .get(1)?
                    .get("uri")?
                    .as_str()
                    .map(str::to_string)
            })
            .take(50)
            .collect();

    if !history_uris.is_empty()
        && let Some(lookup) = rpc_call_blocking(
            agent,
            rpc_url,
            "core.library.lookup",
            Some(json!({ "uris": history_uris })),
        )
    {
        for uri in &history_uris {
            if let Some(track) = lookup
                .get(uri)
                .and_then(Value::as_array)
                .and_then(|matches| matches.first())
            {
                tracks.push(track.clone());
            }
        }
    }

    let mut playlist_tracks = Vec::new();
    for playlist in playlists {
        let Some(result) = rpc_call_blocking(
            agent,
            rpc_url,
            "core.playlists.lookup",
            Some(json!({ "uri": playlist.uri })),
        ) else {
            continue;
        };
        if let Some(entries) = result.get("tracks").and_then(Value::as_array) {
            playlist_tracks.push(entries.clone());
        }
    }

    let longest_playlist = playlist_tracks.iter().map(Vec::len).max().unwrap_or(0);
    for track_index in 0..longest_playlist {
        for playlist in &playlist_tracks {
            if let Some(track) = playlist.get(track_index) {
                tracks.push(track.clone());
            }
        }
    }

    tracks
}

fn parse_web_playlist_refs(value: &Value) -> Vec<WebPlaylist> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "playlist")
        })
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.trim();
            let uri = entry.get("uri")?.as_str()?.trim();
            if name.is_empty() || uri.is_empty() {
                return None;
            }
            Some(WebPlaylist {
                name: name.to_string(),
                uri: uri.to_string(),
                favorite: false,
            })
        })
        .collect()
}

fn merge_web_playlist_catalog(
    configured: &[PlaylistEntry],
    mut discovered: Vec<WebPlaylist>,
) -> Vec<WebPlaylist> {
    let mut seen_uris = HashSet::new();
    let mut merged = Vec::new();

    for playlist in configured {
        let name = playlist.name.trim();
        let uri = playlist.uri.trim();
        if name.is_empty() || uri.is_empty() || !seen_uris.insert(uri.to_string()) {
            continue;
        }
        merged.push(WebPlaylist {
            name: name.to_string(),
            uri: uri.to_string(),
            favorite: true,
        });
    }

    discovered.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.uri.cmp(&right.uri))
    });
    for mut playlist in discovered {
        playlist.name = playlist.name.trim().to_string();
        playlist.uri = playlist.uri.trim().to_string();
        playlist.favorite = false;
        if playlist.name.is_empty()
            || playlist.uri.is_empty()
            || !seen_uris.insert(playlist.uri.clone())
        {
            continue;
        }
        merged.push(playlist);
    }

    merged
}

fn normalized_album_key(artist: &str, album: &str) -> String {
    let normalize = |value: &str| {
        value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    format!("{}\u{1f}{}", normalize(artist), normalize(album))
}

fn track_album_identity(track: &Value) -> Option<(String, String)> {
    let track_uri = track.get("uri")?.as_str()?.to_string();
    let album = track.get("album")?;
    let album_name = album.get("name")?.as_str()?.trim();
    if track_uri.is_empty() || album_name.is_empty() {
        return None;
    }
    let artist = album
        .get("artists")
        .and_then(Value::as_array)
        .and_then(|artists| artists.first())
        .and_then(|artist| artist.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            track
                .get("artists")
                .and_then(Value::as_array)
                .and_then(|artists| artists.first())
                .and_then(|artist| artist.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");

    Some((track_uri, normalized_album_key(artist, album_name)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtworkFingerprint {
    raw_hash: u64,
    perceptual: Option<PerceptualArtworkFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerceptualArtworkFingerprint {
    difference_hash: u64,
    average_rgb: [u8; 3],
}

fn artwork_file_fingerprint(path: &Path) -> Option<ArtworkFingerprint> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let perceptual = image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| reader.decode().ok())
        .map(|image| {
            let sample = image
                .resize_exact(9, 8, image::imageops::FilterType::Triangle)
                .to_rgb8();
            let mut difference_hash = 0_u64;
            let mut bit = 0_u32;
            for y in 0..8 {
                for x in 0..8 {
                    let left = sample.get_pixel(x, y).0;
                    let right = sample.get_pixel(x + 1, y).0;
                    let luminance = |rgb: [u8; 3]| {
                        u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114
                    };
                    if luminance(left) > luminance(right) {
                        difference_hash |= 1_u64 << bit;
                    }
                    bit += 1;
                }
            }
            let average = image
                .resize_exact(1, 1, image::imageops::FilterType::Triangle)
                .to_rgb8()
                .get_pixel(0, 0)
                .0;
            PerceptualArtworkFingerprint {
                difference_hash,
                average_rgb: average,
            }
        });
    Some(ArtworkFingerprint {
        raw_hash: hash,
        perceptual,
    })
}

fn artwork_fingerprints_match(left: ArtworkFingerprint, right: ArtworkFingerprint) -> bool {
    match (left.perceptual, right.perceptual) {
        (Some(left), Some(right)) => {
            let hash_distance = (left.difference_hash ^ right.difference_hash).count_ones();
            let color_distance: u16 = left
                .average_rgb
                .into_iter()
                .zip(right.average_rgb)
                .map(|(left, right)| u16::from(left.abs_diff(right)))
                .sum();
            hash_distance <= 6 && color_distance <= 48
        }
        _ => left.raw_hash == right.raw_hash,
    }
}

/// Fetch art image from a URI, caching to disk. Returns the local file path.
fn fetch_art_blocking(
    agent: &ureq::Agent,
    base_url: &str,
    cache_dir: &Path,
    art_uri: &str,
) -> Option<String> {
    std::fs::create_dir_all(cache_dir).ok()?;
    let hash = simple_hash(art_uri);
    let cache_file = cache_dir.join(format!("{}.jpg", hash));

    if cache_file.exists() {
        return Some(cache_file.to_string_lossy().to_string());
    }

    let url = if art_uri.starts_with("http://") || art_uri.starts_with("https://") {
        art_uri.to_string()
    } else {
        format!("{}{}", base_url, art_uri)
    };

    match agent.get(&url).call() {
        Ok(resp) => {
            let mut bytes = Vec::new();
            use std::io::Read;
            if resp.into_reader().read_to_end(&mut bytes).is_ok()
                && std::fs::write(&cache_file, &bytes).is_ok()
            {
                return Some(cache_file.to_string_lossy().to_string());
            }
            None
        }
        Err(_) => None,
    }
}

/// Query Mopidy for an image URI associated with a given URI (playlist, track, etc.)
fn get_image_uri_blocking(agent: &ureq::Agent, rpc_url: &str, uri: &str) -> Option<String> {
    let result = rpc_call_blocking(
        agent,
        rpc_url,
        "core.library.get_images",
        Some(json!({ "uris": [uri] })),
    )?;
    let images = result.get(uri)?.as_array()?;
    let image = images.first()?;
    let image_uri = image.get("uri")?.as_str()?;

    Some(image_uri.to_string())
}

fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    hash
}

fn scan_cached_art(cache_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    let mut images: Vec<_> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path.to_string_lossy().to_string()))
        })
        .collect();
    images.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    let mut fingerprints = Vec::new();
    let mut unique = Vec::new();
    for (_, path) in images {
        let fingerprint = artwork_file_fingerprint(Path::new(&path));
        if let Some(fingerprint) = fingerprint {
            if fingerprints
                .iter()
                .any(|known| artwork_fingerprints_match(*known, fingerprint))
            {
                continue;
            }
            fingerprints.push(fingerprint);
        }
        unique.push(path);
        if unique.len() == MAX_SCREENSAVER_ART {
            break;
        }
    }
    unique.reverse();
    unique
}

#[cfg(test)]
impl AppState {
    fn new_for_test(
        config: Config,
    ) -> (
        Self,
        Receiver<MopidyCommand>,
        Sender<Option<MopidySnapshot>>,
    ) {
        let (mopidy_commands, command_rx) = mpsc::channel();
        let (update_tx, mopidy_updates) = mpsc::channel();
        let (spotifyd_commands, _spotifyd_command_rx) = mpsc::channel();
        let (_spotifyd_update_tx, spotifyd_updates) = mpsc::channel();
        let (_preload_tx, picker_preload_rx) = mpsc::channel();
        let (_carousel_preload_tx, carousel_preload_rx) = mpsc::channel();
        let (_weather_tx, weather_updates) = mpsc::channel();
        let (_news_tx, news_updates) = mpsc::channel();
        let (_web_tx, web_updates) = mpsc::channel();
        let web_playback_status = Arc::new(Mutex::new(WebPlaybackStatus::default()));
        let web_playlist_catalog = Arc::new(Mutex::new(merge_web_playlist_catalog(
            &config.playlists,
            Vec::new(),
        )));
        let web_queue = Arc::new(Mutex::new(Vec::new()));
        let web_history = Arc::new(Mutex::new(Vec::new()));
        let (web_playlist_update_tx, web_playlist_updates) = mpsc::channel();
        let (_visualizer_tx, visualizer_updates) = mpsc::channel();
        let num_playlists = config.playlists.len();
        let settings = Settings::default();

        let state = Self {
            current_track: Track::idle(),
            next_track: None,
            current_time: 0.0,
            is_playing: false,
            offset: 0,
            brightness: settings.display_brightness,
            volume: None,
            settings,
            settings_path: None,
            last_volume_command: None,
            volume_save_due: None,
            sleep_timer: None,
            sleep_timer_last_reported_seconds: None,
            ui_mode: UiMode::NowPlaying,
            menu_index: 0,
            picker_index: 0,
            picker_playlists: config.playlists,
            picker_art_path: String::new(),
            picker_last_scroll: Instant::now(),
            screensaver_art_paths: Vec::new(),
            screensaver_art_order: Vec::new(),
            screensaver_art_album_keys: Vec::new(),
            screensaver_art_fingerprints: Vec::new(),
            screensaver_offset: 0.0,
            screensaver_preview: false,
            screensaver_shuffle_nonce: 0,
            weather: None,
            weather_error: None,
            weather_updates,
            news: None,
            news_error: None,
            news_updates,
            morning_dismissed_day: None,
            web_updates,
            web_playback_status,
            web_playlist_catalog,
            web_queue,
            web_history,
            history_store: None,
            web_playlist_updates,
            web_playlist_update_tx,
            web_playlist_refresh_in_flight: false,
            last_web_playlist_refresh: Instant::now(),
            visualizer_updates,
            visualizer_pending: VecDeque::new(),
            visualizer_levels: [0.0; BAR_COUNT],
            last_visualizer_update: None,
            picker_art_cache: vec![None; num_playlists],
            picker_preload_rx,
            carousel_preload_rx,
            picker_base_url: String::new(),
            picker_rpc_url: String::new(),
            picker_art_cache_dir: PathBuf::new(),
            album_art_cache_dir: PathBuf::new(),
            mopidy_commands,
            mopidy_updates,
            last_mopidy_snapshot: None,
            poll_in_flight: false,
            last_poll: Instant::now(),
            last_track_uri: None,
            idle_since: Some(Instant::now()),
            mopidy_online: false,
            last_mopidy_update: None,
            spotifyd_commands,
            spotifyd_updates,
            spotifyd_yielded: false,
            last_spotifyd_playback_state: None,
            active_source: PlaybackSource::Mopidy,
            spotifyd_available: false,
            last_spotifyd_update: None,
        };

        (state, command_rx, update_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buttons::{ButtonAction, apply_action};
    use crate::mopidy::{MopidyTrackInfo, QueuePlacement};
    use std::sync::mpsc::TryRecvError;

    fn snapshot(title: &str) -> MopidySnapshot {
        MopidySnapshot {
            playback_state: PlaybackState::Playing,
            time_position_ms: 12_500,
            volume: Some(35),
            track: Some(MopidyTrackInfo {
                uri: format!("test:track:{title}"),
                title: title.to_string(),
                artist: "Test Artist".to_string(),
                album: "Test Album".to_string(),
                duration_ms: 60_000,
                art_uri: Some("http://example.invalid/art.jpg".to_string()),
            }),
            next_track: Some(MopidyTrackInfo {
                uri: "test:track:next".to_string(),
                title: "Coming Next".to_string(),
                artist: "Next Artist".to_string(),
                album: "Next Album".to_string(),
                duration_ms: 180_000,
                art_uri: None,
            }),
            album_art_path: "/tmp/test-art.jpg".to_string(),
            queue: Vec::new(),
        }
    }

    #[test]
    fn poll_requests_are_coalesced_and_completed_asynchronously() {
        let (mut state, commands, updates) = AppState::new_for_test(Config::default());

        state.poll_mopidy();
        assert_eq!(commands.recv().unwrap(), MopidyCommand::Poll);

        state.poll_mopidy();
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));

        updates.send(Some(snapshot("Async track"))).unwrap();
        state.advance_time(0.0);

        assert!(!state.poll_in_flight);
        assert!(state.is_playing);
        assert_eq!(state.current_time, 12.5);
        assert_eq!(state.volume, Some(35));
        assert!(state.mopidy_online);
        assert_eq!(state.current_track.title, "Async track");
        assert_eq!(state.current_track.album_art_path, "/tmp/test-art.jpg");
        assert_eq!(state.next_track.as_ref().unwrap().title, "Coming Next");
        let web_status = state.web_playback_status.lock().unwrap();
        assert_eq!(web_status.title, "Async track");
        assert!(web_status.is_playing);
        assert!(web_status.online);
        assert_eq!(web_status.volume, Some(35));
        assert_eq!(web_status.position_seconds, 12.5);
        assert_eq!(web_status.duration_seconds, 60.0);
        assert_eq!(web_status.next_title.as_deref(), Some("Coming Next"));
        assert_eq!(web_status.next_artist.as_deref(), Some("Next Artist"));
        assert_eq!(
            web_status.artwork_path.as_deref(),
            Some("/tmp/test-art.jpg")
        );
        assert!(web_status.artwork_id.is_some());
    }

    #[test]
    fn offline_mopidy_polling_uses_a_quiet_retry_interval() {
        assert_eq!(mopidy_poll_interval(true), 0.2);
        assert_eq!(mopidy_poll_interval(false), 2.0);
    }

    #[test]
    fn spotify_takeover_requires_a_real_playback_transition() {
        assert!(spotify_playback_started(
            None,
            SpotifydPlaybackState::Playing
        ));
        assert!(spotify_playback_started(
            Some(SpotifydPlaybackState::Paused),
            SpotifydPlaybackState::Playing
        ));
        assert!(!spotify_playback_started(
            Some(SpotifydPlaybackState::Playing),
            SpotifydPlaybackState::Playing
        ));
        assert!(!spotify_playback_started(
            Some(SpotifydPlaybackState::Playing),
            SpotifydPlaybackState::Paused
        ));
    }

    #[test]
    fn failed_poll_keeps_the_last_known_playback_state() {
        let (mut state, _commands, updates) = AppState::new_for_test(Config::default());
        updates.send(Some(snapshot("Known track"))).unwrap();
        state.advance_time(0.0);

        updates.send(None).unwrap();
        state.advance_time(0.0);

        assert!(state.is_playing);
        assert!(!state.mopidy_online);
        assert_eq!(state.current_track.title, "Known track");
        assert_eq!(state.current_time, 12.5);
        let web_status = state.web_playback_status.lock().unwrap();
        assert!(!web_status.online);
        assert_eq!(web_status.title, "Known track");
        assert_eq!(web_status.position_seconds, 12.5);
    }

    #[test]
    fn playback_controls_enqueue_work_without_waiting_for_results() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());

        state.toggle_play_pause();
        assert!(state.is_playing);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::TogglePlayPause);

        state.current_time = 42.0;
        state.publish_web_playback_status();
        state.skip_track();
        assert_eq!(state.current_time, 0.0);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::NextTrack);
        assert_eq!(
            state.web_playback_status.lock().unwrap().position_seconds,
            0.0
        );

        state.current_track.duration = 90.0;
        state.seek_to(75);
        assert_eq!(state.current_time, 75.0);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::Seek(75_000));
        assert_eq!(
            state.web_playback_status.lock().unwrap().position_seconds,
            75.0
        );

        state.current_time = 17.0;
        state.publish_web_playback_status();
        state.previous_track();
        assert_eq!(commands.recv().unwrap(), MopidyCommand::PreviousTrack);
        assert_eq!(
            state.web_playback_status.lock().unwrap().position_seconds,
            0.0
        );
    }

    #[test]
    fn web_playback_actions_use_the_existing_async_controls() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());

        state.apply_web_config_update(WebConfigUpdate::Playback(
            WebPlaybackAction::TogglePlayPause,
        ));
        assert!(state.is_playing);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::TogglePlayPause);

        state.apply_web_config_update(WebConfigUpdate::Playback(WebPlaybackAction::SetVolume(42)));
        assert_eq!(state.volume, Some(42));
        assert_eq!(commands.recv().unwrap(), MopidyCommand::SetVolume(42));
        state.current_track.duration = 60.0;
        state.apply_web_config_update(WebConfigUpdate::Playback(WebPlaybackAction::Seek(90)));
        assert_eq!(state.current_time, 60.0);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::Seek(60_000));
        let web_status = state.web_playback_status.lock().unwrap();
        assert!(web_status.is_playing);
        assert_eq!(web_status.volume, Some(42));
    }

    #[test]
    fn sleep_timer_fades_pauses_and_restores_the_master_volume() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());
        state.volume = Some(60);
        state.settings.playback_volume = 60;
        state.is_playing = true;
        let started_at = Instant::now();

        state.apply_web_config_update(WebConfigUpdate::Playback(
            WebPlaybackAction::StartSleepTimer(15),
        ));
        state.update_sleep_timer_at(started_at + Duration::from_secs(14 * 60 + 31));

        let MopidyCommand::SetVolume(faded_volume) = commands.recv().unwrap() else {
            panic!("expected final-minute volume fade");
        };
        assert!(faded_volume < 60);
        assert!(state.is_playing);

        state.update_sleep_timer_at(started_at + Duration::from_secs(16 * 60));

        assert_eq!(commands.recv().unwrap(), MopidyCommand::Pause);
        assert_eq!(commands.recv().unwrap(), MopidyCommand::SetVolume(60));
        assert!(!state.is_playing);
        assert_eq!(state.volume, Some(60));
        let web_status = state.web_playback_status.lock().unwrap();
        assert_eq!(web_status.sleep_timer_remaining_seconds, None);
        assert!(!web_status.sleep_timer_fading);
    }

    #[test]
    fn master_volume_is_clamped_and_sent_to_both_players() {
        let (mut state, mopidy_commands, _updates) = AppState::new_for_test(Config::default());
        let (spotifyd_commands, spotifyd_updates) = mpsc::channel();
        state.spotifyd_commands = spotifyd_commands;
        state.settings.max_volume = 60;

        state.set_volume(95);

        assert_eq!(state.volume, Some(60));
        assert_eq!(state.settings.playback_volume, 60);
        assert_eq!(
            mopidy_commands.recv().unwrap(),
            MopidyCommand::SetVolume(60)
        );
        assert_eq!(
            spotifyd_updates.recv().unwrap(),
            SpotifydCommand::SetVolume(60)
        );
        let web_status = state.web_playback_status.lock().unwrap();
        assert_eq!(web_status.volume, Some(60));
        assert_eq!(web_status.max_volume, 60);
    }

    #[test]
    fn spotify_volume_changes_are_mirrored_without_echoing_them_back() {
        let (mut state, mopidy_commands, _updates) = AppState::new_for_test(Config::default());
        let (spotifyd_commands, spotifyd_updates) = mpsc::channel();
        state.spotifyd_commands = spotifyd_commands;
        state.volume = Some(70);

        state.observe_backend_volume(PlaybackSource::Spotifyd, 84);

        assert_eq!(state.volume, Some(84));
        assert_eq!(state.settings.playback_volume, 84);
        assert_eq!(
            mopidy_commands.recv().unwrap(),
            MopidyCommand::SetVolume(84)
        );
        assert_eq!(spotifyd_updates.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn inactive_mopidy_is_realigned_before_a_future_source_switch() {
        let (mut state, mopidy_commands, updates) = AppState::new_for_test(Config::default());
        state.active_source = PlaybackSource::Spotifyd;
        state.is_playing = true;
        state.volume = Some(44);
        let mut stale = snapshot("Parked source");
        stale.volume = Some(70);

        updates.send(Some(stale)).unwrap();
        state.advance_time(0.0);

        assert_eq!(
            mopidy_commands.recv().unwrap(),
            MopidyCommand::SetVolume(44)
        );
        assert_eq!(state.volume, Some(44));
    }

    #[test]
    fn web_playlist_play_uses_the_queue_replacement_command() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());
        state.current_time = 48.0;

        state.apply_web_config_update(WebConfigUpdate::PlayPlaylist(PlaylistEntry {
            name: "Web playlist".to_string(),
            uri: "test:playlist:web".to_string(),
            art_uri: None,
        }));

        assert_eq!(state.current_time, 0.0);
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert_eq!(
            commands.recv().unwrap(),
            MopidyCommand::PlayPlaylist {
                uri: "test:playlist:web".to_string(),
                display_name: "Web playlist".to_string(),
                shuffle: true,
            }
        );
    }

    #[test]
    fn web_search_result_uses_the_non_replacing_queue_command() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());
        state.ui_mode = UiMode::Morning;

        state.apply_web_config_update(WebConfigUpdate::QueueTrack {
            uri: "jellyfin:track:chosen".to_string(),
            placement: QueuePlacement::Next,
            requested_by: Some("Alex".to_string()),
        });

        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert_eq!(
            commands.recv().unwrap(),
            MopidyCommand::QueueTrack {
                uri: "jellyfin:track:chosen".to_string(),
                placement: QueuePlacement::Next,
                requested_by: Some("Alex".to_string()),
            }
        );
    }

    #[test]
    fn web_queue_editor_actions_map_to_serialized_mopidy_commands() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());

        for (action, expected) in [
            (
                WebQueueAction::MoveUp(12),
                MopidyCommand::MoveQueueTrack {
                    tlid: 12,
                    direction: -1,
                },
            ),
            (
                WebQueueAction::MoveDown(14),
                MopidyCommand::MoveQueueTrack {
                    tlid: 14,
                    direction: 1,
                },
            ),
            (
                WebQueueAction::Remove(16),
                MopidyCommand::RemoveQueueTrack(16),
            ),
            (WebQueueAction::Play(18), MopidyCommand::PlayQueueTrack(18)),
            (WebQueueAction::Clear, MopidyCommand::ClearQueue),
        ] {
            state.apply_web_config_update(WebConfigUpdate::QueueEdit(action));
            assert_eq!(commands.recv().unwrap(), expected);
        }

        state.apply_web_config_update(WebConfigUpdate::QueueVote(22));
        assert_eq!(commands.recv().unwrap(), MopidyCommand::VoteQueueTrack(22));
    }

    #[test]
    fn playback_history_records_a_track_that_was_loaded_while_paused() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-state-history-{}-{unique}.toml",
            std::process::id()
        ));
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.history_store = Some(HistoryStore::load(path.clone()));
        let snapshot = |playback_state| MopidySnapshot {
            playback_state,
            time_position_ms: 0,
            volume: None,
            track: Some(MopidyTrackInfo {
                uri: "jellyfin:track:paused-first".to_string(),
                title: "Paused first".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                duration_ms: 90_000,
                art_uri: None,
            }),
            next_track: None,
            album_art_path: String::new(),
            queue: Vec::new(),
        };

        state.apply_mopidy_snapshot(snapshot(PlaybackState::Paused));
        assert!(state.web_history.lock().unwrap().is_empty());
        state.apply_mopidy_snapshot(snapshot(PlaybackState::Playing));
        state.apply_mopidy_snapshot(snapshot(PlaybackState::Playing));

        let history = state.web_history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Paused first");
        drop(history);
        assert_eq!(
            HistoryStore::load(path.clone())
                .entries_newest_first()
                .len(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn morning_dashboard_appears_once_per_day_inside_its_window() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.settings.morning_mode_enabled = true;
        state.settings.morning_start_hour = 7;
        state.settings.morning_end_hour = 10;

        state.update_morning_mode_at(100, 6);
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        state.update_morning_mode_at(100, 7);
        assert_eq!(state.ui_mode, UiMode::Morning);

        state.ui_mode = UiMode::NowPlaying;
        state.morning_dismissed_day = Some(100);
        state.update_morning_mode_at(100, 9);
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        state.update_morning_mode_at(101, 9);
        assert_eq!(state.ui_mode, UiMode::Morning);
        state.update_morning_mode_at(101, 10);
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
    }

    #[test]
    fn first_button_press_only_dismisses_the_morning_dashboard() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());
        state.ui_mode = UiMode::Morning;

        apply_action(ButtonAction::Button1Short, &mut state);

        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn web_status_sanitizes_non_finite_timing_values() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.current_time = f64::NAN;
        state.current_track.duration = f64::INFINITY;

        state.publish_web_playback_status();

        let web_status = state.web_playback_status.lock().unwrap();
        assert_eq!(web_status.position_seconds, 0.0);
        assert_eq!(web_status.duration_seconds, 0.0);
        assert!(serde_json::to_string(&*web_status).is_ok());
    }

    #[test]
    fn next_up_toast_only_appears_during_the_final_fifteen_valid_seconds() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.is_playing = true;
        state.current_track.duration = 120.0;
        state.next_track = Some(Track {
            title: "Next".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration: 180.0,
            album_art_path: String::new(),
        });

        state.current_time = 0.0;
        assert!(state.next_up_toast_track().is_none());

        state.current_time = 104.9;
        assert!(state.next_up_toast_track().is_none());

        state.current_time = 105.0;
        assert_eq!(state.next_up_toast_track().unwrap().title, "Next");

        state.current_time = 119.9;
        assert!(state.next_up_toast_track().is_some());

        state.current_time = 120.0;
        assert!(state.next_up_toast_track().is_none());

        state.current_time = 110.0;
        state.current_track.duration = 0.0;
        assert!(state.next_up_toast_track().is_none());

        state.current_track.duration = 120.0;
        state.is_playing = false;
        assert!(state.next_up_toast_track().is_none());
    }

    #[test]
    fn button_actions_drive_playlist_picker_transitions() {
        let config = Config {
            playlists: vec![
                PlaylistEntry {
                    name: "First".to_string(),
                    uri: "test:playlist:first".to_string(),
                    art_uri: None,
                },
                PlaylistEntry {
                    name: "Second".to_string(),
                    uri: "test:playlist:second".to_string(),
                    art_uri: None,
                },
            ],
        };
        let (mut state, commands, _updates) = AppState::new_for_test(config);

        apply_action(ButtonAction::BothLong, &mut state);
        assert_eq!(state.ui_mode, UiMode::MainMenu);

        apply_action(ButtonAction::Button2Short, &mut state);
        assert_eq!(state.ui_mode, UiMode::PlaylistPicker);

        apply_action(ButtonAction::Button1Short, &mut state);
        assert_eq!(state.picker_index, 1);

        apply_action(ButtonAction::Button2Short, &mut state);
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert_eq!(
            commands.recv().unwrap(),
            MopidyCommand::PlayPlaylist {
                uri: "test:playlist:second".to_string(),
                display_name: "Second".to_string(),
                shuffle: true,
            }
        );
    }

    #[test]
    fn volume_editor_changes_volume_in_both_directions() {
        let (mut state, commands, _updates) = AppState::new_for_test(Config::default());
        state.volume = Some(50);
        state.enter_menu();
        state.menu_index = 1;
        state.menu_select();
        assert_eq!(state.ui_mode, UiMode::SettingEditor(SettingKind::Volume));

        apply_action(ButtonAction::Button1Short, &mut state);
        assert_eq!(state.volume, Some(55));
        assert_eq!(commands.recv().unwrap(), MopidyCommand::SetVolume(55));

        apply_action(ButtonAction::Button1Long, &mut state);
        assert_eq!(state.volume, Some(50));
        assert_eq!(commands.recv().unwrap(), MopidyCommand::SetVolume(50));

        apply_action(ButtonAction::Button2Short, &mut state);
        assert_eq!(state.ui_mode, UiMode::MainMenu);
    }

    #[test]
    fn menu_wraps_and_persistent_settings_can_be_adjusted() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.enter_menu();

        state.menu_previous();
        assert_eq!(state.current_menu_item(), MenuItem::Exit);
        state.menu_next();
        assert_eq!(state.current_menu_item(), MenuItem::Playlists);

        state.menu_index = 2;
        state.menu_select();
        state.adjust_setting(-1);
        assert_eq!(state.settings.display_brightness, 0.9);
        assert_eq!(state.brightness, 0.9);
        state.leave_submenu();

        state.menu_index = 4;
        state.menu_select();
        assert_eq!(
            state.idle_display_mode(),
            IdleDisplayMode::DimmedScreensaver
        );

        state.menu_index = 5;
        state.menu_select();
        assert_eq!(state.settings.screensaver_style, ScreensaverStyle::Clock);

        state.menu_index = 6;
        state.menu_select();
        assert!(!state.settings.shuffle_screensaver_art);

        state.menu_index = 7;
        state.menu_select();
        assert_eq!(
            state.ui_mode,
            UiMode::SettingEditor(SettingKind::CarouselSpeed)
        );
        state.adjust_setting(1);
        assert_eq!(state.settings.carousel_speed, 33.0);
        state.adjust_setting(-1);
        assert_eq!(state.settings.carousel_speed, 28.0);
        state.leave_submenu();

        state.menu_index = 8;
        state.menu_select();
        assert!(!state.settings.shuffle_playlists);

        state.menu_index = 9;
        state.menu_select();
        assert_eq!(
            state.settings.now_playing_style,
            NowPlayingStyle::Visualizer
        );
        state.menu_select();
        assert_eq!(state.settings.now_playing_style, NowPlayingStyle::TrackInfo);
        state.menu_select();
        assert_eq!(state.settings.now_playing_style, NowPlayingStyle::Artwork);

        state.menu_index = 10;
        state.menu_select();
        assert_eq!(
            state.ui_mode,
            UiMode::SettingEditor(SettingKind::VisualizerDelay)
        );
        state.adjust_setting(1);
        assert_eq!(state.settings.visualizer_delay_ms, 50);
        state.adjust_setting(-1);
        assert_eq!(state.settings.visualizer_delay_ms, 0);

        state.leave_submenu();
        state.menu_index = 11;
        state.menu_select();
        assert_eq!(
            state.ui_mode,
            UiMode::SettingEditor(SettingKind::SpotifyVisualizerDelay)
        );
        state.adjust_setting(1);
        assert_eq!(state.settings.spotify_visualizer_extra_delay_ms, 50);
    }

    #[test]
    fn guest_qr_is_opened_from_the_menu_and_any_button_returns() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.enter_menu();
        state.menu_index = MAIN_MENU_ITEMS
            .iter()
            .position(|item| *item == MenuItem::GuestQr)
            .unwrap();

        state.menu_select();
        assert_eq!(state.ui_mode, UiMode::GuestQr);

        apply_action(ButtonAction::Button1Short, &mut state);
        assert_eq!(state.ui_mode, UiMode::MainMenu);
    }

    #[test]
    fn web_screensaver_preview_can_run_during_playback_and_toggle_back() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.is_playing = true;
        state.settings.screensaver_style = ScreensaverStyle::Clock;

        state.apply_web_config_update(WebConfigUpdate::Playback(
            WebPlaybackAction::ToggleScreensaver,
        ));
        assert_eq!(state.ui_mode, UiMode::Screensaver);
        assert!(state.screensaver_preview);
        state.update_brightness();
        assert_eq!(state.ui_mode, UiMode::Screensaver);
        assert!(state.web_playback_status.lock().unwrap().screensaver_active);

        state.apply_web_config_update(WebConfigUpdate::Playback(
            WebPlaybackAction::ToggleScreensaver,
        ));
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert!(!state.screensaver_preview);
        assert!(!state.web_playback_status.lock().unwrap().screensaver_active);
    }

    #[test]
    fn carousel_speed_controls_motion_and_zero_pauses_it() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.settings.screensaver_style = ScreensaverStyle::Clock;
        state.settings.carousel_speed = 45.0;
        state.toggle_screensaver_preview();

        state.advance_time(0.5);
        assert_eq!(state.screensaver_offset, 22.5);

        state.settings.carousel_speed = 0.0;
        state.advance_time(1.0);
        assert_eq!(state.screensaver_offset, 22.5);
    }

    #[test]
    fn visualizer_delay_holds_future_frames_and_coalesces_ready_history() {
        let now = Instant::now();
        let mut pending = VecDeque::from([
            (
                now - std::time::Duration::from_millis(700),
                [0.1; BAR_COUNT],
            ),
            (
                now - std::time::Duration::from_millis(550),
                [0.5; BAR_COUNT],
            ),
            (
                now - std::time::Duration::from_millis(300),
                [0.9; BAR_COUNT],
            ),
        ]);

        assert_eq!(
            take_ready_visualizer_frame(&mut pending, now, std::time::Duration::from_millis(600),),
            Some([0.1; BAR_COUNT])
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(
            take_ready_visualizer_frame(&mut pending, now, std::time::Duration::from_millis(500),),
            Some([0.5; BAR_COUNT])
        );
        assert_eq!(
            take_ready_visualizer_frame(&mut pending, now, std::time::Duration::from_millis(200),),
            Some([0.9; BAR_COUNT])
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn spotify_visualizer_offset_does_not_change_mopidy_timing() {
        let settings = Settings {
            visualizer_delay_ms: 1_250,
            spotify_visualizer_extra_delay_ms: 250,
            ..Settings::default()
        };

        assert_eq!(
            visualizer_delay_for_source(&settings, PlaybackSource::Mopidy),
            Duration::from_millis(1_250)
        );
        assert_eq!(
            visualizer_delay_for_source(&settings, PlaybackSource::Spotifyd),
            Duration::from_millis(1_500)
        );
    }

    #[test]
    fn idle_display_menu_cycles_dim_saver_and_dimmed_saver() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.enter_menu();
        state.menu_index = 4;

        assert_eq!(state.idle_display_mode(), IdleDisplayMode::Screensaver);
        state.menu_select();
        assert_eq!(
            state.idle_display_mode(),
            IdleDisplayMode::DimmedScreensaver
        );
        state.menu_select();
        assert_eq!(state.idle_display_mode(), IdleDisplayMode::Dim);
        state.menu_select();
        assert_eq!(state.idle_display_mode(), IdleDisplayMode::Screensaver);
    }

    #[test]
    fn screensaver_shuffle_is_a_permutation_and_can_be_disabled() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.screensaver_art_paths = (0..8).map(|index| format!("/tmp/{index}.jpg")).collect();
        state.reset_screensaver_art_order();

        let sequential = state.screensaver_art_order.clone();
        shuffle_indices(&mut state.screensaver_art_order, 42);
        let mut sorted = state.screensaver_art_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, sequential);
        assert_ne!(state.screensaver_art_order, sequential);

        state.settings.shuffle_screensaver_art = false;
        state.prepare_screensaver_art_order();
        assert_eq!(state.screensaver_art_order, sequential);
    }

    #[test]
    fn carousel_deduplicates_album_identity_and_file_contents() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("carousel-first-{unique}.jpg"));
        let same_bytes = std::env::temp_dir().join(format!("carousel-copy-{unique}.jpg"));
        let different = std::env::temp_dir().join(format!("carousel-other-{unique}.jpg"));
        std::fs::write(&first, b"same artwork bytes").unwrap();
        std::fs::write(&same_bytes, b"same artwork bytes").unwrap();
        std::fs::write(&different, b"different artwork bytes").unwrap();

        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.add_screensaver_art(
            first.to_string_lossy().into_owned(),
            Some(normalized_album_key("Buckethead", "Pike 65")),
        );
        state.add_screensaver_art(
            same_bytes.to_string_lossy().into_owned(),
            Some(normalized_album_key("Someone else", "Different album")),
        );
        state.add_screensaver_art(
            different.to_string_lossy().into_owned(),
            Some(normalized_album_key("  BUCKETHEAD ", "Pike   65")),
        );
        assert_eq!(state.screensaver_art_paths.len(), 1);

        state.add_screensaver_art(
            different.to_string_lossy().into_owned(),
            Some(normalized_album_key("Other", "Other album")),
        );
        assert_eq!(state.screensaver_art_paths.len(), 2);

        std::fs::remove_file(first).ok();
        std::fs::remove_file(same_bytes).ok();
        std::fs::remove_file(different).ok();
    }

    #[test]
    fn cached_art_scan_collapses_identical_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("carousel-scan-{unique}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("one.jpg"), b"duplicate").unwrap();
        std::fs::write(directory.join("two.jpg"), b"duplicate").unwrap();
        std::fs::write(directory.join("three.jpg"), b"unique").unwrap();

        let scanned = scan_cached_art(&directory);
        assert_eq!(scanned.len(), 2);

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn carousel_collapses_the_same_cover_across_png_and_jpeg_encodings() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let png_path = std::env::temp_dir().join(format!("carousel-source-{unique}.png"));
        let jpeg_path = std::env::temp_dir().join(format!("carousel-transcode-{unique}.jpg"));
        let source = image::RgbImage::from_fn(96, 96, |x, y| {
            image::Rgb([
                ((x * 2 + y) % 256) as u8,
                ((x + y * 3) % 256) as u8,
                ((x * 5 + y * 7) % 256) as u8,
            ])
        });
        source.save(&png_path).unwrap();
        image::DynamicImage::ImageRgb8(source)
            .save(&jpeg_path)
            .unwrap();

        let png = artwork_file_fingerprint(&png_path).unwrap();
        let jpeg = artwork_file_fingerprint(&jpeg_path).unwrap();
        assert_ne!(png.raw_hash, jpeg.raw_hash);
        assert!(artwork_fingerprints_match(png, jpeg));

        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        assert!(state.add_screensaver_art(
            png_path.to_string_lossy().into_owned(),
            Some(normalized_album_key("Artist", "Album")),
        ));
        assert!(!state.add_screensaver_art(
            jpeg_path.to_string_lossy().into_owned(),
            Some(normalized_album_key(
                "Different source",
                "Different metadata"
            )),
        ));
        assert_eq!(state.screensaver_art_paths.len(), 1);

        std::fs::remove_file(png_path).ok();
        std::fs::remove_file(jpeg_path).ok();
    }

    #[test]
    fn playlist_thumbnail_updates_remain_picker_only() {
        let config = Config {
            playlists: vec![PlaylistEntry {
                name: "Playlist".to_string(),
                uri: "test:playlist:one".to_string(),
                art_uri: None,
            }],
        };
        let (mut state, _commands, _updates) = AppState::new_for_test(config);
        let (preload_tx, preload_rx) = mpsc::channel();
        state.picker_preload_rx = preload_rx;
        preload_tx
            .send((0, "/tmp/playlist-thumbnail.jpg".to_string()))
            .unwrap();

        state.advance_time(0.0);

        assert_eq!(
            state.picker_art_cache[0].as_deref(),
            Some("/tmp/playlist-thumbnail.jpg")
        );
        assert!(state.screensaver_art_paths.is_empty());
    }

    #[test]
    fn album_identity_prefers_album_artist_and_normalizes_spacing() {
        let track = json!({
            "uri": "test:track:one",
            "artists": [{ "name": "Guest performer" }],
            "album": {
                "name": "  Pike   65 ",
                "artists": [{ "name": "Buckethead" }]
            }
        });

        let (uri, identity) = track_album_identity(&track).unwrap();
        assert_eq!(uri, "test:track:one");
        assert_eq!(identity, normalized_album_key("buckethead", "pike 65"));
    }

    #[test]
    fn mopidy_playlist_refs_are_cleaned_and_non_playlists_are_ignored() {
        let value = json!([
            { "type": "playlist", "name": "  Jazz  ", "uri": "spotify:playlist:jazz" },
            { "type": "album", "name": "Album", "uri": "spotify:album:nope" },
            { "type": "playlist", "name": "", "uri": "spotify:playlist:empty" },
            { "name": "Jellyfin mix", "uri": "jellyfin:playlist:mix" }
        ]);

        let playlists = parse_web_playlist_refs(&value);

        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0].name, "Jazz");
        assert_eq!(playlists[1].name, "Jellyfin mix");
        assert!(playlists.iter().all(|playlist| !playlist.favorite));
    }

    #[test]
    fn configured_playlists_are_pinned_before_sorted_discovered_entries() {
        let configured = vec![PlaylistEntry {
            name: "My favourite".to_string(),
            uri: "spotify:playlist:same".to_string(),
            art_uri: None,
        }];
        let discovered = vec![
            WebPlaylist {
                name: "Zulu".to_string(),
                uri: "spotify:playlist:zulu".to_string(),
                favorite: false,
            },
            WebPlaylist {
                name: "Duplicate remote name".to_string(),
                uri: "spotify:playlist:same".to_string(),
                favorite: false,
            },
            WebPlaylist {
                name: "Alpha".to_string(),
                uri: "spotify:playlist:alpha".to_string(),
                favorite: false,
            },
        ];

        let playlists = merge_web_playlist_catalog(&configured, discovered);

        assert_eq!(playlists.len(), 3);
        assert_eq!(playlists[0].name, "My favourite");
        assert!(playlists[0].favorite);
        assert_eq!(playlists[1].name, "Alpha");
        assert_eq!(playlists[2].name, "Zulu");
    }

    #[test]
    fn album_screensaver_activates_scrolls_and_wakes_without_an_action() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state
            .screensaver_art_paths
            .push("/tmp/example.jpg".to_string());
        state.settings.dim_timeout_seconds = 5;
        state.settings.screensaver_enabled = true;
        state.settings.screensaver_heavy_dim = false;
        state.settings.screensaver_brightness = 0.2;
        state.idle_since = Some(Instant::now() - std::time::Duration::from_secs(6));

        state.update_brightness();
        assert_eq!(state.ui_mode, UiMode::Screensaver);
        assert_eq!(state.brightness, 0.2);

        state.advance_time(1.0);
        assert_eq!(state.screensaver_offset, 28.0);

        state.wake_screensaver();
        assert_eq!(state.ui_mode, UiMode::NowPlaying);
        assert_eq!(state.screensaver_offset, 0.0);
        assert_eq!(state.brightness, state.settings.display_brightness);
    }

    #[test]
    fn clock_screensaver_activates_without_cached_art() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.settings.screensaver_style = ScreensaverStyle::Clock;
        state.settings.dim_timeout_seconds = 5;
        state.settings.screensaver_enabled = true;
        state.idle_since = Some(Instant::now() - std::time::Duration::from_secs(6));

        state.update_brightness();

        assert!(state.screensaver_art_paths.is_empty());
        assert_eq!(state.ui_mode, UiMode::Screensaver);
        assert_eq!(state.brightness, state.settings.screensaver_brightness);
    }

    #[test]
    fn dimmed_screensaver_uses_the_heavy_dim_brightness() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state
            .screensaver_art_paths
            .push("/tmp/example.jpg".to_string());
        state.settings.dim_timeout_seconds = 5;
        state.settings.screensaver_enabled = true;
        state.settings.screensaver_heavy_dim = true;
        state.settings.dim_brightness = 0.01;
        state.idle_since = Some(Instant::now() - std::time::Duration::from_secs(6));

        state.update_brightness();

        assert_eq!(state.ui_mode, UiMode::Screensaver);
        assert_eq!(state.brightness, 0.01);
    }

    #[test]
    fn dimming_uses_idle_time_but_picker_stays_bright() {
        let (mut state, _commands, _updates) = AppState::new_for_test(Config::default());
        state.idle_since = Some(Instant::now() - std::time::Duration::from_secs(11));

        state.update_brightness();
        assert_eq!(state.brightness, state.settings.dim_brightness);

        state.ui_mode = UiMode::MainMenu;
        state.update_brightness();
        assert_eq!(state.brightness, state.settings.display_brightness);
    }
}
