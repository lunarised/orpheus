use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HISTORY_ENTRIES: usize = 250;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub uri: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source: String,
    pub duration_ms: u64,
    pub played_at_unix: u64,
}

pub struct HistoryStore {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
}

impl HistoryStore {
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|contents| toml::from_str::<HistoryFile>(&contents).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn entries_newest_first(&self) -> Vec<HistoryEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        id: String,
        uri: Option<String>,
        title: String,
        artist: String,
        album: String,
        source: String,
        duration_ms: u64,
    ) -> bool {
        if self.entries.last().is_some_and(|entry| entry.id == id) {
            return false;
        }
        self.entries.push(HistoryEntry {
            id,
            uri,
            title,
            artist,
            album,
            source,
            duration_ms,
            played_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            self.entries
                .drain(..self.entries.len() - MAX_HISTORY_ENTRIES);
        }
        if let Err(error) = save_history(&self.path, &self.entries) {
            eprintln!("[HISTORY] {error}");
        }
        true
    }
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

fn save_history(path: &Path, entries: &[HistoryEntry]) -> Result<(), String> {
    let serialized = toml::to_string_pretty(&HistoryFile {
        entries: entries.to_vec(),
    })
    .map_err(|error| format!("could not serialize playback history: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, serialized)
        .map_err(|error| format!("could not write '{}': {error}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("could not replace '{}': {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_durable_newest_first_and_skips_consecutive_duplicates() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "orpheus-history-{}-{unique}.toml",
            std::process::id()
        ));
        let mut history = HistoryStore::load(path.clone());
        history.record(
            "one".into(),
            Some("jellyfin:track:one".into()),
            "First".into(),
            "Artist".into(),
            "Album".into(),
            "Mopidy".into(),
            10_000,
        );
        history.record(
            "one".into(),
            Some("jellyfin:track:one".into()),
            "First".into(),
            "Artist".into(),
            "Album".into(),
            "Mopidy".into(),
            10_000,
        );
        history.record(
            "two".into(),
            None,
            "Second".into(),
            "Artist".into(),
            "Album".into(),
            "Spotify Connect".into(),
            20_000,
        );

        let restored = HistoryStore::load(path.clone()).entries_newest_first();
        std::fs::remove_file(path).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].title, "Second");
        assert_eq!(restored[1].title, "First");
    }
}
