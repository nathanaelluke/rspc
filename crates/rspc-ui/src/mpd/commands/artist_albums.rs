//! Optional rspc release-index extension; ordinary MPD uses its normal find path.
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistAlbum {
    pub uri: String,
    pub name: String,
    pub date: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArtistAlbumPage {
    pub albums: Vec<ArtistAlbum>,
    pub next: Option<usize>,
}
