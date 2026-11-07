use super::*;

pub const PREFIX: &str = "rspc://catalog/";

impl Catalog {
    async fn context_track(self: &Arc<Self>, id: &str) -> Result<TrackInfo> {
        ensure!(
            !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
            "invalid Spotify track ID"
        );
        let uri = format!("spotify:track:{id}");
        let track = self
            .resolve(&uri)
            .await?
            .into_iter()
            .next()
            .context("track not found")?;
        if track.album_id.is_some() && track.artist_refs.iter().any(|artist| artist.id.is_some())
            || self.demo
        {
            return Ok(track);
        }
        let track = track_from_json(&self.get(&format!("tracks/{id}"), &[]).await?)
            .context("track not found")?;
        self.remember(vec![track.clone()]).await;
        Ok(track)
    }

    /// Explicit catalog navigation never contributes entries to library roots.
    pub async fn browse_catalog(
        self: &Arc<Self>,
        path: &str,
    ) -> Result<(Vec<String>, Vec<TrackInfo>)> {
        let path = path.strip_prefix(PREFIX).context("invalid catalog path")?;
        let parts: Vec<_> = path.splitn(3, '/').collect();
        match parts.as_slice() {
            ["album", track] => {
                let track = self.context_track(track).await?;
                let id = track.album_id.context("track has no album")?;
                Ok((vec![], self.album_tracks(&id).await?))
            }
            ["release", id, ..] => {
                ensure!(
                    self.demo || !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
                    "invalid album ID"
                );
                Ok((vec![], self.album_tracks(id).await?))
            }
            ["artist", track, name] => {
                let name = url::form_urlencoded::parse(name.as_bytes())
                    .next()
                    .map(|(key, _)| key.into_owned())
                    .context("missing artist name")?;
                let track = self.context_track(track).await?;
                let mut id = track
                    .artist_refs
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(&name))
                    .and_then(|a| a.id.clone());
                if id.is_none()
                    && let Some(album) = track.album_id.as_deref()
                {
                    let album = self.get(&format!("albums/{album}"), &[]).await?;
                    id = album["artists"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|a| {
                            a["name"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(&name))
                        })
                        .and_then(|a| a["id"].as_str().map(str::to_owned));
                }
                let id = id.context("could not resolve artist from this track's credits")?;
                let albums = if self.demo {
                    demo_albums()
                } else {
                    self.pages(&format!("artists/{id}/albums")).await?
                };
                let mut seen = std::collections::HashSet::new();
                let mut names = HashMap::<String, usize>::new();
                let mut directories = Vec::new();
                let mut known = self.albums.lock().await;
                for album in albums {
                    let Some(id) = album["id"].as_str() else {
                        continue;
                    };
                    if !seen.insert(id.to_owned()) {
                        continue;
                    }
                    let date = album["release_date"].as_str().unwrap_or("");
                    let title = album["name"].as_str().unwrap_or("Untitled");
                    let group = album["album_group"].as_str().unwrap_or("album");
                    let mut label = format!("{date} {title} [{group}]")
                        .trim()
                        .replace(['\n', '\r', '\0'], " ")
                        .replace('/', "∕");
                    let count = names.entry(label.clone()).or_default();
                    *count += 1;
                    if *count > 1 {
                        label.push_str(&format!(" ({count})"));
                    }
                    directories.push(format!("{PREFIX}release/{id}/{label}"));
                    known.insert(id.to_owned(), album);
                }
                Ok((directories, vec![]))
            }
            _ => bail!("invalid catalog path"),
        }
    }
}
