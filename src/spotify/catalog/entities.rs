//! Batched metadata with caches scoped to a single authenticated client.
use super::super::{client::Client, request};
use futures::{StreamExt, TryStreamExt, stream};
use librespot_core::SpotifyUri;
use librespot_metadata::{Album, Artist, Episode, Metadata, Show, Track};
use librespot_protocol::{
    extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
    extension_kind::ExtensionKind,
};
use protobuf::{EnumOrUnknown, Message};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const CAPACITY: usize = 4096;
struct Cached<T> {
    value: Option<Arc<T>>,
    expires: Instant,
    used: Instant,
}
pub struct Store<T> {
    values: Mutex<HashMap<String, Cached<T>>>,
    fetch: tokio::sync::Mutex<()>,
}
impl<T> Default for Store<T> {
    fn default() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            fetch: tokio::sync::Mutex::new(()),
        }
    }
}
#[derive(Default)]
pub struct EntityCache {
    tracks: Store<Track>,
    albums: Store<Album>,
    artists: Store<Artist>,
    episodes: Store<Episode>,
    shows: Store<Show>,
}
pub trait Entity: Metadata + Send + Sync {
    const KIND: ExtensionKind;
    fn store(client: &Client) -> &Store<Self>;
}
macro_rules! entity {
    ($ty:ty,$field:ident,$kind:ident) => {
        impl Entity for $ty {
            const KIND: ExtensionKind = ExtensionKind::$kind;
            fn store(client: &Client) -> &Store<Self> {
                &client.entities.$field
            }
        }
    };
}
entity!(Track, tracks, TRACK_V4);
entity!(Album, albums, ALBUM_V4);
entity!(Artist, artists, ARTIST_V4);
entity!(Episode, episodes, EPISODE_V4);
entity!(Show, shows, SHOW_V4);

impl<T> Store<T> {
    fn lookup(&self, ids: &[String]) -> (Vec<Option<Arc<T>>>, Vec<String>) {
        let now = Instant::now();
        let mut cache = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut missing = HashSet::new();
        let ordered = ids
            .iter()
            .map(
                |id| match cache.get_mut(id).filter(|value| value.expires > now) {
                    Some(value) => {
                        value.used = now;
                        value.value.clone()
                    }
                    None => {
                        missing.insert(id.clone());
                        None
                    }
                },
            )
            .collect();
        (ordered, missing.into_iter().collect())
    }
    fn insert(&self, requested: &[String], mut received: HashMap<String, Arc<T>>) {
        let now = Instant::now();
        let mut cache = self.values.lock().unwrap_or_else(|e| e.into_inner());
        for id in requested {
            let value = received.remove(id);
            let ttl = if value.is_some() {
                Duration::from_secs(600)
            } else {
                Duration::from_secs(30)
            };
            cache.insert(
                id.clone(),
                Cached {
                    value,
                    expires: now + ttl,
                    used: now,
                },
            );
        }
        if cache.len() > CAPACITY {
            let mut age: Vec<_> = cache
                .iter()
                .map(|(id, entry)| (entry.used, id.clone()))
                .collect();
            age.sort_unstable();
            for (_, id) in age.into_iter().take(cache.len() - CAPACITY) {
                cache.remove(&id);
            }
        }
    }
}

pub async fn many<T: Entity>(
    client: &Client,
    ids: &[String],
) -> anyhow::Result<Vec<Option<Arc<T>>>> {
    let store = T::store(client);
    let (ready, missing) = store.lookup(ids);
    if missing.is_empty() {
        return Ok(ready);
    }
    let _fetch = store.fetch.lock().await;
    let (ready, missing) = store.lookup(ids);
    if missing.is_empty() {
        return Ok(ready);
    }
    let batches: Vec<_> = missing.chunks(100).map(<[String]>::to_vec).collect();
    let replies: Vec<HashMap<String, Arc<T>>> = stream::iter(batches)
        .map(|batch| async move { fetch::<T>(client, &batch).await })
        .buffer_unordered(4)
        .try_collect()
        .await?;
    let received: HashMap<_, _> = replies.into_iter().flatten().collect();
    let ordered = ids
        .iter()
        .zip(ready)
        .map(|(id, cached)| cached.or_else(|| received.get(id).cloned()))
        .collect();
    store.insert(&missing, received);
    Ok(ordered)
}

pub async fn one<T: Entity>(client: &Client, id: &str) -> anyhow::Result<Arc<T>> {
    many::<T>(client, &[id.to_owned()])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| {
            request::failure(
                reqwest::StatusCode::NOT_FOUND,
                "Catalog entity is unavailable",
            )
        })
}
async fn fetch<T: Entity>(
    client: &Client,
    ids: &[String],
) -> anyhow::Result<HashMap<String, Arc<T>>> {
    let mut message = BatchedEntityRequest::new();
    for id in ids {
        let mut query = ExtensionQuery::new();
        query.extension_kind = EnumOrUnknown::new(T::KIND);
        let mut entity = EntityRequest::new();
        entity.entity_uri = id.clone();
        entity.query.push(query);
        message.entity_request.push(entity);
    }
    let reply = request::read(|| client.extended_metadata(&message))
        .await
        .map_err(request::upstream)?;
    let expected: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut result = HashMap::new();
    for group in reply.extended_metadata {
        if group.extension_kind.enum_value().ok() != Some(T::KIND) {
            continue;
        }
        for entry in group.extension_data {
            if !expected.contains(entry.entity_uri.as_str()) {
                continue;
            }
            let Some(data) = entry.extension_data.as_ref() else {
                continue;
            };
            let parsed = SpotifyUri::from_uri(&entry.entity_uri)
                .ok()
                .and_then(|uri| {
                    T::Message::parse_from_bytes(&data.value)
                        .ok()
                        .and_then(|msg| T::parse(&msg, &uri).ok())
                });
            if let Some(item) = parsed {
                result.insert(entry.entity_uri, Arc::new(item));
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_entries_are_cached_and_duplicates_keep_their_positions() {
        let store = Store::default();
        let requested = vec!["known".into(), "absent".into()];
        store.insert(&requested, HashMap::from([("known".into(), Arc::new(7))]));
        let (values, missing) = store.lookup(&["known".into(), "absent".into(), "known".into()]);
        assert!(missing.is_empty());
        assert_eq!(
            values
                .iter()
                .map(|v| v.as_deref().copied())
                .collect::<Vec<_>>(),
            [Some(7), None, Some(7)]
        );
    }
    #[test]
    fn expired_entries_are_refetched() {
        let store = Store::default();
        store.insert(
            &["key".into()],
            HashMap::from([("key".into(), Arc::new(1))]),
        );
        store.values.lock().unwrap().get_mut("key").unwrap().expires = Instant::now();
        assert_eq!(store.lookup(&["key".into()]).1, ["key"]);
    }
}
