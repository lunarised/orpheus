use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum number of cached art files to keep in /tmp.
const MAX_CACHE_FILES: usize = 500;
const QUEUE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Information about the currently playing track, fetched from Mopidy.
#[derive(Debug, Clone)]
pub struct MopidyTrackInfo {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub art_uri: Option<String>,
}

/// Playback state reported by Mopidy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// A consistent view of Mopidy state produced by the background worker.
#[derive(Debug, Clone)]
pub struct MopidySnapshot {
    pub playback_state: PlaybackState,
    pub time_position_ms: u64,
    pub volume: Option<u8>,
    pub track: Option<MopidyTrackInfo>,
    pub next_track: Option<MopidyTrackInfo>,
    pub album_art_path: String,
    pub queue: Vec<MopidyQueueTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MopidyQueueTrack {
    pub tlid: u64,
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub current: bool,
    pub requested_by: Option<String>,
    pub votes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct PersistentQueue {
    uris: Vec<String>,
    #[serde(default)]
    current_index: Option<usize>,
    /// Readable fallback for queue files written before current_index existed.
    #[serde(default)]
    current_uri: Option<String>,
    #[serde(default)]
    requesters: HashMap<String, String>,
    #[serde(default)]
    votes: HashMap<String, u32>,
}

/// A Jellyfin track returned by Mopidy's library search API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MopidySearchTrack {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MopidyBrowseItem {
    pub uri: String,
    pub name: String,
    pub item_type: String,
}

/// Where a track selected in the web remote should be inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePlacement {
    End,
    Next,
    Now,
}

/// Work accepted by the Mopidy background thread.
#[derive(Debug, Clone, PartialEq)]
pub enum MopidyCommand {
    Poll,
    TogglePlayPause,
    Pause,
    NextTrack,
    PreviousTrack,
    Seek(u64),
    SetVolume(u8),
    PlayPlaylist {
        uri: String,
        display_name: String,
        shuffle: bool,
    },
    QueueTrack {
        uri: String,
        placement: QueuePlacement,
        requested_by: Option<String>,
    },
    RemoveQueueTrack(u64),
    MoveQueueTrack {
        tlid: u64,
        direction: i8,
    },
    PlayQueueTrack(u64),
    VoteQueueTrack(u64),
    ClearQueue,
}

/// Non-blocking channels used by the UI thread to communicate with Mopidy.
pub struct MopidyWorker {
    pub commands: Sender<MopidyCommand>,
    pub updates: Receiver<Option<MopidySnapshot>>,
}

/// Client for Mopidy's JSON-RPC API.
pub struct MopidyClient {
    base_url: String,
    rpc_url: String,
    art_cache_dir: PathBuf,
    queue_state_path: PathBuf,
    agent: ureq::Agent,
}

/// A lightweight cloneable client dedicated to library searches from web threads.
/// It deliberately does not share the display's serialized playback worker.
#[derive(Clone)]
pub struct MopidyLibraryClient {
    rpc_url: String,
    agent: Arc<ureq::Agent>,
}

impl MopidyClient {
    pub fn new(host: &str, port: u16, art_cache_dir: &str, queue_state_path: PathBuf) -> Self {
        let base_url = format!("http://{}:{}", host, port);
        let rpc_url = format!("{}/mopidy/rpc", base_url);

        // Create cache directory for album art
        let cache_path = PathBuf::from(art_cache_dir);
        std::fs::create_dir_all(&cache_path).ok();

        // Requests run on the dedicated Mopidy worker, so a slightly generous
        // timeout does not block rendering or buttons. It avoids false offline
        // flashes while startup artwork lookups briefly contend with polling.
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();

        Self {
            base_url,
            rpc_url,
            art_cache_dir: cache_path,
            queue_state_path,
            agent,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    pub fn art_cache_dir(&self) -> &PathBuf {
        &self.art_cache_dir
    }

    pub fn library_client(&self) -> MopidyLibraryClient {
        MopidyLibraryClient {
            rpc_url: self.rpc_url.clone(),
            agent: Arc::new(
                ureq::AgentBuilder::new()
                    .timeout(Duration::from_secs(20))
                    .build(),
            ),
        }
    }

    /// Move this client to a background thread and return its communication channels.
    /// Sending a command never waits for network I/O; each command publishes a fresh
    /// snapshot when it has completed. A `None` snapshot means polling failed and the
    /// UI should retain its last known state.
    pub fn spawn_worker(self) -> MopidyWorker {
        let (command_tx, command_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();

        thread::spawn(move || self.run_worker(command_rx, update_tx));

        MopidyWorker {
            commands: command_tx,
            updates: update_rx,
        }
    }

    fn run_worker(
        self,
        commands: Receiver<MopidyCommand>,
        updates: Sender<Option<MopidySnapshot>>,
    ) {
        let mut cached_art: Option<(String, String, String)> = None;
        let mut cached_next_track: Option<(u64, Option<MopidyTrackInfo>)> = None;
        let mut cached_queue = Vec::new();
        let mut last_queue_refresh = Instant::now()
            .checked_sub(QUEUE_REFRESH_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut restore_checked = false;
        let mut last_persisted_queue = load_persistent_queue(&self.queue_state_path);
        let mut queue_requesters = HashMap::new();
        let mut queue_votes = HashMap::new();

        while let Ok(command) = commands.recv() {
            if !restore_checked {
                restore_checked = self
                    .restore_queue_if_empty(last_persisted_queue.as_ref())
                    .is_some();
            }
            let refresh_queue =
                !matches!(&command, MopidyCommand::Poll | MopidyCommand::SetVolume(_));
            match command {
                MopidyCommand::Poll => {}
                MopidyCommand::TogglePlayPause => self.play_pause(),
                MopidyCommand::Pause => self.pause(),
                MopidyCommand::NextTrack => self.next_track(),
                MopidyCommand::PreviousTrack => self.previous_track(),
                MopidyCommand::Seek(time_position_ms) => self.seek(time_position_ms),
                MopidyCommand::SetVolume(volume) => self.set_volume(volume),
                MopidyCommand::PlayPlaylist {
                    uri,
                    display_name,
                    shuffle,
                } => {
                    self.play_playlist_uri(&uri, &display_name, shuffle);
                }
                MopidyCommand::QueueTrack {
                    uri,
                    placement,
                    requested_by,
                } => {
                    if let Some(tlid) = self.queue_track_uri(&uri, placement)
                        && let Some(requester) = requested_by
                    {
                        queue_requesters.insert(tlid, requester);
                    }
                }
                MopidyCommand::RemoveQueueTrack(tlid) => self.remove_queue_track(tlid),
                MopidyCommand::MoveQueueTrack { tlid, direction } => {
                    self.move_queue_track(tlid, direction);
                }
                MopidyCommand::PlayQueueTrack(tlid) => {
                    self.play_tlid(tlid);
                }
                MopidyCommand::VoteQueueTrack(tlid) => {
                    vote_and_promote_queue_track(&self, tlid, &cached_queue, &mut queue_votes);
                }
                MopidyCommand::ClearQueue => {
                    if self.rpc_call("core.tracklist.clear", None).is_some() {
                        queue_requesters.clear();
                        queue_votes.clear();
                        let cleared = PersistentQueue::default();
                        if let Err(error) = save_persistent_queue(&self.queue_state_path, &cleared)
                        {
                            eprintln!("[QUEUE] Could not save cleared queue: {error}");
                        } else {
                            last_persisted_queue = Some(cleared);
                        }
                    }
                }
            }

            let mut snapshot = self.get_snapshot(
                &mut cached_art,
                &mut cached_next_track,
                &mut cached_queue,
                &mut last_queue_refresh,
                refresh_queue,
            );
            if let Some(snapshot) = snapshot.as_mut() {
                apply_queue_metadata(
                    &mut snapshot.queue,
                    last_persisted_queue.as_ref(),
                    &mut queue_requesters,
                    &mut queue_votes,
                );
                let persisted = PersistentQueue::from_tracks(&snapshot.queue);
                if last_persisted_queue.as_ref() != Some(&persisted) {
                    if let Err(error) = save_persistent_queue(&self.queue_state_path, &persisted) {
                        eprintln!("[QUEUE] Could not save persistent queue: {error}");
                    } else {
                        last_persisted_queue = Some(persisted);
                    }
                }
            } else {
                // A later successful poll may be the first response after a
                // Mopidy restart. Re-check its empty tracklist against disk so
                // persistence also covers backend restarts, not just this
                // process starting up.
                restore_checked = false;
            }
            if updates.send(snapshot).is_err() {
                break;
            }
        }
    }

    /// Send a JSON-RPC request to Mopidy.
    fn rpc_call(&self, method: &str, params: Option<Value>) -> Option<Value> {
        let id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });

        if let Some(p) = params {
            body["params"] = p;
        }

        match self
            .agent
            .post(&self.rpc_url)
            .set("Content-Type", "application/json")
            .send_json(&body)
        {
            Ok(resp) => match resp.into_json::<Value>() {
                Ok(json) => {
                    if let Some(error) = json.get("error") {
                        eprintln!("Mopidy RPC error ({method}): {error}");
                        None
                    } else {
                        json.get("result").cloned()
                    }
                }
                Err(e) => {
                    eprintln!("Mopidy RPC parse error: {}", e);
                    None
                }
            },
            Err(e) => {
                eprintln!("Mopidy RPC error ({}): {}", method, e);
                None
            }
        }
    }

    fn try_get_playback_state(&self) -> Option<PlaybackState> {
        match self.rpc_call("core.playback.get_state", None) {
            Some(Value::String(s)) => match s.as_str() {
                "playing" => Some(PlaybackState::Playing),
                "paused" => Some(PlaybackState::Paused),
                "stopped" => Some(PlaybackState::Stopped),
                _ => None,
            },
            _ => None,
        }
    }

    /// Get the current playback state.
    pub fn get_playback_state(&self) -> PlaybackState {
        self.try_get_playback_state()
            .unwrap_or(PlaybackState::Stopped)
    }

    /// Get the current time position in milliseconds.
    fn try_get_time_position(&self) -> Option<u64> {
        match self.rpc_call("core.playback.get_time_position", None) {
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        }
    }

    fn try_get_volume(&self) -> Option<u8> {
        let volume = self.rpc_call("core.mixer.get_volume", None)?.as_u64()?;
        u8::try_from(volume.min(100)).ok()
    }

    /// Get the currently playing track info.
    fn try_get_current_track(&self) -> Option<Option<(u64, MopidyTrackInfo)>> {
        let result = self.rpc_call("core.playback.get_current_tl_track", None)?;
        if result.is_null() {
            return Some(None);
        }
        let tlid = result.get("tlid")?.as_u64()?;
        let track = result.get("track")?;

        Some(Some((tlid, Self::parse_track(track))))
    }

    fn parse_track(track: &Value) -> MopidyTrackInfo {
        let title = track
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        let artist = track
            .get("artists")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Artist")
            .to_string();

        let album = track
            .get("album")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Album")
            .to_string();

        let duration_ms = track.get("length").and_then(|v| v.as_u64()).unwrap_or(0);

        let uri = track
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        MopidyTrackInfo {
            uri,
            title,
            artist,
            album,
            duration_ms,
            art_uri: None,
        }
    }

    fn try_get_next_track(&self) -> Option<Option<MopidyTrackInfo>> {
        let next_tlid = self.rpc_call("core.tracklist.get_next_tlid", None)?;
        let Some(next_tlid) = next_tlid.as_u64() else {
            return Some(None);
        };
        let params = json!({ "criteria": { "tlid": [next_tlid] } });
        let matches = self.rpc_call("core.tracklist.filter", Some(params))?;
        let Some(next_tl_track) = matches.as_array()?.first() else {
            return Some(None);
        };
        let track = next_tl_track.get("track")?;
        Some(Some(Self::parse_track(track)))
    }

    /// Poll all now-playing data on the worker thread.
    fn get_snapshot(
        &self,
        cached_art: &mut Option<(String, String, String)>,
        cached_next_track: &mut Option<(u64, Option<MopidyTrackInfo>)>,
        cached_queue: &mut Vec<MopidyQueueTrack>,
        last_queue_refresh: &mut Instant,
        force_queue_refresh: bool,
    ) -> Option<MopidySnapshot> {
        let playback_state = self.try_get_playback_state()?;
        let time_position_ms = self.try_get_time_position()?;
        let current_tl_track = self.try_get_current_track()?;
        let (current_tlid, mut track) = match current_tl_track {
            Some((tlid, track)) => (Some(tlid), Some(track)),
            None => (None, None),
        };
        let mut album_art_path = String::new();

        if let Some(track_info) = track.as_mut() {
            if let Some((cached_track_uri, cached_art_uri, cached_path)) = cached_art.as_ref()
                && cached_track_uri == &track_info.uri
            {
                track_info.art_uri = Some(cached_art_uri.clone());
                album_art_path = cached_path.clone();
            } else if let Some(art_uri) = self.get_image_uri(&track_info.uri) {
                album_art_path = self
                    .fetch_album_art(&art_uri)
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_default();
                track_info.art_uri = Some(art_uri.clone());

                if !album_art_path.is_empty() {
                    *cached_art = Some((track_info.uri.clone(), art_uri, album_art_path.clone()));
                }
            }
        } else {
            *cached_art = None;
        }

        let next_track = match current_tlid {
            Some(tlid) => {
                if let Some((cached_tlid, next_track)) = cached_next_track.as_ref()
                    && *cached_tlid == tlid
                {
                    next_track.clone()
                } else if let Some(next_track) = self.try_get_next_track() {
                    *cached_next_track = Some((tlid, next_track.clone()));
                    next_track
                } else {
                    None
                }
            }
            None => {
                *cached_next_track = None;
                None
            }
        };

        if (force_queue_refresh || last_queue_refresh.elapsed() >= QUEUE_REFRESH_INTERVAL)
            && let Some(queue) = self.try_get_queue(current_tlid)
        {
            *cached_queue = queue;
            *last_queue_refresh = Instant::now();
        } else {
            for item in cached_queue.iter_mut() {
                item.current = Some(item.tlid) == current_tlid;
            }
        }

        Some(MopidySnapshot {
            playback_state,
            time_position_ms,
            volume: self.try_get_volume(),
            track,
            next_track,
            album_art_path,
            queue: cached_queue.clone(),
        })
    }

    fn try_get_queue(&self, current_tlid: Option<u64>) -> Option<Vec<MopidyQueueTrack>> {
        let tracks = self.rpc_call("core.tracklist.get_tl_tracks", None)?;
        Some(parse_queue_tracks(&tracks, current_tlid))
    }

    fn restore_queue_if_empty(&self, saved: Option<&PersistentQueue>) -> Option<()> {
        let live = self.try_get_queue(None)?;
        if !live.is_empty() {
            return Some(());
        }
        let Some(saved) = saved.filter(|saved| !saved.uris.is_empty()) else {
            return Some(());
        };
        let restore_uris = saved.uris_to_restore();
        if self
            .rpc_call("core.tracklist.add", Some(json!({ "uris": restore_uris })))
            .is_none()
        {
            eprintln!("[QUEUE] Persistent queue restore failed");
            return None;
        }
        println!("[QUEUE] Restored {} persistent tracks", restore_uris.len());
        Some(())
    }

    /// Get image URI for any Mopidy URI (track, album, playlist, etc.).
    pub fn get_image_uri(&self, uri: &str) -> Option<String> {
        let params = json!({ "uris": [uri] });
        let result = self.rpc_call("core.library.get_images", Some(params))?;

        // Result is a map of URI -> array of images
        let images = result.get(uri)?.as_array()?;

        // Get the first (usually largest) image
        let image = images.first()?;
        let uri = image.get("uri")?.as_str()?;

        Some(uri.to_string())
    }

    /// Download album art to a local file and return the path.
    /// Returns None if the art can't be fetched.
    /// Cache lives in /tmp (tmpfs) to avoid SD card write wear.
    pub fn fetch_album_art(&self, art_uri: &str) -> Option<PathBuf> {
        // Create a filename from the URI hash
        let hash = simple_hash(art_uri);
        let cache_file = self.art_cache_dir.join(format!("{}.jpg", hash));

        // If already cached, return the path
        if cache_file.exists() {
            return Some(cache_file);
        }

        // Build the full URL (Mopidy returns relative paths for local images)
        let url = if art_uri.starts_with("http://") || art_uri.starts_with("https://") {
            art_uri.to_string()
        } else {
            format!("{}{}", self.base_url, art_uri)
        };

        // Download the image
        match self.agent.get(&url).call() {
            Ok(resp) => {
                let mut bytes = Vec::new();
                if resp.into_reader().read_to_end(&mut bytes).is_ok() {
                    // Evict oldest files if cache is too large
                    self.evict_cache();
                    if std::fs::write(&cache_file, &bytes).is_ok() {
                        return Some(cache_file);
                    }
                }
                None
            }
            Err(e) => {
                eprintln!("Failed to fetch album art from {}: {}", url, e);
                None
            }
        }
    }

    /// Evict oldest cached art files if the cache exceeds MAX_CACHE_FILES.
    fn evict_cache(&self) {
        let entries: Vec<_> = match std::fs::read_dir(&self.art_cache_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(_) => return,
        };

        if entries.len() < MAX_CACHE_FILES {
            return;
        }

        // Sort by modified time (oldest first)
        let mut files: Vec<_> = entries
            .iter()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?;
                Some((e.path(), modified))
            })
            .collect();

        files.sort_by_key(|(_, t)| *t);

        // Remove oldest files until we're under the limit
        let to_remove = files.len().saturating_sub(MAX_CACHE_FILES / 2);
        for (path, _) in files.iter().take(to_remove) {
            std::fs::remove_file(path).ok();
        }
    }

    // --- Playback control commands ---

    /// Toggle play/pause.
    pub fn play_pause(&self) {
        let state = self.get_playback_state();
        match state {
            PlaybackState::Playing => {
                self.rpc_call("core.playback.pause", None);
            }
            PlaybackState::Paused => {
                self.rpc_call("core.playback.resume", None);
            }
            PlaybackState::Stopped => {
                if let Some(tlid) = self.current_or_first_tlid() {
                    self.play_tlid(tlid);
                } else {
                    eprintln!("Cannot start playback: the Mopidy tracklist is empty");
                }
            }
        }
    }

    /// Pause without accidentally resuming an already-paused player.
    pub fn pause(&self) {
        self.rpc_call("core.playback.pause", None);
    }

    fn current_or_first_tlid(&self) -> Option<u64> {
        self.rpc_call("core.playback.get_current_tlid", None)
            .and_then(|value| value.as_u64())
            .or_else(|| self.first_tlid())
    }

    fn first_tlid(&self) -> Option<u64> {
        let tracks = self.rpc_call(
            "core.tracklist.slice",
            Some(json!({ "start": 0, "end": 1 })),
        )?;
        first_tlid_from_tracklist(&tracks)
    }

    fn play_tlid(&self, tlid: u64) -> bool {
        self.rpc_call("core.playback.play", Some(json!({ "tlid": tlid })))
            .is_some()
    }

    /// Skip to the next track.
    pub fn next_track(&self) {
        self.rpc_call("core.playback.next", None);
    }

    /// Go to the previous track.
    pub fn previous_track(&self) {
        self.rpc_call("core.playback.previous", None);
    }

    /// Seek within the current track to an absolute position in milliseconds.
    pub fn seek(&self, time_position_ms: u64) {
        let params = json!({ "time_position": time_position_ms });
        self.rpc_call("core.playback.seek", Some(params));
    }

    /// Set Mopidy's mixer volume from 0 to 100 percent.
    pub fn set_volume(&self, volume: u8) {
        let params = json!({ "volume": volume.min(100) });
        self.rpc_call("core.mixer.set_volume", Some(params));
    }

    /// Add one URI without replacing the existing tracklist.
    pub fn queue_track_uri(&self, uri: &str, placement: QueuePlacement) -> Option<u64> {
        let insert_position = match placement {
            QueuePlacement::End => None,
            QueuePlacement::Next | QueuePlacement::Now => {
                Some(self.position_after_current_track().unwrap_or(0))
            }
        };
        let params = match insert_position {
            Some(at_position) => json!({ "uris": [uri], "at_position": at_position }),
            None => json!({ "uris": [uri] }),
        };
        let Some(added) = self.rpc_call("core.tracklist.add", Some(params)) else {
            eprintln!("Failed to add track to queue: {uri}");
            return None;
        };
        let Some(tlid) = first_tlid_from_tracklist(&added) else {
            eprintln!("Mopidy returned no queued track ID for: {uri}");
            return None;
        };

        if placement == QueuePlacement::Now && !self.play_tlid(tlid) {
            eprintln!("Failed to play newly queued track ID {tlid}");
            return None;
        }

        let action = match placement {
            QueuePlacement::End => "Queued",
            QueuePlacement::Next => "Queued next",
            QueuePlacement::Now => "Playing now",
        };
        println!("{action}: {uri}");
        Some(tlid)
    }

    fn remove_queue_track(&self, tlid: u64) {
        self.rpc_call(
            "core.tracklist.remove",
            Some(json!({ "criteria": { "tlid": [tlid] } })),
        );
    }

    fn move_queue_track(&self, tlid: u64, direction: i8) {
        let Some(queue) = self.try_get_queue(None) else {
            return;
        };
        let Some(index) = queue.iter().position(|track| track.tlid == tlid) else {
            return;
        };
        let Some((start, end, to_position)) = queue_move_parameters(index, direction, queue.len())
        else {
            return;
        };
        self.rpc_call(
            "core.tracklist.move",
            Some(json!({
                "start": start,
                "end": end,
                "to_position": to_position
            })),
        );
    }

    fn position_after_current_track(&self) -> Option<u64> {
        let tlid = self
            .rpc_call("core.playback.get_current_tlid", None)?
            .as_u64()?;
        let index = self
            .rpc_call("core.tracklist.index", Some(json!({ "tlid": tlid })))?
            .as_u64()?;
        index.checked_add(1)
    }

    // --- Playlist commands ---

    /// Play a playlist by its URI directly. Clears the tracklist and loads it.
    /// Works with Spotify URIs, Jellyfin URIs, or any Mopidy-compatible URI.
    pub fn play_playlist_uri(&self, uri: &str, display_name: &str, shuffle: bool) -> bool {
        // First try: playlist lookup (works for m3u/local/jellyfin playlists)
        let params = json!({ "uri": uri });
        if let Some(playlist) = self.rpc_call("core.playlists.lookup", Some(params))
            && let Some(tracks) = playlist.get("tracks").and_then(|t| t.as_array())
        {
            let track_uris: Vec<&str> = tracks
                .iter()
                .filter_map(|t| t.get("uri").and_then(|u| u.as_str()))
                .collect();

            if !track_uris.is_empty() {
                self.rpc_call("core.tracklist.clear", None);
                let add_params = json!({ "uris": track_uris });
                self.rpc_call("core.tracklist.add", Some(add_params));
                if shuffle {
                    self.rpc_call("core.tracklist.shuffle", None);
                }
                let Some(tlid) = self.first_tlid() else {
                    eprintln!("Failed to start playlist '{display_name}': no queued track ID");
                    return false;
                };
                if !self.play_tlid(tlid) {
                    eprintln!("Failed to start playlist '{display_name}' at track ID {tlid}");
                    return false;
                }
                let shuffle_status = if shuffle { ", shuffled" } else { "" };
                println!(
                    "Playing '{}' ({} tracks{})",
                    display_name,
                    track_uris.len(),
                    shuffle_status
                );
                return true;
            }
        }

        // Fallback: try adding the URI directly to tracklist (works for Spotify playlists)
        self.rpc_call("core.tracklist.clear", None);
        let add_params = json!({ "uris": [uri] });
        if self
            .rpc_call("core.tracklist.add", Some(add_params))
            .is_some()
        {
            if shuffle {
                self.rpc_call("core.tracklist.shuffle", None);
            }
            let Some(tlid) = self.first_tlid() else {
                eprintln!("Failed to start playlist '{display_name}': no queued track ID");
                return false;
            };
            if !self.play_tlid(tlid) {
                eprintln!("Failed to start playlist '{display_name}' at track ID {tlid}");
                return false;
            }
            let shuffle_status = if shuffle { ", shuffled" } else { "" };
            println!("Playing '{}' (direct URI{})", display_name, shuffle_status);
            return true;
        }

        eprintln!("Failed to play playlist '{}' ({})", display_name, uri);
        false
    }
}

impl MopidyLibraryClient {
    /// Search only the Jellyfin backend and return a small, browser-friendly model.
    pub fn search_jellyfin_tracks(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MopidySearchTrack>, String> {
        let query = query.trim();
        if query.chars().count() < 3 {
            return Err("search text must contain at least 3 characters".to_string());
        }
        let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "core.library.search",
            "params": {
                "query": { "any": [query] },
                "uris": ["jellyfin:"],
                "exact": false
            }
        });
        let response = self
            .agent
            .post(&self.rpc_url)
            .set("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|error| format!("Mopidy search request failed: {error}"))?;
        let response = response
            .into_json::<Value>()
            .map_err(|error| format!("Mopidy search response was invalid: {error}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("Mopidy search failed: {error}"));
        }
        let result = response
            .get("result")
            .ok_or_else(|| "Mopidy search response had no result".to_string())?;
        Ok(parse_search_tracks(result, limit.clamp(1, 50)))
    }

    pub fn browse_jellyfin(
        &self,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<MopidyBrowseItem>, String> {
        let uri = match kind {
            "artists" => "jellyfin:artists",
            "albums" => "jellyfin:albums",
            _ => return Err("browse kind must be 'artists' or 'albums'".to_string()),
        };
        let result = self.rpc_request("core.library.browse", Some(json!({ "uri": uri })))?;
        Ok(parse_browse_items(&result, limit.clamp(1, 2_000)))
    }

    pub fn lookup_jellyfin_tracks(
        &self,
        uri: &str,
        limit: usize,
    ) -> Result<Vec<MopidySearchTrack>, String> {
        if (!(uri.starts_with("jellyfin:artist:") || uri.starts_with("jellyfin:album:")))
            || uri.len() > 512
        {
            return Err("invalid Jellyfin artist or album URI".to_string());
        }
        let result = self.rpc_request("core.library.lookup", Some(json!({ "uris": [uri] })))?;
        let tracks = result
            .get(uri)
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        Ok(parse_track_values(&tracks, limit.clamp(1, 1_000)))
    }

    fn rpc_request(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
        });
        if let Some(params) = params {
            body["params"] = params;
        }
        let response = self
            .agent
            .post(&self.rpc_url)
            .set("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|error| format!("Mopidy library request failed: {error}"))?;
        let response = response
            .into_json::<Value>()
            .map_err(|error| format!("Mopidy library response was invalid: {error}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("Mopidy library request failed: {error}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "Mopidy library response had no result".to_string())
    }
}

impl PersistentQueue {
    fn from_tracks(tracks: &[MopidyQueueTrack]) -> Self {
        Self {
            uris: tracks
                .iter()
                .filter(|track| !track.uri.is_empty())
                .map(|track| track.uri.clone())
                .collect(),
            current_index: tracks
                .iter()
                .filter(|track| !track.uri.is_empty())
                .position(|track| track.current),
            current_uri: tracks
                .iter()
                .find(|track| track.current)
                .map(|track| track.uri.clone()),
            requesters: tracks
                .iter()
                .filter_map(|track| Some((track.uri.clone(), track.requested_by.as_ref()?.clone())))
                .collect(),
            votes: tracks
                .iter()
                .filter(|track| track.votes > 0)
                .map(|track| (track.uri.clone(), track.votes))
                .collect(),
        }
    }

    /// A reboot has no safe way to select Mopidy's current TLID without
    /// starting playback. Restore the current and upcoming tail instead, so a
    /// later Play resumes at the saved point rather than replaying old items.
    fn uris_to_restore(&self) -> &[String] {
        let start = self
            .current_index
            .filter(|index| *index < self.uris.len())
            .or_else(|| {
                self.current_uri
                    .as_ref()
                    .and_then(|current| self.uris.iter().position(|uri| uri == current))
            })
            .unwrap_or(0);
        &self.uris[start..]
    }
}

fn load_persistent_queue(path: &PathBuf) -> Option<PersistentQueue> {
    let contents = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&contents) {
        Ok(queue) => Some(queue),
        Err(error) => {
            eprintln!(
                "[QUEUE] Could not parse persistent queue '{}': {error}",
                path.display()
            );
            None
        }
    }
}

fn save_persistent_queue(path: &PathBuf, queue: &PersistentQueue) -> Result<(), String> {
    let serialized = toml::to_string_pretty(queue)
        .map_err(|error| format!("could not serialize queue: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, serialized)
        .map_err(|error| format!("could not write '{}': {error}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("could not replace '{}': {error}", path.display()))
}

fn parse_queue_tracks(value: &Value, current_tlid: Option<u64>) -> Vec<MopidyQueueTrack> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let tlid = entry.get("tlid")?.as_u64()?;
            let track = entry.get("track")?;
            let parsed = MopidyClient::parse_track(track);
            Some(MopidyQueueTrack {
                tlid,
                uri: parsed.uri,
                title: parsed.title,
                artist: parsed.artist,
                album: parsed.album,
                duration_ms: parsed.duration_ms,
                current: current_tlid == Some(tlid),
                requested_by: None,
                votes: 0,
            })
        })
        .collect()
}

fn apply_queue_metadata(
    queue: &mut [MopidyQueueTrack],
    persisted: Option<&PersistentQueue>,
    requesters: &mut HashMap<u64, String>,
    votes: &mut HashMap<u64, u32>,
) {
    let active_tlids: HashSet<u64> = queue.iter().map(|track| track.tlid).collect();
    requesters.retain(|tlid, _| active_tlids.contains(tlid));
    votes.retain(|tlid, _| active_tlids.contains(tlid));

    for track in queue {
        if !requesters.contains_key(&track.tlid)
            && let Some(requester) = persisted
                .and_then(|saved| saved.requesters.get(&track.uri))
                .filter(|requester| !requester.is_empty())
        {
            requesters.insert(track.tlid, requester.clone());
        }
        if !votes.contains_key(&track.tlid)
            && let Some(saved_votes) = persisted
                .and_then(|saved| saved.votes.get(&track.uri))
                .copied()
                .filter(|votes| *votes > 0)
        {
            votes.insert(track.tlid, saved_votes);
        }
        track.requested_by = requesters.get(&track.tlid).cloned();
        track.votes = votes.get(&track.tlid).copied().unwrap_or(0);
    }
}

fn vote_and_promote_queue_track(
    client: &MopidyClient,
    tlid: u64,
    queue: &[MopidyQueueTrack],
    votes: &mut HashMap<u64, u32>,
) {
    let Some((index, target, score)) = queue_vote_promotion(queue, votes, tlid) else {
        return;
    };
    votes.insert(tlid, score);
    if target < index {
        client.rpc_call(
            "core.tracklist.move",
            Some(json!({ "start": index, "end": index + 1, "to_position": target })),
        );
    }
}

fn queue_vote_promotion(
    queue: &[MopidyQueueTrack],
    votes: &HashMap<u64, u32>,
    tlid: u64,
) -> Option<(usize, usize, u32)> {
    let index = queue.iter().position(|track| track.tlid == tlid)?;
    let upcoming_start = queue
        .iter()
        .position(|track| track.current)
        .map(|current| current + 1)
        .unwrap_or(0);
    if index < upcoming_start {
        return None;
    }

    let score = votes.get(&tlid).copied().unwrap_or(0).saturating_add(1);
    let target = (upcoming_start..index)
        .rev()
        .find(|candidate| votes.get(&queue[*candidate].tlid).copied().unwrap_or(0) >= score)
        .map(|candidate| candidate + 1)
        .unwrap_or(upcoming_start);
    Some((index, target, score))
}

fn queue_move_parameters(
    index: usize,
    direction: i8,
    length: usize,
) -> Option<(usize, usize, usize)> {
    match direction.signum() {
        -1 if index > 0 => Some((index, index + 1, index - 1)),
        // Mopidy removes [start:end] before inserting at to_position, so the
        // adjacent item slides into `index` and our item belongs at index + 1.
        1 if index + 1 < length => Some((index, index + 1, index + 1)),
        _ => None,
    }
}

fn parse_browse_items(value: &Value, limit: usize) -> Vec<MopidyBrowseItem> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let uri = item.get("uri")?.as_str()?;
            let item_type = item.get("type")?.as_str()?;
            if !(uri.starts_with("jellyfin:artist:") || uri.starts_with("jellyfin:album:")) {
                return None;
            }
            Some(MopidyBrowseItem {
                uri: uri.to_string(),
                name: item.get("name")?.as_str()?.to_string(),
                item_type: item_type.to_string(),
            })
        })
        .take(limit)
        .collect()
}

fn parse_track_values(value: &Value, limit: usize) -> Vec<MopidySearchTrack> {
    let wrapper = json!([{ "tracks": value.as_array().cloned().unwrap_or_default() }]);
    parse_search_tracks(&wrapper, limit)
}

fn parse_search_tracks(result: &Value, limit: usize) -> Vec<MopidySearchTrack> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();
    let Some(groups) = result.as_array() else {
        return parsed;
    };

    for track in groups
        .iter()
        .filter_map(|group| group.get("tracks").and_then(Value::as_array))
        .flatten()
    {
        let Some(uri) = track.get("uri").and_then(Value::as_str) else {
            continue;
        };
        if !uri.starts_with("jellyfin:track:") || !seen.insert(uri.to_string()) {
            continue;
        }
        let title = track
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("Unknown track")
            .to_string();
        let artist = track
            .get("artists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|artist| artist.get("name").and_then(Value::as_str))
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let album = track
            .get("album")
            .and_then(|album| album.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let duration_ms = track.get("length").and_then(Value::as_u64).unwrap_or(0);
        parsed.push(MopidySearchTrack {
            uri: uri.to_string(),
            title,
            artist: if artist.is_empty() {
                "Unknown artist".to_string()
            } else {
                artist
            },
            album,
            duration_ms,
        });
        if parsed.len() == limit {
            break;
        }
    }
    parsed
}

fn first_tlid_from_tracklist(value: &Value) -> Option<u64> {
    value.as_array()?.first()?.get("tlid")?.as_u64()
}

/// Simple hash function for cache filenames.
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    hash
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_tracklist_id_is_selected_for_explicit_playback() {
        let tracks = json!([
            {"tlid": 42, "track": {"uri": "test:track:first"}},
            {"tlid": 99, "track": {"uri": "test:track:second"}}
        ]);

        assert_eq!(first_tlid_from_tracklist(&tracks), Some(42));
        assert_eq!(first_tlid_from_tracklist(&json!([])), None);
        assert_eq!(first_tlid_from_tracklist(&json!([{"track": {}}])), None);
    }

    #[test]
    fn jellyfin_search_results_are_flattened_deduplicated_and_limited() {
        let results = json!([
            {
                "tracks": [
                    {
                        "uri": "jellyfin:track:first",
                        "name": "First",
                        "artists": [{"name": "Artist One"}, {"name": "Guest"}],
                        "album": {"name": "Album One"},
                        "length": 123000
                    },
                    {
                        "uri": "spotify:track:ignored",
                        "name": "Wrong backend"
                    }
                ]
            },
            {
                "tracks": [
                    {"uri": "jellyfin:track:first", "name": "Duplicate"},
                    {"uri": "jellyfin:track:second", "name": "Second"}
                ]
            }
        ]);

        let parsed = parse_search_tracks(&results, 2);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "First");
        assert_eq!(parsed[0].artist, "Artist One, Guest");
        assert_eq!(parsed[0].album, "Album One");
        assert_eq!(parsed[0].duration_ms, 123_000);
        assert_eq!(parsed[1].uri, "jellyfin:track:second");
        assert_eq!(parsed[1].artist, "Unknown artist");
    }

    #[test]
    fn queue_tracks_are_parsed_in_order_and_mark_the_current_item() {
        let tracks = json!([
            {
                "tlid": 7,
                "track": {
                    "uri": "jellyfin:track:first",
                    "name": "First",
                    "artists": [{"name": "One"}],
                    "album": {"name": "Album A"},
                    "length": 120000
                }
            },
            {
                "tlid": 9,
                "track": {
                    "uri": "jellyfin:track:second",
                    "name": "Second",
                    "artists": [{"name": "Two"}],
                    "album": {"name": "Album B"},
                    "length": 180000
                }
            },
            {"track": {"uri": "jellyfin:track:missing-tlid"}}
        ]);

        let queue = parse_queue_tracks(&tracks, Some(9));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].tlid, 7);
        assert!(!queue[0].current);
        assert_eq!(queue[1].title, "Second");
        assert_eq!(queue[1].duration_ms, 180_000);
        assert!(queue[1].current);
    }

    #[test]
    fn queue_move_parameters_handle_edges_and_adjacent_moves() {
        assert_eq!(queue_move_parameters(2, -1, 4), Some((2, 3, 1)));
        assert_eq!(queue_move_parameters(1, 1, 4), Some((1, 2, 2)));
        assert_eq!(queue_move_parameters(0, -1, 4), None);
        assert_eq!(queue_move_parameters(3, 1, 4), None);
        assert_eq!(queue_move_parameters(1, 0, 4), None);
    }

    #[test]
    fn queue_votes_promote_upcoming_tracks_without_displacing_equal_scores() {
        let track = |tlid, current| MopidyQueueTrack {
            tlid,
            uri: format!("jellyfin:track:{tlid}"),
            title: format!("Track {tlid}"),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_ms: 1,
            current,
            requested_by: None,
            votes: 0,
        };
        let queue = vec![
            track(1, true),
            track(2, false),
            track(3, false),
            track(4, false),
        ];
        let votes = HashMap::from([(2, 2), (3, 1), (4, 1)]);

        // A second vote ties track 4 with track 2 and keeps it behind that
        // earlier equal-scoring request, while moving ahead of track 3.
        assert_eq!(queue_vote_promotion(&queue, &votes, 4), Some((3, 2, 2)));
        assert_eq!(queue_vote_promotion(&queue, &votes, 1), None);
        assert_eq!(queue_vote_promotion(&queue, &votes, 99), None);
    }

    #[test]
    fn persistent_queue_round_trips_order_and_current_track() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-queue-{}-{unique}.toml",
            std::process::id()
        ));
        let queue = vec![
            MopidyQueueTrack {
                tlid: 1,
                uri: "jellyfin:track:first".to_string(),
                title: "First".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                duration_ms: 1,
                current: false,
                requested_by: Some("Alex".to_string()),
                votes: 1,
            },
            MopidyQueueTrack {
                tlid: 2,
                uri: "jellyfin:track:second".to_string(),
                title: "Second".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                duration_ms: 2,
                current: true,
                requested_by: None,
                votes: 0,
            },
        ];

        let expected = PersistentQueue::from_tracks(&queue);
        save_persistent_queue(&path, &expected).unwrap();
        let restored = load_persistent_queue(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(restored, expected);
        assert_eq!(restored.uris[0], "jellyfin:track:first");
        assert_eq!(
            restored.requesters.get("jellyfin:track:first"),
            Some(&"Alex".to_string())
        );
        assert_eq!(restored.votes.get("jellyfin:track:first"), Some(&1));
        assert_eq!(restored.current_index, Some(1));
        assert_eq!(
            restored.current_uri.as_deref(),
            Some("jellyfin:track:second")
        );
        assert_eq!(restored.uris_to_restore(), &["jellyfin:track:second"]);

        let legacy: PersistentQueue = toml::from_str(
            "uris = [\"jellyfin:track:first\", \"jellyfin:track:second\"]\ncurrent_uri = \"jellyfin:track:second\"\n",
        )
        .unwrap();
        assert_eq!(legacy.current_index, None);
        assert_eq!(legacy.uris_to_restore(), &["jellyfin:track:second"]);
    }

    #[test]
    fn persisted_requesters_and_votes_rehydrate_onto_new_mopidy_ids() {
        let saved = PersistentQueue {
            uris: vec!["jellyfin:track:first".to_string()],
            requesters: HashMap::from([("jellyfin:track:first".to_string(), "Alex".to_string())]),
            votes: HashMap::from([("jellyfin:track:first".to_string(), 3)]),
            ..PersistentQueue::default()
        };
        let mut queue = vec![MopidyQueueTrack {
            tlid: 88,
            uri: "jellyfin:track:first".to_string(),
            title: "First".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_ms: 1,
            current: false,
            requested_by: None,
            votes: 0,
        }];
        let mut requesters = HashMap::new();
        let mut votes = HashMap::new();

        apply_queue_metadata(&mut queue, Some(&saved), &mut requesters, &mut votes);

        assert_eq!(queue[0].requested_by.as_deref(), Some("Alex"));
        assert_eq!(queue[0].votes, 3);
        assert_eq!(requesters.get(&88).map(String::as_str), Some("Alex"));
        assert_eq!(votes.get(&88), Some(&3));
    }

    #[test]
    fn browse_items_keep_only_supported_jellyfin_collections() {
        let items = json!([
            {"uri": "jellyfin:artist:one", "name": "Artist One", "type": "artist"},
            {"uri": "jellyfin:album:two", "name": "Album Two", "type": "album"},
            {"uri": "jellyfin:track:ignored", "name": "Track", "type": "track"},
            {"uri": "spotify:artist:ignored", "name": "Spotify", "type": "artist"}
        ]);

        let parsed = parse_browse_items(&items, 10);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Artist One");
        assert_eq!(parsed[1].uri, "jellyfin:album:two");
    }
}
