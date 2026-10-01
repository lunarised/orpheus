use dbus::arg::{PropMap, RefArg};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

const DBUS_DESTINATION: &str = "org.freedesktop.DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const MPRIS_NAME_PREFIX: &str = "org.mpris.MediaPlayer2.spotifyd.instance";
const SPOTIFYD_NAME_PREFIX: &str = "rs.spotifyd.instance";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DBUS_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_ARTWORK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CACHE_FILES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotifydPlaybackState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpotifydTrackInfo {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpotifydSnapshot {
    pub playback_state: SpotifydPlaybackState,
    pub time_position_ms: u64,
    pub volume: Option<u8>,
    pub track: Option<SpotifydTrackInfo>,
    pub album_art_path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpotifydUpdate {
    /// The daemon is running and has registered its private D-Bus name.
    pub available: bool,
    /// Present while a Spotify account has this Connect device selected.
    pub snapshot: Option<SpotifydSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpotifydCommand {
    TogglePlayPause,
    Pause,
    NextTrack,
    PreviousTrack,
    Seek(u64),
    SetVolume(u8),
}

pub struct SpotifydWorker {
    pub commands: Sender<SpotifydCommand>,
    pub updates: Receiver<SpotifydUpdate>,
}

pub fn spawn_worker(art_cache_dir: PathBuf) -> SpotifydWorker {
    let (command_tx, command_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    thread::spawn(move || run_worker(command_rx, update_tx, art_cache_dir));
    SpotifydWorker {
        commands: command_tx,
        updates: update_rx,
    }
}

fn run_worker(
    commands: Receiver<SpotifydCommand>,
    updates: Sender<SpotifydUpdate>,
    art_cache_dir: PathBuf,
) {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(4))
        .build();
    let mut artwork_cache = HashMap::<String, String>::new();
    let mut pending_volume: Option<u8> = None;

    loop {
        let connection = match Connection::new_system() {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("[SPOTIFYD] Cannot connect to the system D-Bus: {error}");
                if updates
                    .send(SpotifydUpdate {
                        available: false,
                        snapshot: None,
                    })
                    .is_err()
                {
                    return;
                }
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        loop {
            let names = match list_bus_names(&connection) {
                Ok(names) => names,
                Err(error) => {
                    eprintln!("[SPOTIFYD] D-Bus discovery failed: {error}");
                    break;
                }
            };
            let active_names: Vec<&str> = names
                .iter()
                .filter(|name| name.starts_with(MPRIS_NAME_PREFIX))
                .map(String::as_str)
                .collect();
            let available = !active_names.is_empty()
                || names
                    .iter()
                    .any(|name| name.starts_with(SPOTIFYD_NAME_PREFIX));

            while let Ok(command) = commands.try_recv() {
                if let SpotifydCommand::SetVolume(volume) = &command {
                    pending_volume = Some(*volume);
                }
                if let Some(name) = preferred_player(&connection, &active_names)
                    && let Err(error) = send_command(&connection, name, command)
                {
                    eprintln!("[SPOTIFYD] Control command failed: {error}");
                }
            }

            let mut snapshot = preferred_player(&connection, &active_names)
                .and_then(|name| read_snapshot(&connection, name, &artwork_cache).ok());
            if let (Some(name), Some(target), Some(current)) = (
                preferred_player(&connection, &active_names),
                pending_volume,
                snapshot.as_ref().and_then(|snapshot| snapshot.volume),
            ) {
                if current == target {
                    pending_volume = None;
                } else if let Err(error) =
                    send_command(&connection, name, SpotifydCommand::SetVolume(target))
                {
                    eprintln!("[SPOTIFYD] Deferred volume synchronization failed: {error}");
                }
            }
            let pending_art = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.track.as_ref())
                .and_then(|track| track.art_url.clone())
                .filter(|url| !artwork_cache.contains_key(url));

            if updates
                .send(SpotifydUpdate {
                    available,
                    snapshot: snapshot.clone(),
                })
                .is_err()
            {
                return;
            }

            // Metadata reaches the UI first. A new cover is downloaded only
            // afterward on this worker and appears in the following update.
            if let Some(art_url) = pending_art
                && let Some(path) = fetch_artwork(&agent, &art_cache_dir, &art_url)
            {
                artwork_cache.insert(art_url, path.clone());
                if let Some(current) = snapshot.as_mut() {
                    current.album_art_path = path;
                    if updates
                        .send(SpotifydUpdate {
                            available,
                            snapshot,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn list_bus_names(connection: &Connection) -> Result<Vec<String>, dbus::Error> {
    let proxy = connection.with_proxy(DBUS_DESTINATION, DBUS_PATH, DBUS_TIMEOUT);
    let (names,): (Vec<String>,) = proxy.method_call(DBUS_INTERFACE, "ListNames", ())?;
    Ok(names)
}

fn preferred_player<'a>(connection: &Connection, names: &'a [&str]) -> Option<&'a str> {
    names
        .iter()
        .copied()
        .find(|name| playback_status(connection, name).ok().as_deref() == Some("Playing"))
        .or_else(|| names.first().copied())
}

fn playback_status(connection: &Connection, name: &str) -> Result<String, dbus::Error> {
    connection
        .with_proxy(name, MPRIS_PATH, DBUS_TIMEOUT)
        .get(MPRIS_PLAYER, "PlaybackStatus")
}

fn read_snapshot(
    connection: &Connection,
    name: &str,
    artwork_cache: &HashMap<String, String>,
) -> Result<SpotifydSnapshot, dbus::Error> {
    let proxy = connection.with_proxy(name, MPRIS_PATH, DBUS_TIMEOUT);
    let status: String = proxy.get(MPRIS_PLAYER, "PlaybackStatus")?;
    let metadata: PropMap = proxy.get(MPRIS_PLAYER, "Metadata")?;
    let time_position_us: i64 = proxy.get(MPRIS_PLAYER, "Position").unwrap_or(0);
    let raw_volume: Option<f64> = proxy.get(MPRIS_PLAYER, "Volume").ok();
    let art_url = metadata_string(&metadata, "mpris:artUrl");
    let album_art_path = art_url
        .as_ref()
        .and_then(|url| artwork_cache.get(url))
        .filter(|path| Path::new(path).exists())
        .cloned()
        .unwrap_or_default();
    let title = metadata_string(&metadata, "xesam:title");
    let track = title.map(|title| SpotifydTrackInfo {
        id: metadata_string(&metadata, "mpris:trackid")
            .unwrap_or_else(|| format!("spotifyd:{title}")),
        title,
        artist: metadata_string_list(&metadata, "xesam:artist")
            .unwrap_or_else(|| "Unknown Artist".to_string()),
        album: metadata_string(&metadata, "xesam:album")
            .unwrap_or_else(|| "Unknown Album".to_string()),
        duration_ms: metadata_i64(&metadata, "mpris:length").unwrap_or(0).max(0) as u64 / 1000,
        art_url,
    });

    Ok(SpotifydSnapshot {
        playback_state: match status.as_str() {
            "Playing" => SpotifydPlaybackState::Playing,
            "Paused" => SpotifydPlaybackState::Paused,
            _ => SpotifydPlaybackState::Stopped,
        },
        time_position_ms: time_position_us.max(0) as u64 / 1000,
        volume: raw_volume.map(|volume| (volume * 100.0).round().clamp(0.0, 100.0) as u8),
        track,
        album_art_path,
    })
}

fn metadata_string(metadata: &PropMap, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(|value| value.0.as_str())
        .map(str::to_string)
}

fn metadata_string_list(metadata: &PropMap, key: &str) -> Option<String> {
    let artists = metadata
        .get(key)?
        .0
        .as_iter()?
        .filter_map(RefArg::as_str)
        .filter(|artist| !artist.is_empty())
        .collect::<Vec<_>>();
    (!artists.is_empty()).then(|| artists.join(", "))
}

fn metadata_i64(metadata: &PropMap, key: &str) -> Option<i64> {
    metadata.get(key)?.0.as_i64()
}

fn send_command(
    connection: &Connection,
    name: &str,
    command: SpotifydCommand,
) -> Result<(), dbus::Error> {
    let proxy = connection.with_proxy(name, MPRIS_PATH, DBUS_TIMEOUT);
    match command {
        SpotifydCommand::TogglePlayPause => proxy.method_call(MPRIS_PLAYER, "PlayPause", ()),
        SpotifydCommand::Pause => proxy.method_call(MPRIS_PLAYER, "Pause", ()),
        SpotifydCommand::NextTrack => proxy.method_call(MPRIS_PLAYER, "Next", ()),
        SpotifydCommand::PreviousTrack => proxy.method_call(MPRIS_PLAYER, "Previous", ()),
        SpotifydCommand::Seek(position_ms) => {
            let current_us: i64 = proxy.get(MPRIS_PLAYER, "Position").unwrap_or(0);
            let target_us = i64::try_from(position_ms)
                .unwrap_or(i64::MAX / 1000)
                .saturating_mul(1000);
            proxy.method_call(
                MPRIS_PLAYER,
                "Seek",
                (target_us.saturating_sub(current_us),),
            )
        }
        SpotifydCommand::SetVolume(volume) => {
            proxy.set(MPRIS_PLAYER, "Volume", f64::from(volume.min(100)) / 100.0)
        }
    }
}

fn fetch_artwork(agent: &ureq::Agent, cache_dir: &Path, url: &str) -> Option<String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    std::fs::create_dir_all(cache_dir).ok()?;
    let cache_file = cache_dir.join(format!("spotify-{:016x}.jpg", stable_hash(url)));
    if cache_file.exists() {
        return Some(cache_file.to_string_lossy().to_string());
    }

    let response = agent.get(url).call().ok()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_ARTWORK_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ARTWORK_BYTES {
        return None;
    }
    evict_artwork_cache(cache_dir);
    std::fs::write(&cache_file, bytes).ok()?;
    Some(cache_file.to_string_lossy().to_string())
}

fn evict_artwork_cache(cache_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            Some((entry.path(), metadata.modified().ok()?))
        })
        .collect();
    if files.len() < MAX_CACHE_FILES {
        return;
    }
    files.sort_by_key(|(_, modified)| *modified);
    for (path, _) in files.iter().take(files.len() - MAX_CACHE_FILES / 2) {
        std::fs::remove_file(path).ok();
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::arg::Variant;

    #[test]
    fn metadata_helpers_read_scalar_and_artist_values() {
        let mut metadata = PropMap::new();
        metadata.insert(
            "xesam:title".to_string(),
            Variant(Box::new("Test title".to_string())),
        );
        metadata.insert(
            "xesam:artist".to_string(),
            Variant(Box::new(vec!["One".to_string(), "Two".to_string()])),
        );
        metadata.insert(
            "mpris:length".to_string(),
            Variant(Box::new(12_345_000_i64)),
        );

        assert_eq!(
            metadata_string(&metadata, "xesam:title").as_deref(),
            Some("Test title")
        );
        assert_eq!(
            metadata_string_list(&metadata, "xesam:artist").as_deref(),
            Some("One, Two")
        );
        assert_eq!(metadata_i64(&metadata, "mpris:length"), Some(12_345_000));
    }

    #[test]
    fn stable_hash_is_repeatable_and_input_sensitive() {
        assert_eq!(stable_hash("cover-a"), stable_hash("cover-a"));
        assert_ne!(stable_hash("cover-a"), stable_hash("cover-b"));
    }
}
