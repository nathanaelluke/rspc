use super::*;
use crate::{model::LibraryItem, spotify::CollectionEdit};
use std::collections::HashSet;

impl Catalog {
    async fn refresh_library<'a>(
        self: &Arc<Self>,
        resource: &str,
        removed: Option<impl Iterator<Item = &'a str>>,
    ) -> Result<()> {
        self.invalidate_resource(resource)
            .await
            .context("Updated Spotify, but could not refresh the local library cache")?;
        if let Some(removed) = removed {
            let mut dates = self.library_added.lock().await;
            for uri in removed {
                dates.remove(uri);
            }
        }
        self.tag_cache.lock().await.clear();
        self.content_revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub async fn add_liked(self: &Arc<Self>, uris: &[String]) -> Result<usize> {
        let (added, _) = self.update_liked(uris, CollectionEdit::Add).await?;
        Ok(added)
    }
    pub async fn remove_liked(self: &Arc<Self>, uris: &[String]) -> Result<usize> {
        let (_, removed) = self.update_liked(uris, CollectionEdit::Remove).await?;
        Ok(removed)
    }
    pub async fn toggle_liked(self: &Arc<Self>, uris: &[String]) -> Result<(usize, usize)> {
        self.update_liked(uris, CollectionEdit::Toggle).await
    }
    async fn update_liked(
        self: &Arc<Self>,
        uris: &[String],
        edit: CollectionEdit,
    ) -> Result<(usize, usize)> {
        ensure!(!uris.is_empty(), "No songs selected");
        ensure!(
            uris.iter().all(|uri| uri.starts_with("spotify:track:")),
            "Liked Songs accepts Spotify tracks only"
        );
        let (added, removed) = if self.demo {
            let mut tracks = Vec::new();
            if edit != CollectionEdit::Remove {
                for uri in uris {
                    tracks.extend(self.resolve(uri).await?);
                }
            }
            let mut liked = self.demo_liked.lock().await;
            if !tracks.is_empty() {
                let mut known: HashSet<_> =
                    liked.iter().filter_map(|track| track.uri.clone()).collect();
                tracks.retain(|track| {
                    track
                        .uri
                        .as_ref()
                        .is_some_and(|uri| known.insert(uri.clone()))
                });
                let added_at = chrono::Utc::now();
                for track in &mut tracks {
                    track.added_at = Some(added_at);
                }
            }
            let count = tracks.len();
            let before = liked.len();
            if edit != CollectionEdit::Add {
                let uris: HashSet<_> = uris.iter().map(String::as_str).collect();
                liked.retain(|track| track.uri.as_deref().is_none_or(|uri| !uris.contains(uri)));
            }
            let removed = before - liked.len();
            liked.splice(0..0, tracks);
            (count, removed)
        } else {
            match edit {
                CollectionEdit::Remove => (0, crate::spotify::remove_liked_tracks(uris).await?),
                CollectionEdit::Add => (crate::spotify::add_liked_tracks(uris).await?, 0),
                CollectionEdit::Toggle => crate::spotify::toggle_liked_tracks(uris).await?,
            }
        };
        if added + removed > 0 {
            self.refresh_library(
                "me/tracks",
                (removed > 0).then(|| uris.iter().map(String::as_str)),
            )
            .await?;
        }
        Ok((added, removed))
    }

    pub async fn library_items(
        self: &Arc<Self>,
        kind: &str,
        value: &str,
    ) -> Result<Vec<LibraryItem>> {
        let mut items = BTreeMap::new();
        match kind {
            "track" if value.starts_with("spotify:track:") => {
                for track in self.resolve(value).await? {
                    if track.is_local {
                        continue;
                    }
                    if let Some(id) = track.album_id {
                        items.insert(format!("spotify:album:{id}"), track.album);
                    }
                    for artist in track.artist_refs {
                        if let Some(id) = artist.id {
                            items.insert(format!("spotify:artist:{id}"), artist.name);
                        }
                    }
                }
            }
            "album" => {
                let saved = self.saved_albums().await?;
                let known = self.albums.lock().await;
                for album in saved.iter().chain(known.values()) {
                    if album["name"].as_str() == Some(value)
                        && let Some(id) = album["id"].as_str()
                    {
                        items.insert(format!("spotify:album:{id}"), value.to_owned());
                    }
                }
            }
            "artist" | "albumartist" => {
                if let Some(id) = self.artist_id(value).await? {
                    items.insert(format!("spotify:artist:{id}"), value.to_owned());
                }
            }
            "path" => {
                if let Some((kind, name)) = selection::library_target(value)? {
                    return Box::pin(self.library_items(kind, &name)).await;
                }
                if let Some(name) = value.strip_prefix("Albums/") {
                    return Box::pin(self.library_items("album", name)).await;
                }
                if let Some(name) = value.strip_prefix("Artists/") {
                    return Box::pin(self.library_items("artist", name)).await;
                }
                let uri = if let Some(id) = value
                    .strip_prefix("rspc://artist-album/spotify/")
                    .or_else(|| value.strip_prefix("rspc://catalog/release/"))
                {
                    format!(
                        "spotify:album:{}",
                        id.split(['/', '?']).next().unwrap_or("")
                    )
                } else {
                    value.to_owned()
                };
                if let Some(id) = uri.strip_prefix("spotify:album:") {
                    let album = self.library_album(id).await?;
                    if let Some(name) = album["name"].as_str() {
                        items.insert(uri, name.to_owned());
                    }
                } else if let Some(id) = uri.strip_prefix("spotify:artist:") {
                    if let Some((_, name)) =
                        self.artists().await?.into_iter().find(|(i, _)| i == id)
                    {
                        items.insert(uri, name);
                    } else if let Some(name) = self
                        .tracks
                        .lock()
                        .await
                        .values()
                        .flat_map(|track| &track.artist_refs)
                        .find(|artist| artist.id.as_deref() == Some(id))
                        .map(|artist| artist.name.clone())
                    {
                        items.insert(uri, name);
                    }
                }
            }
            _ => {}
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let albums = async {
            if items.keys().any(|uri| uri.starts_with("spotify:album:")) {
                self.saved_albums().await
            } else {
                Ok(Vec::new())
            }
        };
        let artists = async {
            if items.keys().any(|uri| uri.starts_with("spotify:artist:")) {
                self.artists().await
            } else {
                Ok(Vec::new())
            }
        };
        let (albums, artists) = tokio::try_join!(albums, artists)?;
        let known: HashSet<_> = albums
            .iter()
            .filter_map(|album| album["id"].as_str())
            .map(|id| format!("spotify:album:{id}"))
            .chain(
                artists
                    .into_iter()
                    .map(|(id, _)| format!("spotify:artist:{id}")),
            )
            .collect();
        Ok(items
            .into_iter()
            .map(|(uri, name)| LibraryItem {
                saved: known.contains(&uri),
                uri,
                name,
            })
            .collect())
    }

    async fn library_album(self: &Arc<Self>, id: &str) -> Result<Value> {
        ensure!(
            self.demo || !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
            "Invalid Spotify album ID"
        );
        if let Some(album) = self.albums.lock().await.get(id).cloned() {
            return Ok(album);
        }
        if self.demo {
            if let Some(album) = demo_albums().into_iter().find(|album| album["id"] == id) {
                return Ok(album);
            }
            return self
                .tracks
                .lock()
                .await
                .values()
                .find(|track| track.album_id.as_deref() == Some(id))
                .map(|track| json!({"id":id,"name":track.album,"artists":track.artist_refs}))
                .context("Album not found");
        }
        self.get(&format!("albums/{id}"), &[]).await
    }

    pub async fn update_saved_item(self: &Arc<Self>, uri: &str, remove: bool) -> Result<usize> {
        let resource = if uri.starts_with("spotify:album:") {
            "me/albums"
        } else if uri.starts_with("spotify:artist:") {
            "me/following"
        } else {
            bail!("Select a Spotify album or artist");
        };
        let count = if self.demo {
            let item = self
                .library_items("path", uri)
                .await?
                .into_iter()
                .next()
                .context("Album or artist not found")?;
            if let Some(id) = uri.strip_prefix("spotify:album:") {
                let album = self.library_album(id).await?;
                let mut saved = self.demo_saved_albums.lock().await;
                if saved.iter().any(|album| album["id"].as_str() == Some(id)) != remove {
                    return Ok(0);
                }
                saved.retain(|album| album["id"].as_str() != Some(id));
                if !remove {
                    saved.push(album);
                }
            } else if let Some(id) = uri.strip_prefix("spotify:artist:") {
                let mut artists = self.demo_followed_artists.lock().await;
                if artists.iter().any(|(artist, _)| artist == id) != remove {
                    return Ok(0);
                }
                artists.retain(|(artist, _)| artist != id);
                if !remove {
                    artists.push((id.to_owned(), item.name));
                }
            }
            1
        } else {
            crate::spotify::update_saved_item(uri, remove).await?
        };
        if count > 0 {
            self.refresh_library(resource, remove.then(|| std::iter::once(uri)))
                .await?;
        }
        Ok(count)
    }
}
