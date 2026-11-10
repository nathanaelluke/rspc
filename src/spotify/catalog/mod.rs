//! Spotify catalog operations expressed in rspc's own record format.
pub(super) mod collection;
pub(super) mod entities;
pub(super) mod playlists;
pub(super) mod records;
pub(super) mod search;
#[cfg(test)]
mod tests;
use super::{client::Client, request::Window};
use entities::{many, one};
use librespot_metadata::{Album, Artist, Track};
use records::{Page, Performer, Release, Song, value};
use serde_json::Value;

pub async fn track(client: &Client, id: &str) -> anyhow::Result<Value> {
    let metadata = one::<Track>(client, &format!("spotify:track:{id}")).await?;
    Ok(value(Song::from(metadata.as_ref())))
}
pub async fn album(client: &Client, id: &str) -> anyhow::Result<Value> {
    let metadata = one::<Album>(client, &format!("spotify:album:{id}")).await?;
    Ok(value(Release::from(metadata.as_ref())))
}
pub async fn tracks_by_ids(client: &Client, ids: &str) -> anyhow::Result<Value> {
    let uris = ids
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|id| format!("spotify:track:{id}"))
        .collect::<Vec<_>>();
    Ok(serde_json::json!({"tracks":full_tracks(client,&uris).await?}))
}
pub async fn full_tracks(client: &Client, ids: &[String]) -> anyhow::Result<Vec<Value>> {
    Ok(many::<Track>(client, ids)
        .await?
        .into_iter()
        .flatten()
        .map(|track| value(Song::from(track.as_ref())))
        .collect())
}
pub async fn full_artists(client: &Client, ids: &[String]) -> anyhow::Result<Vec<Value>> {
    Ok(many::<Artist>(client, ids)
        .await?
        .into_iter()
        .flatten()
        .map(|artist| value(Performer::from(artist.as_ref())))
        .collect())
}
pub async fn album_tracks(client: &Client, id: &str, window: Window) -> anyhow::Result<Value> {
    let album = one::<Album>(client, &format!("spotify:album:{id}")).await?;
    let ids = album
        .tracks()
        .map(|uri| uri.to_uri())
        .collect::<Result<Vec<_>, _>>()?;
    let songs = full_tracks(client, window.select(&ids)).await?;
    Ok(value(Page::new(songs, window, ids.len())))
}
pub async fn artist_albums(client: &Client, id: &str, window: Window) -> anyhow::Result<Value> {
    let artist = one::<Artist>(client, &format!("spotify:artist:{id}")).await?;
    let mut releases = Vec::new();
    let mut unique = std::collections::HashSet::new();
    for (label, groups) in [
        ("album", &artist.albums),
        ("single", &artist.singles),
        ("compilation", &artist.compilations),
        ("appears_on", &artist.appears_on_albums),
    ] {
        for group in groups.iter() {
            if let Some(uri) = group.iter().find_map(|id| id.to_uri().ok())
                && unique.insert(uri.clone())
            {
                releases.push((uri, label));
            }
        }
    }
    let selected = window.select(&releases);
    let ids = selected
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for ((_, group), album) in selected.iter().zip(many::<Album>(client, &ids).await?) {
        if let Some(album) = album {
            let mut row = Release::from(album.as_ref());
            row.album_group = Some(group);
            rows.push(row);
        }
    }
    Ok(value(Page::new(rows, window, releases.len())))
}
