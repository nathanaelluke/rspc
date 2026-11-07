// Derived from Spotatui 0.46.0, MIT; see THIRD_PARTY_NOTICES.md.
//! Track metadata and album indexes used by the browser.

use super::json;
use super::metadata::{get_many, get_one};
use super::router::Paging;
use super::session::SpotifySession;
use librespot_metadata::{Album, Artist, Track};
use serde_json::{Value, json};

fn uri(kind: &str, id: &str) -> String {
    format!("spotify:{kind}:{id}")
}

pub async fn track(session: &SpotifySession, id: &str) -> anyhow::Result<Value> {
    let track = get_one::<Track>(session, &uri("track", id)).await?;
    Ok(json::track_full(&track))
}

pub async fn tracks_by_ids(session: &SpotifySession, ids: &str) -> anyhow::Result<Value> {
    let uris: Vec<String> = ids
        .split(',')
        .filter(|id| !id.is_empty())
        .map(|id| uri("track", id))
        .collect();
    Ok(json!({ "tracks": full_tracks(session, &uris).await? }))
}

/// Full track objects in order; items Spotify does not know are left out.
pub async fn full_tracks(session: &SpotifySession, uris: &[String]) -> anyhow::Result<Vec<Value>> {
    Ok(get_many::<Track>(session, uris)
        .await?
        .into_iter()
        .flatten()
        .map(|track| json::track_full(&track))
        .collect())
}

async fn simplified_tracks(
    session: &SpotifySession,
    uris: &[String],
) -> anyhow::Result<Vec<Value>> {
    Ok(get_many::<Track>(session, uris)
        .await?
        .into_iter()
        .flatten()
        .map(|track| json::track_simplified(&track))
        .collect())
}

pub async fn album_tracks(
    session: &SpotifySession,
    id: &str,
    paging: Paging,
) -> anyhow::Result<Value> {
    let album = get_one::<Album>(session, &uri("album", id)).await?;
    let uris: Vec<String> = album.tracks().map(|track| track.to_uri()).collect();
    let window = paging.window(&uris);
    let items = simplified_tracks(session, window).await?;
    Ok(json::page(items, paging.offset, paging.limit, uris.len()))
}

pub async fn full_artists(session: &SpotifySession, uris: &[String]) -> anyhow::Result<Vec<Value>> {
    Ok(get_many::<Artist>(session, uris)
        .await?
        .into_iter()
        .flatten()
        .map(|artist| json::artist_full(&artist))
        .collect())
}

/// Albums, then singles, then compilations, then appearances, each in its
/// current release.
pub async fn artist_albums(
    session: &SpotifySession,
    id: &str,
    paging: Paging,
) -> anyhow::Result<Value> {
    let artist = get_one::<Artist>(session, &uri("artist", id)).await?;
    let groups = [
        ("album", &artist.albums),
        ("single", &artist.singles),
        ("compilation", &artist.compilations),
        ("appears_on", &artist.appears_on_albums),
    ];
    let entries: Vec<(&str, String)> = groups
        .iter()
        .flat_map(|(group, albums)| {
            albums
                .iter()
                .filter_map(|variants| variants.first())
                .map(move |album| (*group, album.to_uri()))
        })
        .collect();
    let window = paging.window(&entries);
    let uris: Vec<String> = window.iter().map(|(_, uri)| uri.clone()).collect();
    let albums = get_many::<Album>(session, &uris).await?;
    let items = window
        .iter()
        .zip(albums)
        .filter_map(|((group, _), album)| {
            let mut value = json::album_index(album.as_deref()?);
            value["album_group"] = json!(group);
            Some(value)
        })
        .collect();
    Ok(json::page(
        items,
        paging.offset,
        paging.limit,
        entries.len(),
    ))
}
