//! Saved collections. Each account and collection has its own refresh lock.
use super::super::{
    client::Client,
    protocol::collection::CollectionItem,
    request::{self, Window, parameter},
};
use super::{
    entities,
    records::{self, Page, Release, Song},
};
use librespot_metadata::{Album, Track};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

type Snapshot = (Instant, Arc<Vec<CollectionItem>>);
#[derive(Default)]
pub struct CollectionCache {
    sets: Mutex<HashMap<&'static str, Arc<Mutex<Option<Snapshot>>>>>,
}

async fn load(client: &Client, name: &'static str) -> anyhow::Result<Arc<Vec<CollectionItem>>> {
    let gate = client
        .collections
        .sets
        .lock()
        .await
        .entry(name)
        .or_default()
        .clone();
    let mut slot = gate.lock().await;
    if let Some((_, entries)) = slot
        .as_ref()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
    {
        return Ok(Arc::clone(entries));
    }
    let mut token = String::new();
    let mut seen = HashSet::new();
    let mut latest = HashMap::new();
    loop {
        anyhow::ensure!(
            seen.len() < 200 && seen.insert(token.clone()),
            "Collection pagination did not terminate"
        );
        let page = request::read(|| client.collection_page(name, &token, 300))
            .await
            .map_err(request::upstream)?;
        for entry in page.items {
            latest.insert(entry.uri.clone(), entry);
        }
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    let mut entries: Vec<_> = latest
        .into_values()
        .filter(|entry| !entry.is_removed)
        .collect();
    entries.sort_by(|a, b| b.added_at.cmp(&a.added_at).then_with(|| a.uri.cmp(&b.uri)));
    let entries = Arc::new(entries);
    *slot = Some((Instant::now(), Arc::clone(&entries)));
    Ok(entries)
}
fn timestamp(seconds: i32) -> String {
    chrono::DateTime::from_timestamp(i64::from(seconds), 0)
        .unwrap_or_default()
        .to_rfc3339()
}
async fn saved(client: &Client, window: Window, albums: bool) -> anyhow::Result<Value> {
    let snapshot = load(client, "collection").await?;
    let prefix = if albums {
        "spotify:album:"
    } else {
        "spotify:track:"
    };
    let entries: Vec<_> = snapshot
        .iter()
        .filter(|row| row.uri.starts_with(prefix))
        .collect();
    let selected = window.select(&entries);
    let ids: Vec<_> = selected.iter().map(|row| row.uri.clone()).collect();
    let values = if albums {
        entities::many::<Album>(client, &ids)
            .await?
            .into_iter()
            .map(|album| album.map(|album| records::value(Release::from(album.as_ref()))))
            .collect::<Vec<_>>()
    } else {
        entities::many::<Track>(client, &ids)
            .await?
            .into_iter()
            .map(|track| track.map(|track| records::value(Song::from(track.as_ref()))))
            .collect::<Vec<_>>()
    };
    let key = if albums { "album" } else { "track" };
    let rows: Vec<_> = selected
        .iter()
        .zip(values)
        .filter_map(|(entry, value)| {
            value.map(|value| {
                let mut row = json!({"added_at":timestamp(entry.added_at)});
                row[key] = value;
                row
            })
        })
        .collect();
    Ok(records::value(Page::new(rows, window, entries.len())))
}
pub async fn saved_tracks(client: &Client, window: Window) -> anyhow::Result<Value> {
    saved(client, window, false).await
}
pub async fn saved_albums(client: &Client, window: Window) -> anyhow::Result<Value> {
    saved(client, window, true).await
}

pub async fn followed_artists(client: &Client, query: &[(&str, String)]) -> anyhow::Result<Value> {
    let entries = load(client, "artist").await?;
    let artists: Vec<_> = entries
        .iter()
        .filter(|entry| entry.uri.starts_with("spotify:artist:"))
        .collect();
    let offset = parameter(query, "after")
        .and_then(|id| {
            artists
                .iter()
                .position(|row| row.uri.strip_prefix("spotify:artist:") == Some(id))
        })
        .map_or(0, |n| n + 1);
    let window = Window {
        offset,
        limit: Window::query(query, 20).limit,
    };
    let selected = window.select(&artists);
    let ids: Vec<_> = selected.iter().map(|row| row.uri.clone()).collect();
    let rendered = super::full_artists(client, &ids).await?;
    let cursor = selected
        .last()
        .and_then(|row| row.uri.strip_prefix("spotify:artist:"));
    let page = Page::new(rendered, window, artists.len());
    let mut result = records::value(page);
    result["cursors"] = json!({"after":cursor});
    Ok(json!({"artists":result}))
}
