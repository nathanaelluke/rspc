use serde::{Deserialize, Serialize};

/// A release index entry, without downloading its track metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistAlbum {
    pub uri: String,
    pub name: String,
    pub date: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtistAlbumPage {
    pub albums: Vec<ArtistAlbum>,
    pub next: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub uri: Option<String>,
    pub name: String,
    pub artists: Vec<String>,
    #[serde(default)]
    pub album_artists: Vec<String>,
    pub album: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub artist_refs: Vec<ArtistRef>,
    #[serde(default = "default_true")]
    pub is_playable: bool,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub track_number: u32,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: Option<String>,
    pub name: String,
}

fn default_true() -> bool {
    true
}
