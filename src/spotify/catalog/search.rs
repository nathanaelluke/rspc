//! One Pathfinder response serves all result categories for a search window.
use super::super::{
    client::Client,
    request::{self, Window, parameter},
};
use super::{
    entities,
    records::{self, Page, Podcast, Release},
};
use librespot_core::error::ErrorKind;
use librespot_metadata::{Album, Show};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

// Public identifiers of Spotify's persisted searchDesktop query, newest first.
const HASHES: &[&str] = &[
    "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03",
    "fcad5a3e0d5af727fb76966f06971c19cfa2275e6ff7671196753e008611873c",
    "3c9d3f60dac5dea3876b6db3f534192b1c1d90032c4233c1bbaba526db41eb31",
    "4801118d4a100f756e833d33984436a3899cff359c532f8fd3aaf174b60b3b49",
];
#[derive(Hash, PartialEq, Eq, Clone)]
struct Key {
    term: String,
    offset: usize,
    limit: usize,
}
type Response = Arc<Mutex<Option<(Instant, Value)>>>;
#[derive(Default)]
pub struct SearchCache {
    entries: Mutex<HashMap<Key, Response>>,
    hash: AtomicUsize,
}

fn normalized(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            let word = word
                .split_once(':')
                .filter(|(prefix, _)| matches!(*prefix, "track" | "artist" | "album" | "isrc"))
                .map_or(word, |(_, text)| text);
            word.replace('"', "")
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
async fn response(client: &Client, key: &Key) -> anyhow::Result<Value> {
    let gate = {
        let mut entries = client.search.entries.lock().await;
        if entries.len() >= 64 {
            entries.retain(|_, entry| Arc::strong_count(entry) > 1);
        }
        Arc::clone(entries.entry(key.clone()).or_default())
    };
    let mut cached = gate.lock().await;
    if let Some((_, answer)) = cached
        .as_ref()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(15))
    {
        return Ok(answer.clone());
    }
    let variables = json!({"searchTerm":key.term,"offset":key.offset,"limit":key.limit,"numberOfTopResults":5,"includeAudiobooks":false,"includeArtistHasConcertsField":false,"includePreReleases":false,"includeLocalConcertsField":false,"includeAuthors":false});
    let first = client.search.hash.load(Ordering::Relaxed);
    for step in 0..HASHES.len() {
        let index = (first + step) % HASHES.len();
        let answer = match client
            .search_query("searchDesktop", HASHES[index], variables.clone())
            .await
        {
            Ok(answer) => answer,
            Err(error) if error.kind == ErrorKind::FailedPrecondition => continue,
            Err(error) => return Err(request::upstream(error)),
        };
        if let Some(result) = answer.pointer("/data/searchV2").filter(|v| v.is_object()) {
            client.search.hash.store(index, Ordering::Relaxed);
            *cached = Some((Instant::now(), result.clone()));
            return Ok(result.clone());
        }
        let retired = answer["errors"].as_array().is_some_and(|errors| {
            errors.iter().any(|error| {
                error["message"].as_str().is_some_and(|s| {
                    matches!(s, "PersistedQueryNotFound" | "PersistedQueryNotSupported")
                }) || error.pointer("/extensions/code").and_then(Value::as_str)
                    == Some("PERSISTED_QUERY_NOT_FOUND")
            })
        });
        if !retired {
            anyhow::bail!("Spotify search returned an invalid response");
        }
    }
    anyhow::bail!("Spotify's search query has changed; update rspc")
}
fn ids(section: &Value, pointer: &str) -> Vec<String> {
    section["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            row.pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}
fn playlist(item: &Value) -> Option<Value> {
    let uri = item["uri"].as_str()?;
    let id = uri.strip_prefix("spotify:playlist:")?;
    let owner = &item["ownerV2"]["data"];
    let owner_id = owner["username"].as_str().unwrap_or_default();
    let images: Vec<Value> = item["images"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|image| image["sources"].get(0))
        .cloned()
        .collect();
    Some(
        json!({"id":id,"uri":uri,"name":item["name"],"description":item["description"],"images":images,"owner":{"id":owner_id,"display_name":owner["name"].as_str().unwrap_or(owner_id)},"items":{"total":0}}),
    )
}
pub async fn search(client: &Client, query: &[(&str, String)]) -> anyhow::Result<Value> {
    let term = normalized(parameter(query, "q").unwrap_or_default());
    anyhow::ensure!(!term.is_empty(), "Search text is empty");
    let requested: Vec<_> = parameter(query, "type")
        .unwrap_or("track")
        .split(',')
        .collect();
    anyhow::ensure!(
        requested
            .iter()
            .all(|kind| matches!(*kind, "track" | "album" | "artist" | "playlist" | "show")),
        "Unsupported search category"
    );
    let mut window = Window::query(query, 20);
    window.limit = window.limit.min(50);
    let result = response(
        client,
        &Key {
            term,
            offset: window.offset,
            limit: window.limit,
        },
    )
    .await?;
    let mut answer = serde_json::Map::new();
    for kind in requested {
        let (key, section, pointer) = match kind {
            "track" => ("tracks", "tracksV2", "/item/data/uri"),
            "album" => ("albums", "albumsV2", "/data/uri"),
            "artist" => ("artists", "artists", "/data/uri"),
            "playlist" => ("playlists", "playlists", "/data/uri"),
            _ => ("shows", "podcasts", "/data/uri"),
        };
        let section = &result[section];
        let uris = ids(section, pointer);
        let rows = match kind {
            "track" => super::full_tracks(client, &uris).await?,
            "artist" => super::full_artists(client, &uris).await?,
            "album" => entities::many::<Album>(client, &uris)
                .await?
                .into_iter()
                .flatten()
                .map(|album| records::value(Release::from(album.as_ref())))
                .collect(),
            "show" => entities::many::<Show>(client, &uris)
                .await?
                .into_iter()
                .flatten()
                .map(|show| records::value(Podcast::from(show.as_ref())))
                .collect(),
            _ => section["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| playlist(&row["data"]))
                .collect(),
        };
        let total = section["totalCount"].as_u64().unwrap_or(0) as usize;
        answer.insert(key.into(), records::value(Page::new(rows, window, total)));
    }
    Ok(Value::Object(answer))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_known_field_filters_are_removed() {
        assert_eq!(
            normalized(r#"track:"A Song" artist:"Someone""#),
            "A Song Someone"
        );
        assert_eq!(
            normalized("isrc:EXAMPLE other:value"),
            "EXAMPLE other:value"
        );
    }
}
