use super::*;
use crate::daemon::local;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveMode {
    Create,
    Replace,
    Append,
}

impl Catalog {
    pub async fn reload_playlist_settings(&self) -> Result<bool> {
        let changed = self.playlist_settings.lock().unwrap().refresh()?;
        if changed {
            self.warming.lock().await.clear();
            *self.playlist_fetched.lock().await = None;
            *self.people_cache.lock().await = None;
        }
        Ok(changed)
    }
    pub async fn playlist_names(self: &Arc<Self>) -> Result<BTreeMap<String, String>> {
        if self.demo {
            let rules = self.playlist_settings.lock().unwrap().rules.clone();
            return Ok(self
                .demo_playlists
                .lock()
                .await
                .keys()
                .filter(|name| !rules.playlist("", name))
                .map(|name| (name.clone(), name.clone()))
                .collect());
        }
        if self
            .playlist_fetched
            .lock()
            .await
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Ok(self.playlists.lock().await.clone());
        }
        let items = self.pages("me/playlists").await?;
        let mut names = BTreeMap::new();
        for item in items {
            if let (Some(name), Some(id)) = (item["name"].as_str(), item["id"].as_str()) {
                let clean = name.replace(['/', '\n', '\r', '\0'], " ");
                insert_named(&mut names, clean, id.into());
            }
        }
        *self.playlists.lock().await = names.clone();
        *self.playlist_fetched.lock().await = Some(std::time::Instant::now());
        Ok(names)
    }
    async fn playlist_id(self: &Arc<Self>, name: &str) -> Result<String> {
        self.playlist_names()
            .await?
            .get(name)
            .cloned()
            .context("playlist not found")
    }
    pub async fn playlist_tracks(self: &Arc<Self>, name: &str) -> Result<Vec<TrackInfo>> {
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            return Ok(self.remember(self.local.playlist(name).await?).await);
        }
        if self.demo {
            return self
                .demo_playlists
                .lock()
                .await
                .get(name)
                .cloned()
                .context("playlist not found");
        }
        let id = self.playlist_id(name).await?;
        Ok(self
            .playlist_view(&format!("playlists/{id}/tracks"))
            .await?
            .as_ref()
            .clone())
    }
    pub(super) async fn invalidate_resource(&self, path: &str) -> Result<()> {
        if self.demo {
            return Ok(());
        }
        let bytes = {
            let mut versions = self.versions.lock().unwrap();
            let value = versions.entry(path.to_owned()).or_default();
            *value = value.wrapping_add(1).max(1);
            serde_json::to_vec(&*versions)?
        };
        self.invalidate_playlist_view(path).await;
        let file = self.disk_root.join("catalog-versions.json");
        tokio::task::spawn_blocking(move || crate::paths::write_private_file_atomic(&file, &bytes))
            .await??;
        self.cache
            .lock()
            .await
            .retain(|key, _| !key.starts_with(&format!("{path}|")));
        Ok(())
    }
    async fn playlist_changed(&self, id: Option<&str>) -> Result<()> {
        *self.people_cache.lock().await = None;
        if let Some(id) = id {
            self.invalidate_resource(&format!("playlists/{id}/tracks"))
                .await?;
        }
        self.invalidate_resource("me/playlists").await?;
        *self.playlist_fetched.lock().await = None;
        Ok(())
    }
    pub(super) async fn playlist_positions(
        self: &Arc<Self>,
        name: &str,
    ) -> Result<Vec<(usize, String)>> {
        if name.starts_with(local::PLAYLISTS) || self.demo {
            return Ok(self
                .playlist_tracks(name)
                .await?
                .into_iter()
                .enumerate()
                .filter_map(|(p, t)| t.uri.map(|u| (p, u)))
                .collect());
        }
        let id = self.playlist_id(name).await?;
        Ok(self
            .pages(&format!("playlists/{id}/tracks"))
            .await?
            .iter()
            .enumerate()
            .filter_map(|(p, item)| {
                track_from_playlist_item(item).and_then(|t| t.uri.map(|u| (p, u)))
            })
            .collect())
    }
    pub async fn playlist_add(
        self: &Arc<Self>,
        name: &str,
        uri: &str,
        position: Option<usize>,
    ) -> Result<()> {
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            let path = local::playlist_uri(uri)?;
            ensure!(
                !path.starts_with("spotify:"),
                "MPD playlists cannot play Spotify URIs"
            );
            let tail = position.map(|p| format!(" {p}")).unwrap_or_default();
            self.local
                .request(&format!(
                    "playlistadd {} {}{tail}",
                    local::quote(name)?,
                    local::quote(&path)?
                ))
                .await?;
            return Ok(());
        }
        ensure!(
            uri.starts_with("spotify:track:"),
            "Spotify playlists accept Spotify tracks only"
        );
        if self.demo {
            let mut track = self
                .resolve(uri)
                .await?
                .into_iter()
                .next()
                .context("track not found")?;
            let mut playlists = self.demo_playlists.lock().await;
            let tracks = playlists.get_mut(name).context("playlist not found")?;
            let at = position.unwrap_or(tracks.len());
            ensure!(at <= tracks.len(), "playlist position out of range");
            track.added_at = Some(chrono::Utc::now());
            tracks.insert(at, track);
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        let position = if let Some(position) = position {
            let positions = self.playlist_positions(name).await?;
            ensure!(
                position <= positions.len(),
                "playlist position out of range"
            );
            Some(
                positions
                    .get(position)
                    .map_or_else(|| positions.last().map_or(0, |(p, _)| p + 1), |(p, _)| *p),
            )
        } else {
            None
        };
        crate::spotify::edit_playlist(
            &id,
            crate::spotify::PlaylistChange::Add {
                uris: vec![uri.to_owned()],
                position,
            },
        )
        .await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_remove(
        self: &Arc<Self>,
        name: &str,
        start: usize,
        end: usize,
    ) -> Result<()> {
        let positions = self.playlist_positions(name).await?;
        ensure!(
            start <= end && end <= positions.len(),
            "playlist range out of bounds"
        );
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            if start < end {
                self.local
                    .request(&format!(
                        "playlistdelete {} {start}:{end}",
                        local::quote(name)?
                    ))
                    .await?;
            }
            return Ok(());
        }
        if self.demo {
            self.demo_playlists
                .lock()
                .await
                .get_mut(name)
                .context("playlist not found")?
                .drain(start..end);
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        crate::spotify::edit_playlist(
            &id,
            crate::spotify::PlaylistChange::Remove {
                entries: positions[start..end].to_vec(),
            },
        )
        .await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_move(
        self: &Arc<Self>,
        name: &str,
        from: usize,
        end: usize,
        to: usize,
    ) -> Result<()> {
        let positions = self.playlist_positions(name).await?;
        ensure!(
            from < end && end <= positions.len() && to <= positions.len(),
            "playlist position out of range"
        );
        let to = to.min(positions.len() - (end - from));
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            self.local
                .request(&format!(
                    "playlistmove {} {from}:{end} {to}",
                    local::quote(name)?
                ))
                .await?;
            return Ok(());
        }
        if self.demo {
            let mut playlists = self.demo_playlists.lock().await;
            let tracks = playlists.get_mut(name).context("playlist not found")?;
            let moving: Vec<_> = tracks.drain(from..end).collect();
            let at = to.min(tracks.len());
            tracks.splice(at..at, moving);
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        let entries = positions[from..end].to_vec();
        let remaining: Vec<_> = positions[..from].iter().chain(&positions[end..]).collect();
        let to = remaining.get(to).map_or_else(
            || positions.last().map_or(0, |(p, _)| p + 1) - entries.len(),
            |(p, _)| p - entries.iter().filter(|(removed, _)| removed < p).count(),
        );
        crate::spotify::edit_playlist(&id, crate::spotify::PlaylistChange::Move { entries, to })
            .await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_clear(self: &Arc<Self>, name: &str) -> Result<()> {
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            self.local
                .request(&format!("playlistclear {}", local::quote(name)?))
                .await?;
            return Ok(());
        }
        if self.demo {
            self.demo_playlists
                .lock()
                .await
                .get_mut(name)
                .context("playlist not found")?
                .clear();
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        crate::spotify::edit_playlist(&id, crate::spotify::PlaylistChange::Clear).await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_rename(self: &Arc<Self>, name: &str, new: &str) -> Result<()> {
        ensure!(!new.is_empty(), "empty playlist name");
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            let new = new.strip_prefix(local::PLAYLISTS).unwrap_or(new);
            self.local
                .request(&format!(
                    "rename {} {}",
                    local::quote(name)?,
                    local::quote(new)?
                ))
                .await?;
            return Ok(());
        }
        ensure!(
            !self.playlist_names().await?.contains_key(new),
            "playlist already exists"
        );
        if self.demo {
            let mut playlists = self.demo_playlists.lock().await;
            let tracks = playlists.remove(name).context("playlist not found")?;
            playlists.insert(new.into(), tracks);
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        crate::spotify::edit_playlist(&id, crate::spotify::PlaylistChange::Rename(new.into()))
            .await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_delete(self: &Arc<Self>, name: &str) -> Result<()> {
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            self.local
                .request(&format!("rm {}", local::quote(name)?))
                .await?;
            return Ok(());
        }
        if self.demo {
            self.demo_playlists
                .lock()
                .await
                .remove(name)
                .context("playlist not found")?;
            return Ok(());
        }
        let id = self.playlist_id(name).await?;
        crate::spotify::remove_playlist(&id).await?;
        self.playlist_changed(Some(&id)).await
    }
    pub async fn playlist_save(self: &Arc<Self>, name: &str, tracks: Vec<TrackInfo>) -> Result<()> {
        self.playlist_save_with_mode(name, tracks, "create").await
    }
    pub async fn playlist_save_with_mode(
        self: &Arc<Self>,
        name: &str,
        tracks: Vec<TrackInfo>,
        mode: &str,
    ) -> Result<()> {
        let mode = match mode {
            "create" => SaveMode::Create,
            "replace" => SaveMode::Replace,
            "append" => SaveMode::Append,
            _ => bail!("invalid playlist save mode"),
        };
        ensure!(!name.is_empty(), "empty playlist name");
        if let Some(name) = name.strip_prefix(local::PLAYLISTS) {
            ensure!(!name.is_empty(), "empty playlist name");
            let quoted_name = local::quote(name)?;
            let paths = tracks
                .iter()
                .map(|track| {
                    let uri = track.uri.as_deref().context("track has no URI")?;
                    ensure!(
                        uri.starts_with("mpd:"),
                        "MPD playlists cannot play Spotify entries"
                    );
                    let path = local::playlist_uri(uri)?;
                    ensure!(
                        !path.starts_with("spotify:"),
                        "MPD playlists cannot play Spotify URIs"
                    );
                    local::quote(&path)
                })
                .collect::<Result<Vec<_>>>()?;
            let exists = self
                .local
                .playlists()
                .await?
                .iter()
                .any(|n| n.strip_prefix(local::PLAYLISTS) == Some(name));
            if mode == SaveMode::Create {
                ensure!(!exists, "playlist already exists");
                ensure!(!paths.is_empty(), "cannot save an empty local playlist");
            } else {
                ensure!(exists, "playlist not found");
            }
            if mode == SaveMode::Replace {
                if !paths.is_empty() {
                    return self.local.replace_playlist(&quoted_name, paths).await;
                }
                self.local
                    .request(&format!("playlistclear {quoted_name}"))
                    .await?;
            }
            for path in paths {
                self.local
                    .request(&format!("playlistadd {quoted_name} {path}"))
                    .await?;
            }
            return Ok(());
        }
        let uris = tracks
            .iter()
            .map(|track| track.uri.clone().context("track has no URI"))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            uris.iter().all(|uri| uri.starts_with("spotify:track:")),
            "Spotify playlists cannot contain local files; the shared queue is saved automatically"
        );
        if self.demo {
            let mut playlists = self.demo_playlists.lock().await;
            if mode == SaveMode::Create {
                ensure!(!playlists.contains_key(name), "playlist already exists");
                playlists.insert(name.into(), tracks);
            } else {
                let playlist = playlists.get_mut(name).context("playlist not found")?;
                if mode == SaveMode::Replace {
                    *playlist = tracks;
                } else {
                    playlist.extend(tracks);
                }
            }
            return Ok(());
        }
        for uri in &uris {
            ensure!(
                matches!(
                    librespot_core::SpotifyUri::from_uri(uri)?,
                    librespot_core::SpotifyUri::Track { .. }
                ),
                "Spotify playlists accept Spotify tracks only"
            );
        }
        let id = self.playlist_names().await?.get(name).cloned();
        if mode == SaveMode::Create {
            ensure!(id.is_none(), "playlist already exists");
            crate::spotify::create_playlist(name, uris).await?;
            return self.playlist_changed(None).await;
        }
        let id = id.context("playlist not found")?;
        if mode == SaveMode::Append && uris.is_empty() {
            return Ok(());
        }
        let change = if mode == SaveMode::Replace {
            crate::spotify::PlaylistChange::Replace { uris }
        } else {
            crate::spotify::PlaylistChange::Add {
                uris,
                position: None,
            }
        };
        let result = crate::spotify::edit_playlist(&id, change).await;
        let refreshed = self.playlist_changed(Some(&id)).await;
        result?;
        refreshed
    }
}
