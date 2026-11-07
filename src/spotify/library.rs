// Derived from Spotatui 0.46.0, MIT; see THIRD_PARTY_NOTICES.md.
//! The user's library from collection v2: liked songs, saved albums and shows,
//! followed artists.

use super::catalog::full_artists;
use super::json;
use super::metadata::get_many;
use super::router::{Paging, query_value, retry_read, upstream_error};
use super::session::SpotifySession;
use chrono::{DateTime, Utc};
use librespot_metadata::{Album, Track};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(30);
const PAGE_SIZE: i32 = 300;
/// A runaway pagination token must not loop forever.
const MAX_PAGES: usize = 200;

#[derive(Clone)]
struct Entry {
    uri: String,
    added_at: i64,
}

struct CachedSet {
    entries: Arc<Vec<Entry>>,
    fetched: Instant,
}

type CollectionCache = HashMap<&'static str, Arc<Mutex<Option<CachedSet>>>>;

fn cache() -> &'static Mutex<CollectionCache> {
    static CACHE: OnceLock<Mutex<CollectionCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The set's items, newest first.
async fn entries(session: &SpotifySession, set: &'static str) -> anyhow::Result<Arc<Vec<Entry>>> {
    // Coalesce one collection's reads without serializing unrelated collections
    // behind its network requests (notably artist and album startup warming).
    let slot = cache().lock().await.entry(set).or_default().clone();
    let mut cache = slot.lock().await;
    if let Some(cached) = cache.as_ref()
        && cached.fetched.elapsed() < CACHE_TTL
    {
        return Ok(cached.entries.clone());
    }
    let mut entries = Vec::new();
    let mut token = String::new();
    for _ in 0..MAX_PAGES {
        let spclient = session.session().spclient();
        let page = retry_read(|| spclient.get_collection_page(set, &token, PAGE_SIZE))
            .await
            .map_err(upstream_error)?;
        entries.extend(
            page.items
                .iter()
                .filter(|item| !item.is_removed)
                .map(|item| Entry {
                    uri: item.uri.clone(),
                    added_at: i64::from(item.added_at),
                }),
        );
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.added_at));
    let entries = Arc::new(entries);
    *cache = Some(CachedSet {
        entries: Arc::clone(&entries),
        fetched: Instant::now(),
    });
    Ok(entries)
}

fn with_prefix(entries: Arc<Vec<Entry>>, prefix: &str) -> Vec<Entry> {
    entries
        .iter()
        .filter(|entry| entry.uri.starts_with(prefix))
        .cloned()
        .collect()
}

fn added_at(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

pub async fn saved_tracks(session: &SpotifySession, paging: Paging) -> anyhow::Result<Value> {
    let tracks = with_prefix(entries(session, "collection").await?, "spotify:track:");
    let window = paging.window(&tracks);
    let uris: Vec<String> = window.iter().map(|entry| entry.uri.clone()).collect();
    let items = window
    .iter()
    .zip(get_many::<Track>(session, &uris).await?)
    .filter_map(|(entry, track)| {
      Some(json!({ "added_at": added_at(entry.added_at), "track": json::track_full(track.as_deref()?) }))
    })
    .collect();
    Ok(json::page(items, paging.offset, paging.limit, tracks.len()))
}

pub async fn saved_albums(session: &SpotifySession, paging: Paging) -> anyhow::Result<Value> {
    let albums = with_prefix(entries(session, "collection").await?, "spotify:album:");
    let window = paging.window(&albums);
    let uris: Vec<String> = window.iter().map(|entry| entry.uri.clone()).collect();
    let metadata = get_many::<Album>(session, &uris).await?;
    let items=window.iter().zip(metadata).filter_map(|(entry,album)|Some(json!({"added_at":added_at(entry.added_at),"album":json::album_index(album.as_deref()?)}))).collect();
    Ok(json::page(items, paging.offset, paging.limit, albums.len()))
}

/// Cursor paging: `after` names the last artist id of the previous page.
pub async fn followed_artists(
    session: &SpotifySession,
    query: &[(&str, String)],
) -> anyhow::Result<Value> {
    let artists = entries(session, "artist").await?;
    let limit = Paging::from_query(query, 20).limit;
    let start = query_value(query, "after")
        .and_then(|after| {
            artists
                .iter()
                .position(|entry| entry.uri == format!("spotify:artist:{after}"))
        })
        .map_or(0, |index| index + 1);
    let window: Vec<String> = artists
        .iter()
        .skip(start)
        .take(limit)
        .map(|entry| entry.uri.clone())
        .collect();
    let items = full_artists(session, &window).await?;
    let more = start + window.len() < artists.len();
    let after = window.last().and_then(|uri| uri.rsplit(':').next());
    Ok(json!({
      "artists": {
        "href": "",
        "items": items,
        "limit": limit,
        "total": artists.len(),
        "next": more.then_some("internal://following"),
        "cursors": { "after": after },
      }
    }))
}
