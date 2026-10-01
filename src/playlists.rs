/// Track information for display purposes.
/// Uses owned Strings so it can hold data from Mopidy API.
#[derive(Debug, Clone)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub album_art_path: String,
}

impl Track {
    /// Create a placeholder track shown when nothing is playing.
    pub fn idle() -> Self {
        Self {
            title: "No track playing".to_string(),
            artist: "—".to_string(),
            album: "—".to_string(),
            duration: 0.0,
            album_art_path: String::new(),
        }
    }
}
