// Derived from Spotatui 0.46.0, MIT; see THIRD_PARTY_NOTICES.md.
//! Catalogue metadata from extended-metadata, many items per request, cached.

use super::router::{retry_read, upstream_error};
use super::session::SpotifySession;
use librespot_core::SpotifyUri;
use librespot_metadata::{Album, Artist, Episode, Metadata, Show, Track};
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use protobuf::{EnumOrUnknown, Message};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Spotify answers larger batches, but a failed one then costs more.
const BATCH_SIZE: usize = 100;
const CACHE_CAPACITY: usize = 4000;

type MetadataMap<T> = HashMap<String, (Arc<T>, Instant)>;

pub trait Kind: Metadata + Send + Sync {
    const EXTENSION: ExtensionKind;
    fn cache() -> &'static Mutex<MetadataMap<Self>>;
}

macro_rules! kind {
    ($type:ty, $extension:ident) => {
        impl Kind for $type {
            const EXTENSION: ExtensionKind = ExtensionKind::$extension;
            fn cache() -> &'static Mutex<MetadataMap<Self>> {
                static CACHE: OnceLock<Mutex<MetadataMap<$type>>> = OnceLock::new();
                CACHE.get_or_init(|| Mutex::new(HashMap::new()))
            }
        }
    };
}

kind!(Track, TRACK_V4);
kind!(Album, ALBUM_V4);
kind!(Artist, ARTIST_V4);
kind!(Episode, EPISODE_V4);
kind!(Show, SHOW_V4);

/// Metadata for each uri, in the order of `uris`. An item Spotify does not
/// know is `None`.
pub async fn get_many<T: Kind>(
    session: &SpotifySession,
    uris: &[String],
) -> anyhow::Result<Vec<Option<Arc<T>>>> {
    let mut found: HashMap<String, Arc<T>> = HashMap::new();
    let mut missing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    {
        let mut cache = T::cache().lock().unwrap_or_else(|e| e.into_inner());
        for uri in uris {
            match cache.get_mut(uri) {
                Some((item, used)) => {
                    *used = Instant::now();
                    found.insert(uri.clone(), Arc::clone(item));
                }
                None if seen.insert(uri.clone()) => missing.push(uri.clone()),
                None => {}
            }
        }
    }

    use futures::{StreamExt, TryStreamExt, stream};
    let chunks: Vec<Vec<String>> = missing.chunks(BATCH_SIZE).map(|s| s.to_vec()).collect();
    let batches: Vec<_> = stream::iter(chunks)
        .map(|batch| async move { fetch_batch::<T>(session, &batch).await })
        .buffer_unordered(4)
        .try_collect()
        .await?;

    let mut cache = T::cache().lock().unwrap_or_else(|e| e.into_inner());
    for items in batches {
        for (uri, item) in items {
            cache.insert(uri.clone(), (Arc::clone(&item), Instant::now()));
            found.insert(uri, item);
        }
    }

    trim_cache(&mut cache, CACHE_CAPACITY);
    Ok(uris.iter().map(|uri| found.get(uri).cloned()).collect())
}

fn trim_cache<T>(cache: &mut MetadataMap<T>, capacity: usize) {
    let excess = cache.len().saturating_sub(capacity);
    if excess == 0 {
        return;
    }
    let mut oldest: Vec<_> = cache
        .iter()
        .map(|(key, (_, used))| (key.clone(), *used))
        .collect();
    oldest.sort_unstable_by_key(|(_, used)| *used);
    for (key, _) in oldest.into_iter().take(excess) {
        cache.remove(&key);
    }
}

pub async fn get_one<T: Kind>(session: &SpotifySession, uri: &str) -> anyhow::Result<Arc<T>> {
    get_many::<T>(session, &[uri.to_string()])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| super::router::api_error(reqwest::StatusCode::NOT_FOUND, "Not found"))
}

async fn fetch_batch<T: Kind>(
    session: &SpotifySession,
    uris: &[String],
) -> anyhow::Result<Vec<(String, Arc<T>)>> {
    let request = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|uri| EntityRequest {
                entity_uri: uri.clone(),
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(T::EXTENSION),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let spclient = session.session().spclient();
    let response = retry_read(|| spclient.get_extended_metadata(request.clone()))
        .await
        .map_err(upstream_error)?;

    let mut items = Vec::new();
    for array in response.extended_metadata {
        for data in array.extension_data {
            let Some(any) = data.extension_data.as_ref() else {
                continue;
            };
            let Ok(uri) = SpotifyUri::from_uri(&data.entity_uri) else {
                continue;
            };
            let Ok(message) = T::Message::parse_from_bytes(&any.value) else {
                continue;
            };
            if let Ok(item) = T::parse(&message, &uri) {
                items.push((data.entity_uri.clone(), Arc::new(item)));
            }
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn eviction_retains_recent_metadata_instead_of_flushing_the_whole_cache() {
        let now = Instant::now();
        let mut cache = MetadataMap::from([
            ("old".into(), (Arc::new(1), now - Duration::from_secs(10))),
            ("hot".into(), (Arc::new(2), now)),
            ("new".into(), (Arc::new(3), now)),
        ]);
        trim_cache(&mut cache, 2);
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key("hot"));
        assert!(cache.contains_key("new"));
        trim_cache(&mut cache, 2);
        assert_eq!(cache.len(), 2);
    }
}
