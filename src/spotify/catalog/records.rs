//! The daemon's catalog records. Only fields consumed by rspc are serialized.
use librespot_core::SpotifyUri;
use librespot_metadata::{Album, Artist, Episode, Show, Track, image::Images};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
pub struct Identity {
    pub id: Option<String>,
    pub uri: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}
impl Identity {
    pub fn new(uri: &SpotifyUri, kind: &'static str) -> Self {
        Self {
            id: uri.to_id().ok(),
            uri: uri.to_string(),
            kind,
        }
    }
}

#[derive(Serialize)]
pub struct Picture {
    pub url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}
pub fn pictures(source: &Images) -> Vec<Picture> {
    let mut result: Vec<_> = source
        .iter()
        .map(|image| Picture {
            url: format!("https://i.scdn.co/image/{}", image.id),
            width: (image.width > 0).then_some(image.width),
            height: (image.height > 0).then_some(image.height),
        })
        .collect();
    result.sort_by_key(|picture| std::cmp::Reverse(picture.width));
    result
}

#[derive(Serialize)]
pub struct Performer {
    #[serde(flatten)]
    pub identity: Identity,
    pub name: String,
    pub images: Vec<Picture>,
}
impl From<&Artist> for Performer {
    fn from(artist: &Artist) -> Self {
        let images = if artist.portraits.is_empty() {
            &artist.portrait_group
        } else {
            &artist.portraits
        };
        Self {
            identity: Identity::new(&artist.id, "artist"),
            name: artist.name.clone(),
            images: pictures(images),
        }
    }
}

#[derive(Serialize)]
pub struct Release {
    #[serde(flatten)]
    pub identity: Identity,
    pub name: String,
    pub artists: Vec<Performer>,
    pub images: Vec<Picture>,
    pub release_date: String,
    pub release_date_precision: &'static str,
    pub album_type: &'static str,
    pub total_tracks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rspc_track_uris: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_group: Option<&'static str>,
}
impl From<&Album> for Release {
    fn from(album: &Album) -> Self {
        let mut release = Self::summary(album);
        release.rspc_track_uris = Some(album.tracks().filter_map(|id| id.to_uri().ok()).collect());
        release
    }
}
impl Release {
    fn summary(album: &Album) -> Self {
        use librespot_metadata::album::AlbumType;
        Self {
            identity: Identity::new(&album.id, "album"),
            name: album.name.clone(),
            artists: album.artists.iter().map(Performer::from).collect(),
            images: pictures(&album.covers),
            release_date: format!("{:04}", album.date.year().max(0)),
            release_date_precision: "year",
            album_type: match album.album_type {
                AlbumType::SINGLE | AlbumType::EP => "single",
                AlbumType::COMPILATION => "compilation",
                _ => "album",
            },
            total_tracks: album.tracks().count(),
            rspc_track_uris: None,
            album_group: None,
        }
    }
}

#[derive(Serialize)]
pub struct Song {
    #[serde(flatten)]
    pub identity: Identity,
    pub name: String,
    pub artists: Vec<Performer>,
    pub album: Release,
    pub duration_ms: u64,
    pub track_number: u32,
    pub disc_number: u32,
    pub explicit: bool,
    pub is_local: bool,
    pub is_playable: bool,
}
impl From<&Track> for Song {
    fn from(track: &Track) -> Self {
        Self {
            identity: Identity::new(&track.id, "track"),
            name: track.name.clone(),
            artists: track.artists.iter().map(Performer::from).collect(),
            album: Release::summary(&track.album),
            duration_ms: track.duration.max(0) as u64,
            track_number: track.number.max(0) as u32,
            disc_number: track.disc_number.max(0) as u32,
            explicit: track.is_explicit,
            is_local: false,
            is_playable: true,
        }
    }
}

#[derive(Serialize)]
pub struct Podcast {
    #[serde(flatten)]
    pub identity: Identity,
    pub name: String,
    pub publisher: String,
    pub description: String,
    pub images: Vec<Picture>,
}
impl From<&Show> for Podcast {
    fn from(show: &Show) -> Self {
        Self {
            identity: Identity::new(&show.id, "show"),
            name: show.name.clone(),
            publisher: show.publisher.clone(),
            description: show.description.clone(),
            images: pictures(&show.covers),
        }
    }
}

#[derive(Serialize)]
pub struct SpokenTrack {
    #[serde(flatten)]
    pub identity: Identity,
    pub name: String,
    pub description: String,
    pub duration_ms: u64,
    pub explicit: bool,
    pub images: Vec<Picture>,
    pub show: serde_json::Value,
}
impl From<&Episode> for SpokenTrack {
    fn from(episode: &Episode) -> Self {
        Self {
            identity: Identity::new(&episode.id, "episode"),
            name: episode.name.clone(),
            description: episode.description.clone(),
            duration_ms: episode.duration.max(0) as u64,
            explicit: episode.is_explicit,
            images: pictures(&episode.covers),
            show: serde_json::json!({"name":episode.show_name}),
        }
    }
}

pub fn value(record: impl Serialize) -> Value {
    // These records contain only finite integers, strings, arrays and options.
    serde_json::to_value(record).expect("catalog record serialization")
}

#[derive(Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub next: Option<String>,
}
impl<T> Page<T> {
    pub fn new(items: Vec<T>, window: super::super::request::Window, total: usize) -> Self {
        let next_offset = window.offset.saturating_add(window.limit);
        Self {
            items,
            offset: window.offset,
            limit: window.limit,
            total,
            next: (next_offset < total).then(|| format!("internal://page?offset={next_offset}")),
        }
    }
}
