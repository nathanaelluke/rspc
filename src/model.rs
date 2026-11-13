//! rspc's persisted queue records and lazy artist release index.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ArtistAlbumPage {
    pub albums: Vec<ArtistAlbum>,
    pub next: Option<usize>,
    #[serde(default)]
    pub total: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistAlbum {
    pub uri: String,
    pub name: String,
    pub date: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: Option<String>,
    pub name: String,
}

/// Field names are the queue-file format; defaults keep older queues readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    pub uri: Option<String>,
    pub id: Option<String>,
    pub name: String,
    pub duration_ms: u64,
    pub artists: Vec<String>,
    pub artist_refs: Vec<ArtistRef>,
    pub album: String,
    pub album_id: Option<String>,
    pub album_artists: Vec<String>,
    pub genres: Vec<String>,
    pub release_date: Option<String>,
    pub added_at: Option<chrono::DateTime<chrono::Utc>>,
    pub track_number: u32,
    pub image_url: Option<String>,
    pub explicit: bool,
    pub is_local: bool,
    pub is_playable: bool,
}
impl TrackInfo {
    pub fn album_artists(&self) -> &[String] {
        if self.album_artists.is_empty() {
            &self.artists
        } else {
            &self.album_artists
        }
    }
}
impl Default for TrackInfo {
    fn default() -> Self {
        Self {
            uri: None,
            id: None,
            name: String::new(),
            duration_ms: 0,
            artists: Vec::new(),
            artist_refs: Vec::new(),
            album: String::new(),
            album_id: None,
            album_artists: Vec::new(),
            genres: Vec::new(),
            release_date: None,
            added_at: None,
            track_number: 0,
            image_url: None,
            explicit: false,
            is_local: false,
            is_playable: true,
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn old_queue_records_keep_their_playability_and_album_defaults() {
        let old = r#"{"uri":"spotify:track:fixture","name":"Fixture","artists":["An artist"],"album":"A release","duration_ms":1000}"#;
        let track: super::TrackInfo = serde_json::from_str(old).unwrap();
        assert!(track.is_playable);
        assert!(track.artist_refs.is_empty());
        let encoded = serde_json::to_value(&track).unwrap();
        let decoded: super::TrackInfo = serde_json::from_value(encoded).unwrap();
        assert_eq!(track, decoded);
    }
}
