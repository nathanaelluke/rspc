//! Events shared by the Spotify and MPD players.
use crate::model::TrackInfo;
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Spotify,
    Mpd,
}
impl Backend {
    pub fn for_uri(uri: &str) -> Result<Self> {
        if uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:") {
            Ok(Self::Spotify)
        } else if uri.starts_with("mpd:") {
            Ok(Self::Mpd)
        } else {
            bail!("unsupported playback URI")
        }
    }
}
#[derive(Debug)]
pub struct AudioUpdate {
    pub backend: Backend,
    pub epoch: u64,
    pub event: AudioEvent,
}
impl AudioUpdate {
    pub(crate) fn spotify(epoch: u64, event: AudioEvent) -> Self {
        Self {
            backend: Backend::Spotify,
            epoch,
            event,
        }
    }
}
#[derive(Debug)]
pub enum AudioEvent {
    State {
        request: u64,
        position: u64,
        playing: bool,
    },
    Position {
        request: u64,
        position: u64,
    },
    Ended {
        request: u64,
    },
    Error(String),
    ExternalState {
        track: TrackInfo,
        position: u64,
        playing: bool,
        stopped: bool,
    },
    Authenticated,
    Inactive,
    Volume(u8),
    LibraryChanged,
    Metadata {
        request: u64,
        track: TrackInfo,
    },
}
