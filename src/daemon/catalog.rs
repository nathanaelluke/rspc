mod artist_albums;
mod artist_views;
mod background;
mod browse;
mod people;
mod playlist_views;
mod playlists;
mod selection;

use super::filter;
use crate::model::{ArtistRef, TrackInfo};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};

const CACHE_CAPACITY: usize = 2048;

fn insert_named(items: &mut BTreeMap<String, String>, name: String, id: String) {
    let base = if items.contains_key(&name) {
        format!("{name} [{id}]")
    } else {
        name
    };
    let mut label = base.clone();
    let mut suffix = 2;
    while items.contains_key(&label) {
        label = format!("{base} ({suffix})");
        suffix += 1;
    }
    items.insert(label, id);
}
#[derive(Serialize, Deserialize)]
struct Cached {
    fetched: u64,
    value: Value,
    #[serde(skip)]
    used: AtomicU64,
}
static CACHE_ACCESS: AtomicU64 = AtomicU64::new(1);

fn cache_insert(cache: &mut HashMap<String, Arc<Cached>>, key: String, entry: Arc<Cached>) {
    entry.used.store(
        CACHE_ACCESS.fetch_add(1, Ordering::Relaxed),
        Ordering::Relaxed,
    );
    if cache.len() >= CACHE_CAPACITY && !cache.contains_key(&key) {
        // Evict in a small batch so a large library does not scan the cache on every insertion.
        let mut oldest: Vec<_> = cache
            .iter()
            .map(|(key, entry)| (key.clone(), entry.used.load(Ordering::Relaxed)))
            .collect();
        oldest.sort_unstable_by_key(|(_, used)| *used);
        for (key, _) in oldest.into_iter().take(CACHE_CAPACITY / 8) {
            cache.remove(&key);
        }
    }
    cache.insert(key, entry);
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct Catalog {
    pub playlist_settings: std::sync::Mutex<crate::config::PlaylistSettings>,
    pub local: Arc<super::local::Library>,
    pub tracks: Mutex<HashMap<String, TrackInfo>>,
    albums: Mutex<HashMap<String, Value>>,
    playlists: Mutex<BTreeMap<String, String>>,
    cache: Mutex<HashMap<String, Arc<Cached>>>,
    gates: Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    disk_writes: Mutex<()>,
    limit: Semaphore,
    foreground: AtomicUsize,
    content_revision: AtomicU64,
    warming: Mutex<std::collections::VecDeque<background::WarmItem>>,
    generation: AtomicU64,
    playlist_view_epoch: AtomicU64,
    playlist_views: Mutex<HashMap<String, playlist_views::PlaylistView>>,
    pub demo: bool,
    disk_root: std::path::PathBuf,
    tag_cache: Mutex<HashMap<String, (std::time::Instant, Vec<String>)>>,
    playlist_fetched: Mutex<Option<std::time::Instant>>,
    artist_views: Mutex<HashMap<String, artist_views::ArtistView>>,
    artist_pages: Mutex<HashMap<(String, usize), artist_albums::ArtistPageView>>,
    artist_view_epoch: AtomicU64,
    people_cache: Mutex<Option<(std::time::Instant, BTreeMap<String, String>)>>,
    next_request: Mutex<std::time::Instant>,
    cooldown: Mutex<Option<std::time::Instant>>,
    versions: std::sync::Mutex<HashMap<String, u64>>,
    demo_playlists: Mutex<BTreeMap<String, Vec<TrackInfo>>>,
}
impl Catalog {
    pub fn new(demo: bool) -> Self {
        Self {
            playlist_settings: std::sync::Mutex::new(Default::default()),
            local: Arc::new(super::local::Library::new()),
            demo,
            disk_root: crate::paths::cache().join("catalog-v2"),
            tag_cache: Mutex::new(HashMap::new()),
            playlist_fetched: Mutex::new(None),
            artist_views: Mutex::new(HashMap::new()),
            artist_pages: Mutex::new(HashMap::new()),
            artist_view_epoch: AtomicU64::new(0),
            people_cache: Mutex::new(None),
            next_request: Mutex::new(std::time::Instant::now()),
            cooldown: Mutex::new(None),
            versions: std::sync::Mutex::new(if demo {
                HashMap::new()
            } else {
                std::fs::read(crate::paths::cache().join("catalog-versions.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default()
            }),
            demo_playlists: Mutex::new(BTreeMap::from([("Demo Mix".into(), demo_tracks())])),
            tracks: Mutex::new(HashMap::new()),
            albums: Mutex::new(HashMap::new()),
            playlists: Mutex::new(BTreeMap::new()),
            cache: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            disk_writes: Mutex::new(()),
            limit: Semaphore::new(4),
            foreground: AtomicUsize::new(0),
            content_revision: AtomicU64::new(0),
            warming: Mutex::new(Default::default()),
            generation: AtomicU64::new(0),
            playlist_view_epoch: AtomicU64::new(0),
            playlist_views: Mutex::new(HashMap::new()),
        }
    }
    fn disk(&self, key: &str) -> std::path::PathBuf {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hash);
        self.disk_root
            .clone()
            .join(format!("{:016x}.json", hash.finish()))
    }
    pub async fn invalidate(self: &Arc<Self>) {
        self.local.invalidate().await;
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.warming.lock().await.clear();
        self.playlist_views.lock().await.clear();
        self.artist_view_epoch.fetch_add(1, Ordering::Relaxed);
        self.artist_views.lock().await.clear();
        self.artist_pages.lock().await.clear();
        self.cache.lock().await.clear();
        self.tag_cache.lock().await.clear();
        self.tracks.lock().await.clear();
        self.albums.lock().await.clear();
        *self.people_cache.lock().await = None;
        *self.playlist_fetched.lock().await = None;
        let _writes = self.disk_writes.lock().await;
        let _ = tokio::fs::remove_dir_all(&self.disk_root).await;
    }

    async fn request_gate(&self, key: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().await;
        if gates.len() >= CACHE_CAPACITY {
            gates.retain(|_, gate| gate.strong_count() > 0);
        }
        if let Some(gate) = gates.get(key).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(key.to_owned(), Arc::downgrade(&gate));
        gate
    }
    async fn fetch(
        self: &Arc<Self>,
        key: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        let generation = self.generation.load(Ordering::Relaxed);
        let _foreground = background::ForegroundRequest::new(&self.foreground);
        if let Some(until) = self
            .cooldown
            .lock()
            .await
            .filter(|at| *at > std::time::Instant::now())
        {
            bail!(
                "Spotify is cooling down for {} seconds; cached music remains available",
                until
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs()
                    + 1
            );
        }
        let _permit = self.request_permit().await?;
        // Another in-flight request may have received a limit response while
        // this one was waiting for its scheduled slot.
        ensure!(
            self.cooldown
                .lock()
                .await
                .is_none_or(|until| until <= std::time::Instant::now()),
            "Spotify is cooling down; cached music remains available"
        );
        let rules = self.playlist_settings.lock().unwrap().rules.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            crate::spotify::get(path, query, &rules),
        )
        .await
        .context("Spotify catalog request timed out")?;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if error
                    .downcast_ref::<crate::spotify::ApiError>()
                    .is_some_and(|error| error.status == reqwest::StatusCode::TOO_MANY_REQUESTS)
                {
                    *self.cooldown.lock().await =
                        Some(std::time::Instant::now() + Duration::from_secs(30));
                }
                return Err(error);
            }
        };
        let entry = Arc::new(Cached {
            used: Default::default(),
            fetched: now(),
            value: value.clone(),
        });
        let mut cache = self.cache.lock().await;
        if generation == self.generation.load(Ordering::Relaxed) {
            if cache
                .get(key)
                .is_none_or(|previous| previous.value != value)
            {
                self.content_revision.fetch_add(1, Ordering::Relaxed);
            }
            cache_insert(&mut cache, key.to_owned(), entry.clone());
            if matches!(path, "me/albums" | "me/tracks" | "me/following") {
                self.tag_cache.lock().await.clear();
            }
            self.invalidate_artist_view(path).await;
            if path == "me/tracks" || path.starts_with("playlists/") && path.ends_with("/tracks") {
                self.invalidate_playlist_view(path).await;
            }
            drop(cache);
            let catalog = Arc::clone(self);
            let key = key.to_owned();
            tokio::spawn(async move {
                if let Err(error) = catalog.persist_entry(&key, generation, entry).await {
                    log::debug!("could not persist Spotify catalog cache: {error}");
                }
            });
        }
        Ok(value)
    }

    async fn persist_entry(&self, key: &str, generation: u64, entry: Arc<Cached>) -> Result<()> {
        let _writes = self.disk_writes.lock().await;
        if generation != self.generation.load(Ordering::Relaxed) {
            return Ok(());
        }
        let file = self.disk(key);
        tokio::task::spawn_blocking(move || {
            crate::paths::write_private_file_atomic(&file, &serde_json::to_vec(entry.as_ref())?)
        })
        .await??;
        Ok(())
    }
    fn cache_key(&self, path: &str, query: &[(&str, String)]) -> String {
        let version = self
            .versions
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0);
        let mut key = if version == 0 {
            format!("{path}|{query:?}")
        } else {
            format!("{path}|{query:?}|v{version}")
        };
        if path == "me/playlists" {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            self.playlist_settings.lock().unwrap().rules.hash(&mut hash);
            key.push_str(&format!("|exclusions-v1:{:016x}", hash.finish()));
        }
        key
    }
    pub async fn get(self: &Arc<Self>, path: &str, query: &[(&str, String)]) -> Result<Value> {
        ensure!(!self.demo, "demo backend has no Spotify connection");
        let generation = self.generation.load(Ordering::Relaxed);
        let key = self.cache_key(path, query);
        let cached = self.cache.lock().await.get(&key).cloned();
        let cached = match cached {
            Some(v) => Some(v),
            None => tokio::fs::read(self.disk(&key))
                .await
                .ok()
                .and_then(|b| serde_json::from_slice::<Cached>(&b).ok().map(Arc::new)),
        };
        if let Some(cached) = cached {
            let age = now().saturating_sub(cached.fetched);
            let value = cached.value.clone();
            {
                let mut memory = self.cache.lock().await;
                // A disk read begun before invalidation may finish after the
                // memory cache was cleared. Do not promote that old page.
                if generation != self.generation.load(Ordering::Relaxed) {
                    return Ok(value);
                }
                cache_insert(&mut memory, key.clone(), cached);
            }
            if age > 300 {
                let gate = self.request_gate(&key).await;
                if background::is_background() {
                    // Keep warming within its request budget. Detached refresh
                    // tasks would otherwise turn one stale library into a burst.
                    if let Ok(_guard) = gate.try_lock()
                        && let Err(error) = self.fetch(&key, path, query).await
                    {
                        log::debug!("background catalog refresh: {error}");
                    }
                    return Ok(self
                        .cache
                        .lock()
                        .await
                        .get(&key)
                        .map(|entry| entry.value.clone())
                        .unwrap_or(value));
                }
                let this = Arc::clone(self);
                let path = path.to_owned();
                let query: Vec<_> = query
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect();
                tokio::spawn(background::run(async move {
                    if let Ok(_guard) = gate.try_lock() {
                        if this
                            .cache
                            .lock()
                            .await
                            .get(&key)
                            .is_some_and(|entry| now().saturating_sub(entry.fetched) <= 300)
                        {
                            return;
                        }
                        let q: Vec<_> =
                            query.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                        if let Err(e) = this.fetch(&key, &path, &q).await {
                            log::debug!("background catalog refresh failed: {e}");
                        }
                    }
                }));
            }
            return Ok(value);
        }
        let gate = self.request_gate(&key).await;
        let _guard = gate.lock().await;
        if let Some(cached) = self.cache.lock().await.get(&key) {
            return Ok(cached.value.clone());
        }
        self.fetch(&key, path, query).await
    }
    async fn pages(self: &Arc<Self>, path: &str) -> Result<Vec<Value>> {
        use futures::{StreamExt, TryStreamExt, stream};
        let first = self
            .get(path, &[("limit", "100".into()), ("offset", "0".into())])
            .await?;
        let items = first["items"]
            .as_array()
            .context("Spotify response has no items array")?
            .clone();
        let total = first["total"].as_u64().map(|n| n as usize);
        if let Some(total) = total {
            ensure!(total <= 100_000, "library exceeds pagination limit");
            let pages: Vec<(usize, Vec<Value>)> = stream::iter((100..total).step_by(100))
                .map(|offset| async move {
                    let page = self
                        .get(
                            path,
                            &[("limit", "100".into()), ("offset", offset.to_string())],
                        )
                        .await?;
                    Ok::<_, anyhow::Error>((
                        offset,
                        page["items"]
                            .as_array()
                            .context("missing page items")?
                            .clone(),
                    ))
                })
                .buffer_unordered(4)
                .try_collect()
                .await?;
            let mut pages = pages;
            pages.sort_by_key(|(offset, _)| *offset);
            return Ok(items
                .into_iter()
                .chain(pages.into_iter().flat_map(|(_, p)| p))
                .collect());
        }
        let mut items = items;
        let mut more = first["next"].is_string();
        while more {
            let offset = items.len();
            ensure!(offset < 100_000, "pagination limit exceeded");
            let page = self
                .get(
                    path,
                    &[("limit", "100".into()), ("offset", offset.to_string())],
                )
                .await?;
            let next = page["items"].as_array().context("missing page items")?;
            if next.is_empty() {
                break;
            }
            items.extend(next.iter().cloned());
            more = page["next"].is_string();
        }
        Ok(items)
    }
    pub async fn remember(self: &Arc<Self>, tracks: Vec<TrackInfo>) -> Vec<TrackInfo> {
        let mut cache = self.tracks.lock().await;
        for track in &tracks {
            if let Some(uri) = &track.uri
                && cache.get(uri) != Some(track)
            {
                cache.insert(uri.clone(), track.clone());
            }
        }
        tracks
    }
    pub async fn liked(self: &Arc<Self>) -> Result<Vec<TrackInfo>> {
        if self.demo {
            return Ok(self.remember(demo_tracks()).await);
        }
        Ok(self.playlist_view("me/tracks").await?.as_ref().clone())
    }
    pub async fn recent(self: &Arc<Self>) -> Result<Vec<TrackInfo>> {
        if self.demo {
            return self.liked().await;
        }
        let tracks = crate::spotify::load_recent()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v["track"].clone()).ok())
            .collect();
        Ok(self.remember(tracks).await)
    }
    pub async fn saved_albums(self: &Arc<Self>) -> Result<Vec<Value>> {
        if self.demo {
            return Ok(demo_albums());
        }
        let albums: Vec<Value> = self
            .pages("me/albums")
            .await?
            .into_iter()
            .map(|v| v["album"].clone())
            .collect();
        let mut cache = self.albums.lock().await;
        for album in &albums {
            if let Some(id) = album["id"].as_str() {
                cache.insert(id.into(), album.clone());
            }
        }
        Ok(albums)
    }
    pub async fn artists(self: &Arc<Self>) -> Result<Vec<(String, String)>> {
        if self.demo {
            return Ok(vec![("demo-artist".into(), "The Example Ensemble".into())]);
        }
        let mut result = BTreeMap::new();
        let mut after: Option<String> = None;
        loop {
            let mut query = vec![("limit", "100".to_owned())];
            if let Some(cursor) = &after {
                query.push(("after", cursor.clone()));
            }
            let page = self.get("me/following", &query).await?;
            let artists = &page["artists"];
            let items = artists["items"]
                .as_array()
                .context("missing saved artists")?;
            for artist in items {
                if let (Some(id), Some(name)) = (artist["id"].as_str(), artist["name"].as_str()) {
                    result.insert(name.to_owned(), id.to_owned());
                }
            }
            if items.is_empty() || !artists["next"].is_string() {
                break;
            }
            let next = artists["cursors"]["after"]
                .as_str()
                .context("missing artist cursor")?
                .to_owned();
            ensure!(
                after.as_ref() != Some(&next),
                "artist pagination did not advance"
            );
            ensure!(
                result.len() < 100_000,
                "saved artist pagination limit exceeded"
            );
            after = Some(next);
        }
        Ok(result.into_iter().map(|(name, id)| (id, name)).collect())
    }
    pub async fn artist_albums(self: &Arc<Self>, name: &str) -> Result<Vec<Value>> {
        if self.demo {
            return Ok(demo_albums());
        }
        Ok(self
            .saved_albums()
            .await?
            .into_iter()
            .filter(|album| {
                album["artists"].as_array().into_iter().flatten().any(|a| {
                    a["name"]
                        .as_str()
                        .is_some_and(|artist| artist.eq_ignore_ascii_case(name))
                })
            })
            .collect())
    }
    pub async fn album_tracks(self: &Arc<Self>, id: &str) -> Result<Vec<TrackInfo>> {
        if self.demo {
            return self.liked().await;
        }
        let cached = self
            .albums
            .lock()
            .await
            .get(id)
            .filter(|a| a["rspc_partial"] != true)
            .cloned();
        let album = match cached {
            Some(a) => a,
            None => self.get(&format!("albums/{id}"), &[]).await?,
        };
        self.album_tracks_many(&[album]).await
    }
    async fn album_tracks_many(self: &Arc<Self>, albums: &[Value]) -> Result<Vec<TrackInfo>> {
        let uris: Vec<String> = albums
            .iter()
            .flat_map(|a| {
                a["rspc_track_uris"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
            })
            .collect();
        use futures::{StreamExt, TryStreamExt, stream};
        let mut known: HashMap<String, TrackInfo> = {
            let cache = self.tracks.lock().await;
            uris.iter()
                .filter_map(|uri| cache.get(uri).cloned().map(|track| (uri.clone(), track)))
                .collect()
        };
        let mut requested = std::collections::HashSet::new();
        let missing: Vec<_> = uris
            .iter()
            .filter(|uri| !known.contains_key(*uri) && requested.insert(uri.as_str()))
            .cloned()
            .collect();
        let chunks: Vec<Vec<String>> = missing.chunks(100).map(|c| c.to_vec()).collect();
        let pages: Vec<(usize, Vec<TrackInfo>)> = stream::iter(chunks.into_iter().enumerate())
            .map(|(i, chunk)| async move {
                let ids = chunk
                    .iter()
                    .filter_map(|u| u.strip_prefix("spotify:track:"))
                    .collect::<Vec<_>>()
                    .join(",");
                let page = self.get("tracks", &[("ids", ids)]).await?;
                let tracks = page["tracks"]
                    .as_array()
                    .context("missing tracks")?
                    .iter()
                    .filter_map(track_from_json)
                    .collect();
                Ok::<_, anyhow::Error>((i, tracks))
            })
            .buffer_unordered(4)
            .try_collect()
            .await?;
        let mut pages = pages;
        pages.sort_by_key(|(i, _)| *i);
        let fetched = self
            .remember(pages.into_iter().flat_map(|(_, t)| t).collect())
            .await;
        for track in fetched {
            if let Some(uri) = &track.uri {
                known.insert(uri.clone(), track);
            }
        }
        Ok(uris
            .iter()
            .filter_map(|uri| known.get(uri).cloned())
            .collect())
    }
    pub async fn resolve(self: &Arc<Self>, uri: &str) -> Result<Vec<TrackInfo>> {
        if let Some((kind, name)) = selection::library_target(uri)? {
            let tag = if kind == "album" { "Album" } else { "Artist" };
            return self
                .find(&format!("({tag} == '{}')", escape(&name)), false)
                .await;
        }
        if uri.starts_with("rspc://artist-album/") {
            return self.artist_album_tracks(uri).await;
        }
        if uri.starts_with("People/playlist/") {
            return Ok(self.browse_people(uri).await?.1);
        }
        if uri == super::local::ROOT {
            return Ok(self
                .remember(self.local.all().await?.as_ref().clone())
                .await);
        }
        if let Some(path) = uri.strip_prefix("Local Files/") {
            return Ok(self
                .remember(self.local.browse(path, true).await?.tracks)
                .await);
        }
        if uri.starts_with(super::local::PLAYLISTS) {
            return self.playlist_tracks(uri).await;
        }
        if uri.starts_with("mpd:")
            || (uri.starts_with('/') && uri != "/")
            || uri.starts_with("file://")
            || uri.starts_with("http://")
            || uri.starts_with("https://")
        {
            if let Some(track) = self.tracks.lock().await.get(uri).cloned() {
                return Ok(vec![track]);
            }
            return Ok(self.remember(self.local.resolve(uri).await?).await);
        }
        if uri.is_empty() || uri == "/" || uri == "Liked Songs" {
            return self.liked().await;
        }
        if uri == "Recently Played" {
            return self.recent().await;
        }
        if let Some(name) = uri.strip_prefix("Playlists/") {
            return self.playlist_tracks(name).await;
        }
        if let Some(name) = uri.strip_prefix("Albums/") {
            return self
                .find(&format!("(Album == '{}')", escape(name)), false)
                .await;
        }
        if let Some(name) = uri.strip_prefix("Artists/") {
            return self
                .find(&format!("(Artist == '{}')", escape(name)), false)
                .await;
        }
        if let Some(id) = uri.strip_prefix("spotify:album:") {
            return self.album_tracks(id).await;
        }
        if let Some(track) = self.tracks.lock().await.get(uri).cloned() {
            return Ok(vec![track]);
        }
        if self.demo {
            self.liked().await?;
            return self
                .tracks
                .lock()
                .await
                .get(uri)
                .cloned()
                .map(|t| vec![t])
                .context("demo track not found");
        }
        if let Some(id) = uri.strip_prefix("spotify:track:") {
            ensure!(
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
                "invalid Spotify track URI"
            );
            let item = self.get(&format!("tracks/{id}"), &[]).await?;
            return Ok(self
                .remember(vec![
                    track_from_json(&item).context("invalid Spotify track")?,
                ])
                .await);
        }
        bail!("unsupported Spotify path: {uri}")
    }
    pub async fn find(self: &Arc<Self>, query: &str, search: bool) -> Result<Vec<TrackInfo>> {
        if self.demo {
            return self.spotify_find(query, search).await;
        }
        let filters = filter::parse(query)?;
        if let Some(uri) = filters.iter().find(|f| {
            matches!(f.tag.as_str(), "file" | "filename")
                && f.op == "=="
                && f.value.starts_with("mpd:")
        }) {
            let cached = self.tracks.lock().await.get(&uri.value).cloned();
            let tracks = match cached {
                Some(track) => vec![track],
                None => self.local.resolve(&uri.value).await?,
            };
            return tracks
                .into_iter()
                .filter_map(|t| match filter::matches(&t, &filters, search) {
                    Ok(true) => Some(Ok(t)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect();
        }
        let (spotify, local) = tokio::join!(self.spotify_find(query, search), self.local.all());
        let local_available = local.is_ok();
        let mut local_matches = Vec::new();
        if let Ok(local) = local {
            for track in local.iter() {
                if filter::matches(track, &filters, search)? {
                    local_matches.push(track.clone());
                }
            }
        }
        let mut tracks = match spotify {
            Ok(t) => t,
            Err(e) if !local_available => return Err(e),
            Err(_) => Vec::new(),
        };
        tracks.extend(local_matches);
        Ok(self.remember(tracks).await)
    }
    async fn spotify_find(self: &Arc<Self>, query: &str, search: bool) -> Result<Vec<TrackInfo>> {
        let filters = filter::parse(query)?;
        let tracks = if self.demo {
            self.liked().await?
        } else if search {
            let text = filters
                .iter()
                .map(|f| match f.tag.as_str() {
                    "artist" | "albumartist" => format!("artist:{}", f.value),
                    "album" => format!("album:{}", f.value),
                    "title" => format!("track:{}", f.value),
                    _ => f.value.clone(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            let page = self
                .get(
                    "search",
                    &[
                        ("q", text),
                        ("type", "track".into()),
                        ("limit", "50".into()),
                    ],
                )
                .await?;
            self.remember(
                page["tracks"]["items"]
                    .as_array()
                    .context("missing search tracks")?
                    .iter()
                    .filter_map(track_from_json)
                    .collect(),
            )
            .await
        } else {
            let artist = filters
                .iter()
                .find(|f| matches!(f.tag.as_str(), "artist" | "albumartist") && f.op == "==")
                .map(|f| f.value.as_str());
            let album = filters
                .iter()
                .find(|f| f.tag == "album" && f.op == "==")
                .map(|f| f.value.as_str());
            if let Some(uri) = filters
                .iter()
                .find(|f| matches!(f.tag.as_str(), "file" | "filename") && f.op == "==")
            {
                // Only literal track URIs reach this arm; no recursive filter resolution.
                if let Some(track) = self.tracks.lock().await.get(&uri.value).cloned() {
                    vec![track]
                } else if let Some(id) = uri.value.strip_prefix("spotify:track:") {
                    vec![
                        track_from_json(&self.get(&format!("tracks/{id}"), &[]).await?)
                            .context("track not found")?,
                    ]
                } else {
                    Vec::new()
                }
            } else if artist.is_some() || album.is_some() {
                let album_artist = filters
                    .iter()
                    .any(|f| f.tag == "albumartist" && f.op == "==");
                if let Some(name) = artist.filter(|_| !album_artist) {
                    self.artist_tracks(name).await?.as_ref().clone()
                } else {
                    let albums = if let Some(artist) = artist {
                        self.artist_albums(artist).await?
                    } else {
                        self.saved_albums().await?
                    };
                    let albums: Vec<_> = albums
                        .into_iter()
                        .filter(|a| album.is_none_or(|name| a["name"].as_str() == Some(name)))
                        .collect();
                    self.album_tracks_many(&albums).await?
                }
            } else {
                self.liked().await?
            }
        };
        let mut selected = Vec::new();
        for track in tracks {
            if filter::matches(&track, &filters, search)? {
                selected.push(track);
            }
        }
        Ok(self.remember(selected).await)
    }
    pub async fn tags(self: &Arc<Self>, tag: &str, query: &str) -> Result<Vec<String>> {
        if self.demo {
            return self.spotify_tags(tag, query).await;
        }
        let filters = filter::parse(query)?;
        let (spotify, local) = tokio::join!(self.spotify_tags(tag, query), self.local.all());
        let local_available = local.is_ok();
        let mut values = std::collections::BTreeSet::new();
        if let Ok(local) = local {
            for track in local.iter() {
                if filter::matches(track, &filters, false)? {
                    values.extend(filter::values(track, tag)?);
                }
            }
        }
        match spotify {
            Ok(v) => values.extend(v),
            Err(e) if !local_available => return Err(e),
            Err(_) => {}
        }
        Ok(values.into_iter().collect())
    }
    async fn spotify_tags(self: &Arc<Self>, tag: &str, query: &str) -> Result<Vec<String>> {
        let cache_key = format!("{tag}|{query}");
        if let Some((at, values)) = self.tag_cache.lock().await.get(&cache_key)
            && at.elapsed() < Duration::from_secs(30)
        {
            return Ok(values.clone());
        }
        let filters = filter::parse(query)?;
        let mut values = std::collections::BTreeSet::new();
        match tag.to_lowercase().as_str() {
            "artist" if filters.is_empty() => {
                for (_, name) in self.artists().await? {
                    values.insert(name);
                }
            }
            "albumartist" if filters.is_empty() => {
                for album in self.saved_albums().await? {
                    values.extend(
                        album["artists"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|a| a["name"].as_str().map(str::to_owned)),
                    );
                }
            }
            "album"
                if filters.is_empty()
                    || filters.len() == 1
                        && filters[0].tag == "albumartist"
                        && filters[0].op == "==" =>
            {
                let albums = if let Some(artist) = filters.first() {
                    self.artist_albums(&artist.value).await?
                } else {
                    self.saved_albums().await?
                };
                for album in albums {
                    if let Some(name) = album["name"].as_str() {
                        values.insert(name.to_owned());
                    }
                }
            }
            _ => {
                for track in self.find(query, false).await? {
                    values.extend(filter::values(&track, tag)?);
                }
            }
        }
        let values: Vec<String> = values.into_iter().collect();
        let mut cache = self.tag_cache.lock().await;
        if cache.len() > 128 {
            cache.clear();
        }
        cache.insert(cache_key, (std::time::Instant::now(), values.clone()));
        Ok(values)
    }
}
pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
pub fn track_from_json(v: &Value) -> Option<TrackInfo> {
    let uri = v["uri"].as_str()?.to_string();
    if !uri.starts_with("spotify:track:") {
        return None;
    }
    let artists: Vec<ArtistRef> = v["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| ArtistRef {
            id: a["id"].as_str().map(String::from),
            name: a["name"].as_str().unwrap_or("Unknown").into(),
        })
        .collect();
    Some(TrackInfo {
        uri: Some(uri),
        name: v["name"].as_str()?.into(),
        artists: artists.iter().map(|a| a.name.clone()).collect(),
        album_artists: v["album"]["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str().map(str::to_owned))
            .collect(),
        artist_refs: artists,
        album: v["album"]["name"].as_str().unwrap_or_default().into(),
        album_id: v["album"]["id"].as_str().map(String::from),
        image_url: v["album"]["images"][0]["url"].as_str().map(String::from),
        release_date: v["album"]["release_date"].as_str().map(String::from),
        duration_ms: v["duration_ms"].as_u64().unwrap_or(0),
        id: v["id"].as_str().map(String::from),
        is_playable: v["is_playable"].as_bool().unwrap_or(true),
        is_local: false,
        track_number: v["track_number"].as_u64().unwrap_or(0) as u32,
        explicit: v["explicit"].as_bool().unwrap_or(false),
    })
}
pub fn demo_tracks() -> Vec<TrackInfo> {
    ["Morning Light", "Second Window", "After Hours"].iter().enumerate().map(|(i, name)| track_from_json(&json!({
        "id":format!("demo{}",i+1), "uri":format!("spotify:track:demo{}",i+1), "name":name, "duration_ms":180000 + i as u64 * 30000, "track_number":i+1,
        "artists":[{"id":"demo-artist","name":"The Example Ensemble"}], "album":{"id":"demo-album","name":"Example Sessions","release_date":"2026-01-01"}
    })).unwrap()).collect()
}
fn demo_albums() -> Vec<Value> {
    vec![
        json!({"id":"demo-album","name":"Example Sessions","release_date":"2026-01-01","artists":[{"id":"demo-artist","name":"The Example Ensemble"}]}),
    ]
}

#[cfg(test)]
mod tests;
