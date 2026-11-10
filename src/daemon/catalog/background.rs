//! Bounded cache warming. Interactive misses always get the next request slot.
use super::*;
use std::{cell::Cell, future::Future};
use tokio::sync::SemaphorePermit;

tokio::task_local! {
    static REQUESTS_LEFT: Cell<usize>;
}

pub(super) fn is_background() -> bool {
    REQUESTS_LEFT.try_with(|_| ()).is_ok()
}

fn has_budget() -> bool {
    REQUESTS_LEFT
        .try_with(|left| left.get() > 0)
        .unwrap_or(true)
}

pub(super) async fn run<T>(future: impl Future<Output = T>) -> T {
    REQUESTS_LEFT.scope(Cell::new(usize::MAX), future).await
}

pub(super) struct ForegroundRequest<'a>(Option<&'a AtomicUsize>);

impl<'a> ForegroundRequest<'a> {
    pub(super) fn new(active: &'a AtomicUsize) -> Self {
        if is_background() {
            Self(None)
        } else {
            active.fetch_add(1, Ordering::Relaxed);
            Self(Some(active))
        }
    }
}

impl Drop for ForegroundRequest<'_> {
    fn drop(&mut self) {
        if let Some(active) = self.0 {
            active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub(super) enum WarmItem {
    Playlist(String, usize),
    Album(String),
    Artist(String),
}

pub struct WarmProgress {
    pub changed: bool,
    pub pending: bool,
}

impl Catalog {
    pub(super) async fn request_permit(&self) -> Result<SemaphorePermit<'_>> {
        if !is_background() {
            return Ok(self.limit.acquire().await?);
        }
        loop {
            ensure!(has_budget(), "background cache request budget exhausted");
            if self.foreground.load(Ordering::Relaxed) > 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            // Never hold all network permits while waiting for foreground work.
            let permit = match self.limit.try_acquire() {
                Ok(permit) if self.limit.available_permits() > 0 => permit,
                permit => {
                    // Always leave one slot for an interactive miss.
                    drop(permit);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
            };
            let delay = {
                let mut next = self.next_request.lock().await;
                let now = std::time::Instant::now();
                if self.foreground.load(Ordering::Relaxed) > 0 {
                    Duration::from_millis(25)
                } else if *next > now {
                    next.saturating_duration_since(now)
                } else {
                    *next = now + Duration::from_millis(500);
                    let _ = REQUESTS_LEFT.try_with(|left| left.set(left.get() - 1));
                    return Ok(permit);
                }
            };
            drop(permit);
            // Recheck priority after sleeping instead of reserving future slots.
            tokio::time::sleep(delay).await;
        }
    }

    /// Run once at daemon startup and periodically while it remains detached.
    /// Disk hits are free; each pass permits at most 128 network requests.
    pub async fn warm(self: &Arc<Self>) -> WarmProgress {
        let revision = self.content_revision.load(Ordering::Relaxed);
        let generation = self.generation.load(Ordering::Relaxed);
        let rules = self.playlist_settings.lock().unwrap().rules.clone();
        let mut pending = std::mem::take(&mut *self.warming.lock().await);
        REQUESTS_LEFT
            .scope(Cell::new(128), async {
                if pending.is_empty() {
                    let (playlists, albums, artists) =
                        tokio::join!(self.playlist_names(), self.saved_albums(), self.artists());
                    let mut albums = albums.unwrap_or_default();
                    albums.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                    let artists = artists.unwrap_or_default();
                    let playlists: Vec<_> = playlists.unwrap_or_default().into_values().collect();
                    pending.push_back(WarmItem::Playlist("me/tracks".into(), 0));
                    for index in 0..playlists.len().max(albums.len()).max(artists.len().min(12)) {
                        if let Some(id) = playlists.get(index) {
                            pending
                                .push_back(WarmItem::Playlist(format!("playlists/{id}/tracks"), 0));
                        }
                        if let Some(id) = albums.get(index).and_then(|album| album["id"].as_str()) {
                            pending.push_back(WarmItem::Album(id.to_owned()));
                        }
                        if let Some((_, name)) = artists.get(index).filter(|_| index < 12) {
                            pending.push_back(WarmItem::Artist(name.clone()));
                        }
                    }
                }
                while let Some(item) = pending.pop_front() {
                    if !has_budget()
                        || self
                            .cooldown
                            .lock()
                            .await
                            .is_some_and(|until| until > std::time::Instant::now())
                    {
                        pending.push_front(item);
                        break;
                    }
                    // Give each playlist one page per turn. A large collection
                    // cannot consume the entire startup budget before albums warm.
                    let result = match item {
                        WarmItem::Playlist(path, offset) => self
                            .prefetch_playlist_page(&path, offset)
                            .await
                            .map(|next| {
                                if let Some(next) = next {
                                    pending.push_back(WarmItem::Playlist(path, next));
                                }
                            }),
                        WarmItem::Album(id) => self.album_tracks(&id).await.map(|_| ()),
                        WarmItem::Artist(name) => {
                            self.artist_album_page(&name, 0).await.map(|_| ())
                        }
                    };
                    if let Err(error) = result {
                        log::debug!("background cache warming: {error}");
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
        let mut saved = self.warming.lock().await;
        if generation == self.generation.load(Ordering::Relaxed)
            && self.playlist_settings.lock().unwrap().rules == rules
        {
            *saved = pending;
        }
        WarmProgress {
            changed: self.content_revision.load(Ordering::Relaxed) != revision,
            pending: !saved.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::catalog::tests::{cache_page, playlist_track};
    use std::collections::VecDeque;

    #[tokio::test]
    async fn interactive_requests_do_not_inherit_background_delay() {
        let catalog = Catalog::new(true);
        *catalog.next_request.lock().await = std::time::Instant::now() + Duration::from_secs(60);
        drop(
            tokio::time::timeout(Duration::from_millis(100), catalog.request_permit())
                .await
                .unwrap()
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn warming_leaves_a_network_slot_for_interactive_work() {
        let catalog = Arc::new(Catalog::new(true));
        let held = catalog.limit.acquire_many(3).await.unwrap();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let background = Arc::clone(&catalog);
        let marker = Arc::clone(&started);
        let task = tokio::spawn(run(async move {
            let _permit = background.request_permit().await.unwrap();
            marker.store(true, Ordering::Relaxed);
        }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!started.load(Ordering::Relaxed));
        let foreground = tokio::time::timeout(Duration::from_millis(100), catalog.request_permit())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        drop((foreground, held));
        let _ = task.await;
    }

    #[tokio::test]
    async fn interactive_requests_overtake_waiting_background_work() {
        let catalog = Arc::new(Catalog::new(true));
        *catalog.next_request.lock().await = std::time::Instant::now() + Duration::from_millis(40);
        let mut waiting = Vec::new();
        for _ in 0..4 {
            let catalog = Arc::clone(&catalog);
            waiting.push(tokio::spawn(run(async move {
                drop(catalog.request_permit().await.unwrap());
            })));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        let foreground = ForegroundRequest::new(&catalog.foreground);
        drop(
            tokio::time::timeout(Duration::from_millis(300), catalog.request_permit())
                .await
                .expect("foreground must not wait behind prefetch")
                .unwrap(),
        );
        assert!(waiting.iter().all(|task| !task.is_finished()));
        // Cancellation must release priority as well as semaphore permits.
        drop(foreground);
        assert_eq!(catalog.foreground.load(Ordering::Relaxed), 0);
        for task in waiting {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(catalog.limit.available_permits(), 4);
    }

    #[tokio::test]
    async fn budget_caps_network_work_without_charging_cached_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::new(false);
        catalog.disk_root = dir.path().join("catalog");
        catalog.versions.get_mut().unwrap().clear();
        cache_page(&catalog, "me/tracks", 0, vec![], 0);
        let catalog = Arc::new(catalog);
        REQUESTS_LEFT
            .scope(Cell::new(1), async {
                drop(catalog.request_permit().await.unwrap());
                assert!(!has_budget());
                assert!(catalog.request_permit().await.is_err());
                assert!(
                    catalog
                        .get(
                            "me/tracks",
                            &[("limit", "100".into()), ("offset", "0".into())]
                        )
                        .await
                        .is_ok()
                );
            })
            .await;
        assert!(!is_background());
    }

    #[tokio::test]
    async fn warming_resumes_after_cooldown_and_skips_a_broken_collection() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::new(false);
        catalog.disk_root = dir.path().join("catalog");
        catalog.versions.get_mut().unwrap().clear();
        cache_page(
            &catalog,
            "playlists/mix/tracks",
            0,
            vec![json!({"item":playlist_track(0)})],
            1,
        );
        // A malformed collection must not cancel the rest of the warm plan.
        let key = catalog.cache_key("broken", &[("limit", "100".into()), ("offset", "0".into())]);
        let entry = Cached {
            fetched: now(),
            value: json!({"total":1}),
            used: Default::default(),
        };
        crate::paths::write_private_file_atomic(
            &catalog.disk(&key),
            &serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        *catalog.warming.get_mut() = VecDeque::from([
            WarmItem::Playlist("broken".into(), 0),
            WarmItem::Playlist("playlists/mix/tracks".into(), 0),
        ]);
        *catalog.cooldown.get_mut() = Some(std::time::Instant::now() + Duration::from_secs(30));
        let catalog = Arc::new(catalog);
        assert!(catalog.warm().await.pending);
        assert_eq!(catalog.warming.lock().await.len(), 2);
        *catalog.cooldown.lock().await = None;
        assert!(!catalog.warm().await.pending);
        assert!(
            catalog
                .playlist_views
                .lock()
                .await
                .contains_key("playlists/mix/tracks")
        );
    }

    #[tokio::test]
    async fn warming_prepares_albums_playlists_and_artist_indexes_from_persistent_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::new(false);
        catalog.disk_root = dir.path().join("catalog");
        catalog.versions.get_mut().unwrap().clear();
        cache_page(
            &catalog,
            "me/playlists",
            0,
            vec![json!({"name":"Mix","id":"mix"})],
            1,
        );
        cache_page(
            &catalog,
            "playlists/mix/tracks",
            0,
            vec![json!({"item":playlist_track(0)})],
            1,
        );
        cache_page(&catalog, "me/tracks", 0, vec![], 0);
        let mut album = playlist_track(1)["album"].clone();
        album["rspc_track_uris"] = json!(["spotify:track:fixture1"]);
        cache_page(
            &catalog,
            "me/albums",
            0,
            vec![json!({"album":album.clone()})],
            1,
        );
        cache_page(&catalog, "artists/artist1/albums", 0, vec![album], 1);
        for (path, query, value) in [
            (
                "me/following",
                vec![("limit", "100".into())],
                json!({"artists":{"items":[{"id":"artist1","name":"Playlist Artist"}],"next":null}}),
            ),
            (
                "tracks",
                vec![("ids", "fixture1".into())],
                json!({"tracks":[playlist_track(1)]}),
            ),
        ] {
            let entry = Cached {
                fetched: now(),
                value,
                used: Default::default(),
            };
            crate::paths::write_private_file_atomic(
                &catalog.disk(&catalog.cache_key(path, &query)),
                &serde_json::to_vec(&entry).unwrap(),
            )
            .unwrap();
        }
        let catalog = Arc::new(catalog);
        tokio::time::timeout(Duration::from_secs(2), catalog.warm())
            .await
            .unwrap();
        std::fs::remove_dir_all(&catalog.disk_root).unwrap();
        assert!(
            catalog
                .tracks
                .lock()
                .await
                .contains_key("spotify:track:fixture1")
        );
        assert!(
            catalog
                .playlist_views
                .lock()
                .await
                .contains_key("playlists/mix/tracks")
        );
        assert!(
            catalog
                .artist_pages
                .lock()
                .await
                .contains_key(&("Playlist Artist".into(), 0))
        );
        assert_eq!(catalog.album_tracks("album1").await.unwrap().len(), 1);
        assert_eq!(catalog.playlist_tracks("Mix").await.unwrap().len(), 1);
    }
}
