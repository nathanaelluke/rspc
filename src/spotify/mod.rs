//! Only Spotify session, pairing and catalog protocol code; no application runtime.
mod catalog;
pub mod credentials;
mod json;
mod library;
// Retained for optional Spotify lyrics; the default setup disables lyrics.
#[allow(dead_code)]
mod lyrics;
mod metadata;
mod people;
mod playlists;
mod router;
pub(crate) use router::ApiError;
mod search;
pub mod session;
use anyhow::{Context, Result, bail};
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
    let session = session::current()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    playlists::edit(&session, id, change).await
}
pub async fn create_playlist(name: &str, uris: Vec<String>) -> Result<()> {
    let session = session::current()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
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
    let session = session::current()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    playlists::unlist(&session, &format!("spotify:playlist:{id}")).await
}
pub fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
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
    let s = session::current()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let paging = router::Paging::from_query(q, 100);
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
            catalog::tracks_by_ids(&s, router::query_value(q, "ids").unwrap_or_default()).await
        }
        ["tracks", id] => catalog::track(&s, id).await,
        ["albums", id] => {
            let album =
                metadata::get_one::<librespot_metadata::Album>(&s, &format!("spotify:album:{id}"))
                    .await?;
            Ok(json::album_index(&album))
        }
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
    let uri = track.uri.as_deref().context("missing lyric track URI")?;
    let id = uri
        .strip_prefix("spotify:track:")
        .context("not a Spotify track")?;
    anyhow::ensure!(
        id.chars().all(|c| c.is_ascii_alphanumeric()),
        "invalid lyric URI"
    );
    let dir = crate::paths::cache().join("lyrics");
    let path = dir.join(format!("{uri}.lrc"));
    if path.is_file() {
        return Ok(());
    }
    let session = session::current()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let value = lyrics::track_lyrics(&session, id).await?;
    let Some(lyrics) = lyrics::parse_lyrics(value) else {
        return Ok(());
    };
    let clean = |s: &str| s.replace(['\r', '\n'], " ");
    let mut text = format!(
        "[ti:{}]\n[ar:{}]\n[al:{}]\n",
        clean(&track.name),
        clean(&track.artists.join(", ")),
        clean(&track.album)
    );
    for (time, words) in lyrics.lines {
        text.push_str(&format!(
            "[{:02}:{:02}.{:03}]{}\n",
            time / 60000,
            (time / 1000) % 60,
            time % 1000,
            clean(&words)
        ));
    }
    tokio::task::spawn_blocking(move || -> Result<()> {
        crate::paths::write_private_file_atomic(&path, text.as_bytes())?;
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)?
            .write_all(b"\n")?;
        Ok(())
    })
    .await??;
    Ok(())
}
