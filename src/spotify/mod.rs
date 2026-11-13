//! Only Spotify session, pairing and catalog protocol code; no application runtime.
mod auth;
mod catalog;
use catalog::{collection as library, playlists, search};
mod protocol {
    include!(concat!(env!("OUT_DIR"), "/spotify-protocol/mod.rs"));
}
mod client;
pub(crate) mod connect_auth;
pub mod connection;
pub mod login;
// Retained for optional Spotify lyrics; the default setup disables lyrics.
#[allow(dead_code)]
mod lyrics;
mod people;
mod request;
use anyhow::{Context, Result, bail};
pub(crate) use request::ApiError;
use serde_json::Value;
use std::sync::OnceLock;
#[derive(Clone, Debug)]
pub enum PlaylistChange {
    Add {
        uris: Vec<String>,
        position: Option<usize>,
    },
    Remove {
        entries: Vec<(usize, String)>,
    },
    Move {
        entries: Vec<(usize, String)>,
        to: usize,
    },
    Clear,
    Rename(String),
}
pub async fn edit_playlist(id: &str, change: PlaylistChange) -> Result<()> {
    let session = connection::current().await.map_err(anyhow::Error::from)?;
    playlists::edit(&session, id, change).await
}
pub async fn add_liked_tracks(uris: &[String]) -> Result<usize> {
    let session = connection::current().await.map_err(anyhow::Error::from)?;
    library::add_tracks(&session, uris).await
}
pub async fn create_playlist(name: &str, uris: Vec<String>) -> Result<()> {
    let session = connection::current().await.map_err(anyhow::Error::from)?;
    let playlist = playlists::create(
        &session,
        Some(&serde_json::json!({"name":name,"public":false})),
    )
    .await?;
    if !uris.is_empty()
        && let Err(error) = playlists::add_items(
            &session,
            playlist["id"].as_str().context("new playlist has no ID")?,
            Some(&serde_json::json!({"uris":uris})),
        )
        .await
    {
        let _ = playlists::unlist(
            &session,
            playlist["uri"]
                .as_str()
                .context("new playlist has no URI")?,
        )
        .await;
        return Err(error);
    }
    Ok(())
}
pub async fn remove_playlist(id: &str) -> Result<()> {
    let session = connection::current().await.map_err(anyhow::Error::from)?;
    playlists::unlist(&session, &format!("spotify:playlist:{id}")).await
}
pub fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .unwrap()
    })
}
pub async fn get(
    path: &str,
    q: &[(&str, String)],
    exclusions: &crate::config::PlaylistExclusions,
) -> Result<Value> {
    let s = connection::current().await.map_err(anyhow::Error::from)?;
    let paging = request::Window::query(q, 100);
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["me", "playlists"] => playlists::user_playlists(&s, q, exclusions).await,
        ["me", "tracks"] => library::saved_tracks(&s, paging).await,
        ["me", "albums"] => library::saved_albums(&s, paging).await,
        ["me", "following"] => library::followed_artists(&s, q).await,
        ["me", "following", "users"] => people::following(&s).await,
        ["users", id, "profile"] => people::profile(&s, id).await,
        ["me", "player", "recently-played"] => {
            Ok(serde_json::json!({"items":load_recent(),"next":null}))
        }
        ["tracks"] => {
            catalog::tracks_by_ids(&s, request::parameter(q, "ids").unwrap_or_default()).await
        }
        ["tracks", id] => catalog::track(&s, id).await,
        ["albums", id] => catalog::album(&s, id).await,
        ["albums", id, "tracks"] => catalog::album_tracks(&s, id, paging).await,
        ["artists", id, "albums"] => catalog::artist_albums(&s, id, paging).await,
        ["playlists", id, "tracks"] => playlists::playlist_items(&s, id, q).await,
        ["search"] => search::search(&s, q).await,
        _ => bail!("unsupported Spotify read: {path}"),
    }
}
pub fn load_recent() -> Vec<Value> {
    std::fs::read(crate::paths::state().join("recent.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
pub fn record(track: &crate::model::TrackInfo) {
    let mut recent = load_recent();
    let value = serde_json::json!({"track":track,"played_at":chrono::Utc::now().to_rfc3339()});
    recent.retain(|v| v["track"]["uri"].as_str() != track.uri.as_deref());
    recent.insert(0, value);
    recent.truncate(50);
    if let Ok(bytes) = serde_json::to_vec(&recent) {
        let _ = crate::paths::write_private_file_atomic(
            &crate::paths::state().join("recent.json"),
            &bytes,
        );
    }
}

// Kept available alongside the lyrics module; fetching is not automatic.
#[allow(dead_code)]
pub async fn cache_lyrics(track: crate::model::TrackInfo) -> Result<()> {
    let client = connection::current().await.map_err(anyhow::Error::from)?;
    lyrics::cache(&client, &track).await
}
