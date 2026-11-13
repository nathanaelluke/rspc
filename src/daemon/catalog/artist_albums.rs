//! Paged release indexes. Browsing an artist never resolves all of their tracks.
use super::*;
use crate::model::{ArtistAlbum, ArtistAlbumPage};
use std::time::Instant;

const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 256;

pub(super) struct ArtistPageView {
    pub(super) id: String,
    page: ArtistAlbumPage,
    fetched: Instant,
    used: Instant,
}

impl Catalog {
    async fn spotify_artist_album_page(
        self: &Arc<Self>,
        name: &str,
        offset: usize,
    ) -> Result<ArtistAlbumPage> {
        let key = (name.to_owned(), offset);
        {
            let mut views = self.artist_pages.lock().await;
            if let Some(view) = views.get_mut(&key)
                && view.fetched.elapsed() < Duration::from_secs(300)
            {
                view.used = Instant::now();
                return Ok(view.page.clone());
            }
        }
        let Some(id) = self.artist_id(name).await? else {
            return Ok(ArtistAlbumPage {
                total: Some(0),
                ..Default::default()
            });
        };
        let epoch = self.artist_view_epoch.load(Ordering::Relaxed);
        let page = self
            .get(
                &format!("artists/{id}/albums"),
                &[
                    ("limit", PAGE_SIZE.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        // Cached readers must not wait behind a background network refresh.
        let gate = self
            .request_gate(&format!("artist-page:{name}:{offset}"))
            .await;
        let _guard = gate.lock().await;
        if let Some(view) = self.artist_pages.lock().await.get(&key)
            && view.fetched.elapsed() < Duration::from_secs(300)
        {
            return Ok(view.page.clone());
        }
        let albums = page["items"].as_array().context("missing artist albums")?;
        let total = page["total"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok());
        let next = page["next"].is_string().then_some(offset + PAGE_SIZE);
        ensure!(
            next.is_none_or(|offset| offset < 100_000),
            "artist album pagination limit exceeded"
        );
        let mut known = self.albums.lock().await;
        let mut result = Vec::new();
        for album in albums {
            let Some(id) = album["id"].as_str() else {
                continue;
            };
            known.insert(id.to_owned(), album.clone());
            result.push(album_summary(name, id, album));
        }
        drop(known);
        let page = ArtistAlbumPage {
            albums: result,
            next,
            total,
        };
        let mut views = self.artist_pages.lock().await;
        if epoch == self.artist_view_epoch.load(Ordering::Relaxed) {
            if views.len() >= MAX_PAGES
                && let Some(oldest) = views
                    .iter()
                    .min_by_key(|(_, view)| view.used)
                    .map(|(key, _)| key.clone())
            {
                views.remove(&oldest);
            }
            views.insert(
                key,
                ArtistPageView {
                    id,
                    page: page.clone(),
                    fetched: Instant::now(),
                    used: Instant::now(),
                },
            );
        }
        Ok(page)
    }

    pub async fn artist_album_page(
        self: &Arc<Self>,
        name: &str,
        offset: usize,
    ) -> Result<ArtistAlbumPage> {
        ensure!(
            offset.is_multiple_of(PAGE_SIZE) && offset < 100_000,
            "invalid artist page offset"
        );
        if self.demo {
            return Ok(ArtistAlbumPage {
                albums: if offset == 0 {
                    demo_albums()
                        .iter()
                        .filter_map(|album| Some(album_summary(name, album["id"].as_str()?, album)))
                        .collect()
                } else {
                    vec![]
                },
                next: None,
                total: Some(if offset == 0 { demo_albums().len() } else { 0 }),
            });
        }
        let local = async {
            if offset == 0 {
                self.local.all().await
            } else {
                Ok(Arc::new(Vec::new()))
            }
        };
        let (spotify, local) = tokio::join!(self.spotify_artist_album_page(name, offset), local);
        let mut local_albums = BTreeMap::new();
        if offset == 0
            && let Ok(tracks) = local
        {
            for track in tracks
                .iter()
                .filter(|track| track.artists.iter().any(|artist| artist == name))
            {
                let date = track.release_date.clone().unwrap_or_default();
                let mut uri = url::Url::parse("rspc://artist-album/local")?;
                uri.query_pairs_mut()
                    .append_pair("artist", name)
                    .append_pair("album", &track.album)
                    .append_pair("date", &date);
                local_albums
                    .entry((track.album.clone(), date.clone()))
                    .or_insert(ArtistAlbum {
                        uri: uri.into(),
                        name: track.album.clone(),
                        date,
                    });
            }
        }
        let mut page = match spotify {
            Ok(page) => page,
            Err(_) if !local_albums.is_empty() => ArtistAlbumPage::default(),
            Err(error) => return Err(error),
        };
        page.total = page.total.map(|total| total + local_albums.len());
        page.albums.extend(local_albums.into_values());
        Ok(page)
    }

    pub(super) async fn artist_album_tracks(self: &Arc<Self>, uri: &str) -> Result<Vec<TrackInfo>> {
        let uri = url::Url::parse(uri)?;
        let query: HashMap<_, _> = uri.query_pairs().collect();
        let artist = query.get("artist").context("missing artist")?;
        let tracks = if let Some(id) = uri.path().strip_prefix("/spotify/") {
            ensure!(
                self.demo || !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
                "invalid album ID"
            );
            self.album_tracks(id).await?
        } else if uri.path() == "/local" {
            let album = query.get("album").context("missing local album")?;
            let date = query
                .get("date")
                .map(|date| date.as_ref())
                .unwrap_or_default();
            self.local
                .all()
                .await?
                .iter()
                .filter(|track| {
                    track.album == album.as_ref()
                        && track.release_date.as_deref().unwrap_or_default() == date
                })
                .cloned()
                .collect()
        } else {
            bail!("invalid artist album path")
        };
        Ok(tracks
            .into_iter()
            .filter(|track| track.artists.iter().any(|name| name == artist.as_ref()))
            .collect())
    }
}

fn album_summary(artist: &str, id: &str, album: &Value) -> ArtistAlbum {
    // ID validation happens before this URI is resolved into tracks.
    let mut uri = url::Url::parse("rspc://artist-album/").expect("static album URI");
    uri.set_path(&format!("/spotify/{id}"));
    uri.query_pairs_mut().append_pair("artist", artist);
    ArtistAlbum {
        uri: uri.into(),
        name: album["name"].as_str().unwrap_or("Untitled").to_owned(),
        date: album["release_date"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    }
}
