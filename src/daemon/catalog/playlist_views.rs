//! Assembled playlists sit above the persistent API-page cache.
use super::*;
use std::time::Instant;

const MAX_VIEWS: usize = 64;
const MAX_TRACKS: usize = 20_000;
const FRESH_FOR: Duration = Duration::from_secs(300);

pub(super) struct PlaylistView {
    tracks: Arc<Vec<TrackInfo>>,
    fetched: Instant,
    used: Instant,
}

impl Catalog {
    pub(super) async fn invalidate_playlist_view(&self, path: &str) {
        self.playlist_view_epoch.fetch_add(1, Ordering::Relaxed);
        self.playlist_views.lock().await.remove(path);
    }

    pub(super) async fn playlist_view(self: &Arc<Self>, path: &str) -> Result<Arc<Vec<TrackInfo>>> {
        let cached = {
            let mut views = self.playlist_views.lock().await;
            views.get_mut(path).map(|view| {
                view.used = Instant::now();
                (Arc::clone(&view.tracks), view.fetched.elapsed() < FRESH_FOR)
            })
        };
        if let Some((tracks, fresh)) = cached {
            if !fresh {
                if background::is_background() {
                    return self.fill_playlist_view(path).await.or(Ok(tracks));
                }
                let catalog = Arc::clone(self);
                let path = path.to_owned();
                tokio::spawn(background::run(async move {
                    if let Err(error) = catalog.fill_playlist_view(&path).await {
                        log::debug!("playlist cache refresh: {error}");
                    }
                }));
            }
            return Ok(tracks);
        }
        self.fill_playlist_view(path).await
    }

    async fn fill_playlist_view(self: &Arc<Self>, path: &str) -> Result<Arc<Vec<TrackInfo>>> {
        let gate = self.request_gate(&format!("view:{path}")).await;
        let _guard = gate.lock().await;
        if let Some(view) = self.playlist_views.lock().await.get(path)
            && view.fetched.elapsed() < FRESH_FOR
        {
            return Ok(Arc::clone(&view.tracks));
        }
        let epoch = self.playlist_view_epoch.load(Ordering::Relaxed);
        let generation = self.generation.load(Ordering::Relaxed);
        let items = self.pages(path).await?;
        let tracks = tokio::task::spawn_blocking(move || {
            items
                .iter()
                .filter_map(track_from_playlist_item)
                .collect::<Vec<_>>()
        })
        .await?;
        let tracks = Arc::new(self.remember(tracks).await);
        let mut views = self.playlist_views.lock().await;
        if tracks.len() <= MAX_TRACKS
            && epoch == self.playlist_view_epoch.load(Ordering::Relaxed)
            && generation == self.generation.load(Ordering::Relaxed)
        {
            views.remove(path);
            while views.len() >= MAX_VIEWS
                || views.values().map(|view| view.tracks.len()).sum::<usize>() + tracks.len()
                    > MAX_TRACKS
            {
                let oldest = views
                    .iter()
                    .min_by_key(|(_, view)| view.used)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    views.remove(&oldest);
                } else {
                    break;
                }
            }
            views.insert(
                path.to_owned(),
                PlaylistView {
                    tracks: Arc::clone(&tracks),
                    fetched: Instant::now(),
                    used: Instant::now(),
                },
            );
        }
        Ok(tracks)
    }

    pub(super) async fn prefetch_playlist_page(
        self: &Arc<Self>,
        path: &str,
        offset: usize,
    ) -> Result<(Option<usize>, Vec<String>)> {
        let page = self
            .get(
                path,
                &[("limit", "100".into()), ("offset", offset.to_string())],
            )
            .await?;
        let total = page["total"].as_u64().unwrap_or(0);
        if total > MAX_TRACKS as u64 {
            return Ok((None, Vec::new()));
        }
        let artwork = page["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(track_from_playlist_item)
            .filter_map(|track| track.image_url)
            .collect();
        if (offset as u64 + 100) < total {
            Ok((Some(offset + 100), artwork))
        } else {
            self.playlist_view(path).await?;
            Ok((None, artwork))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::catalog::tests::{cache_page, playlist_track};

    fn fixture() -> (tempfile::TempDir, Arc<Catalog>) {
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
        (dir, Arc::new(catalog))
    }

    #[tokio::test]
    async fn assembled_playlists_survive_page_eviction_and_stale_reads_stay_immediate() {
        let (_dir, catalog) = fixture();
        catalog.playlist_names().await.unwrap();
        catalog
            .prefetch_playlist_page("playlists/mix/tracks", 0)
            .await
            .unwrap();
        let path = "playlists/mix/tracks";
        let first = catalog.playlist_view(path).await.unwrap();
        catalog.cache.lock().await.clear();
        std::fs::remove_dir_all(&catalog.disk_root).unwrap();
        assert_eq!(catalog.playlist_tracks("Mix").await.unwrap(), *first);
        catalog
            .playlist_views
            .lock()
            .await
            .get_mut(path)
            .unwrap()
            .fetched = Instant::now()
            .checked_sub(FRESH_FOR + Duration::from_secs(1))
            .unwrap();
        let cached = tokio::time::timeout(Duration::from_millis(100), catalog.playlist_view(path))
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &cached));
    }

    #[tokio::test]
    async fn playlist_edits_and_library_updates_invalidate_assembled_views() {
        let (_dir, catalog) = fixture();
        let before = catalog.playlist_tracks("Mix").await.unwrap();
        let path = "playlists/mix/tracks";
        catalog.invalidate_resource(path).await.unwrap();
        cache_page(
            &catalog,
            path,
            0,
            vec![json!({"item":playlist_track(1)})],
            1,
        );
        let after = catalog.playlist_tracks("Mix").await.unwrap();
        assert_ne!(before[0].uri, after[0].uri);
        catalog.invalidate().await;
        assert!(catalog.playlist_views.lock().await.is_empty());
    }

    #[tokio::test]
    async fn assembled_cache_evicts_least_recently_used_playlists() {
        let (_dir, catalog) = fixture();
        for index in 0..=MAX_VIEWS {
            let path = format!("playlists/fixture-{index}/tracks");
            cache_page(&catalog, &path, 0, vec![], 0);
            catalog.playlist_view(&path).await.unwrap();
        }
        let views = catalog.playlist_views.lock().await;
        assert_eq!(views.len(), MAX_VIEWS);
        assert!(!views.contains_key("playlists/fixture-0/tracks"));
        assert!(views.contains_key(&format!("playlists/fixture-{MAX_VIEWS}/tracks")));
    }
}
