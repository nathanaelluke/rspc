//! Prepared discographies for the followed-artists browser.
use super::*;
use std::time::Instant;

const FRESH_FOR: Duration = Duration::from_secs(300);
const MAX_VIEWS: usize = 64;
const MAX_TRACKS: usize = 40_000;

pub(super) struct ArtistView {
    id: String,
    tracks: Arc<Vec<TrackInfo>>,
    fetched: Instant,
    used: Instant,
}

impl Catalog {
    pub(super) async fn invalidate_artist_view(&self, path: &str) {
        if let Some(id) = path
            .strip_prefix("artists/")
            .and_then(|path| path.strip_suffix("/albums"))
        {
            self.artist_view_epoch.fetch_add(1, Ordering::Relaxed);
            self.artist_views
                .lock()
                .await
                .retain(|_, view| view.id != id);
            self.artist_pages
                .lock()
                .await
                .retain(|_, view| view.id != id);
        }
    }

    pub(super) async fn artist_id(self: &Arc<Self>, name: &str) -> Result<Option<String>> {
        let id = self
            .artists()
            .await?
            .into_iter()
            .find(|(_, artist)| artist == name)
            .map(|(id, _)| id);
        // Explicit tag queries can also refer to a credited, unfollowed artist.
        let id = match id {
            Some(id) => Some(id),
            None => self
                .tracks
                .lock()
                .await
                .values()
                .flat_map(|track| &track.artist_refs)
                .find(|artist| artist.name == name)
                .and_then(|artist| artist.id.clone()),
        };
        Ok(id)
    }

    pub(super) async fn cached_artist_tracks(&self, name: &str) -> Option<Arc<Vec<TrackInfo>>> {
        let mut views = self.artist_views.lock().await;
        let view = views.get_mut(name)?;
        if view.fetched.elapsed() >= FRESH_FOR {
            return None;
        }
        view.used = Instant::now();
        Some(Arc::clone(&view.tracks))
    }

    pub(super) async fn artist_tracks(self: &Arc<Self>, name: &str) -> Result<Arc<Vec<TrackInfo>>> {
        if let Some(tracks) = self.cached_artist_tracks(name).await {
            return Ok(tracks);
        }
        let gate = self.request_gate(&format!("artist-view:{name}")).await;
        let _guard = gate.lock().await;
        if let Some(tracks) = self.cached_artist_tracks(name).await {
            return Ok(tracks);
        }
        let epoch = self.artist_view_epoch.load(Ordering::Relaxed);
        let id = self.artist_id(name).await?;
        let Some(id) = id else {
            return Ok(Arc::new(Vec::new()));
        };
        let albums = self.pages(&format!("artists/{id}/albums")).await?;
        let tracks = Arc::new(self.album_tracks_many(&albums).await?);
        let mut views = self.artist_views.lock().await;
        if tracks.len() <= MAX_TRACKS && epoch == self.artist_view_epoch.load(Ordering::Relaxed) {
            views.remove(name);
            while views.len() >= MAX_VIEWS
                || views.values().map(|view| view.tracks.len()).sum::<usize>() + tracks.len()
                    > MAX_TRACKS
            {
                let oldest = views
                    .iter()
                    .min_by_key(|(_, view)| view.used)
                    .map(|(name, _)| name.clone());
                if let Some(oldest) = oldest {
                    views.remove(&oldest);
                } else {
                    break;
                }
            }
            views.insert(
                name.to_owned(),
                ArtistView {
                    id,
                    tracks: Arc::clone(&tracks),
                    fetched: Instant::now(),
                    used: Instant::now(),
                },
            );
        }
        Ok(tracks)
    }
}
