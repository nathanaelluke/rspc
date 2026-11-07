//! Selection summaries avoid transferring every song through the MPD socket.
use super::*;
use std::collections::HashSet;

type Info = Vec<(String, String)>;
const LIBRARY_PREFIX: &str = "rspc://library/";

pub(super) fn library_target(path: &str) -> Result<Option<(&str, String)>> {
    let Some(path) = path.strip_prefix(LIBRARY_PREFIX) else {
        return Ok(None);
    };
    let (kind, rest) = path.split_once('/').context("missing library kind")?;
    ensure!(matches!(kind, "album" | "artist"), "unknown library kind");
    let encoded = rest.split('/').next().context("missing library name")?;
    let name = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .map(|(name, _)| name.into_owned())
        .context("missing library name")?;
    Ok(Some((kind, name)))
}

fn count(label: &str, kind: &str, total: usize) -> Info {
    vec![
        ("Selection".into(), label.into()),
        (kind.into(), total.to_string()),
    ]
}

fn songs<'a>(tracks: impl IntoIterator<Item = &'a TrackInfo>) -> Info {
    let mut artists = HashSet::new();
    let mut albums = HashSet::new();
    let mut count = 0;
    let mut duration_ms = 0;
    for track in tracks {
        count += 1;
        duration_ms += track.duration_ms;
        artists.extend(&track.artists);
        albums.insert(&track.album);
    }
    let seconds = duration_ms / 1000;
    let duration = if seconds >= 86400 {
        format!(
            "{}d {:02}:{:02}:{:02}",
            seconds / 86400,
            seconds / 3600 % 24,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    };
    vec![
        ("Songs".into(), count.to_string()),
        ("Total duration".into(), duration),
        ("Artists".into(), artists.len().to_string()),
        ("Albums".into(), albums.len().to_string()),
        ("Genres".into(), "0".into()),
    ]
}

impl Catalog {
    pub fn library_directories(kind: &str, names: Vec<String>) -> Vec<String> {
        let mut labels = BTreeMap::new();
        for name in names {
            let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
            let label = name.replace(['\n', '\r', '\0'], " ").replace('/', "∕");
            insert_named(&mut labels, label, encoded);
        }
        labels
            .into_iter()
            .map(|(label, encoded)| format!("{LIBRARY_PREFIX}{kind}/{encoded}/{label}"))
            .collect()
    }

    pub async fn selection_info(self: &Arc<Self>, kind: &str, value: &str) -> Result<Info> {
        match kind {
            "artist" => {
                // Release metadata is enough for a useful artist summary. Do not fetch
                // thousands of tracks just to open an information popup.
                let mut releases = HashSet::new();
                let mut offset = 0;
                loop {
                    let page = self.artist_album_page(value, offset).await?;
                    releases.extend(page.albums.into_iter().map(|album| album.uri));
                    match page.next {
                        Some(next) if next > offset => offset = next,
                        _ => break,
                    }
                }
                let mut info = count(value, "Releases", releases.len());
                if let Some(cached) = self.cached_artist_tracks(value).await {
                    let local = self.local.all().await.ok();
                    info.extend(songs(
                        cached
                            .iter()
                            .chain(local.iter().flat_map(|tracks| tracks.iter()))
                            .filter(|track| track.artists.iter().any(|artist| artist == value)),
                    ));
                }
                Ok(info)
            }
            "albumartist" | "album" => {
                let tag = if kind == "album" {
                    "Album"
                } else {
                    "AlbumArtist"
                };
                Ok(songs(
                    &self
                        .find(&format!("({tag} == '{}')", escape(value)), false)
                        .await?,
                ))
            }
            "playlist" => Ok(songs(&self.playlist_tracks(value).await?)),
            "path" => self.path_info(value).await,
            _ => bail!("unknown selection kind"),
        }
    }

    /// Recursive folder actions use their virtual directory identity, not a prefix
    /// match against Spotify track URIs. A visited set also bounds malformed trees.
    pub async fn selection_tracks(self: &Arc<Self>, path: &str) -> Result<Vec<TrackInfo>> {
        let mut pending = vec![path.to_owned()];
        let mut visited = HashSet::new();
        let mut tracks = Vec::new();
        while let Some(path) = pending.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            ensure!(visited.len() <= 100_000, "selection has too many folders");
            match path.as_str() {
                "Albums" => {
                    if self.demo {
                        tracks.extend(self.liked().await?);
                    } else {
                        tracks.extend(self.album_tracks_many(&self.saved_albums().await?).await?);
                        if let Ok(local) = self.local.all().await {
                            tracks.extend(local.iter().cloned());
                        }
                    }
                }
                "Artists" => pending.extend(
                    self.tags("Artist", "")
                        .await?
                        .into_iter()
                        .map(|name| format!("Artists/{name}")),
                ),
                "Playlists" => pending.extend(
                    self.playlist_names()
                        .await?
                        .into_keys()
                        .map(|name| format!("Playlists/{name}")),
                ),
                "Local Playlists" => pending.extend(
                    self.local
                        .playlists()
                        .await?
                        .into_iter()
                        .map(|name| format!("Local Playlists/{name}")),
                ),
                path if path == "People" || path.starts_with("People/") => {
                    let (dirs, songs) = self.browse_people(path).await?;
                    pending.extend(dirs);
                    tracks.extend(songs);
                }
                path if path.starts_with("rspc://catalog/") => {
                    let (dirs, songs) = self.browse_catalog(path).await?;
                    pending.extend(dirs);
                    tracks.extend(songs);
                }
                path => tracks.extend(self.resolve(path).await?),
            }
            ensure!(tracks.len() <= 1_000_000, "selection has too many songs");
        }
        Ok(tracks)
    }

    async fn path_info(self: &Arc<Self>, path: &str) -> Result<Info> {
        if let Some((kind, value)) = library_target(path)? {
            return Box::pin(self.selection_info(kind, &value)).await;
        }
        match path {
            "Albums" => Ok(count(path, "Albums", self.tags("Album", "").await?.len())),
            "Artists" => Ok(count(path, "Artists", self.tags("Artist", "").await?.len())),
            "Playlists" => Ok(count(path, "Playlists", self.playlist_names().await?.len())),
            "Local Playlists" => Ok(count(
                path,
                "Playlists",
                self.local.playlists().await?.len(),
            )),
            "People" => Ok(count(
                path,
                "People",
                self.browse_people(path).await?.0.len(),
            )),
            _ if path.starts_with("People/profile/") => Ok(count(
                path.rsplit('/').next().unwrap_or(path),
                "Public playlists",
                self.browse_people(path).await?.0.len(),
            )),
            _ if path.starts_with("Artists/") => {
                Box::pin(self.selection_info("artist", &path[8..])).await
            }
            _ if path.starts_with("rspc://catalog/") => {
                let (directories, tracks) = self.browse_catalog(path).await?;
                if directories.is_empty() {
                    Ok(songs(&tracks))
                } else {
                    Ok(vec![("Releases".into(), directories.len().to_string())])
                }
            }
            _ => Ok(songs(&self.resolve(path).await?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_paths_keep_names_separate_from_display_labels() {
        let name = "A/B + C% & 'D' / 音楽";
        let path = Catalog::library_directories("album", vec![name.into()]).remove(0);
        assert_eq!(path.rsplit('/').next(), Some("A∕B + C% & 'D' ∕ 音楽"));
        assert_eq!(library_target(&path).unwrap(), Some(("album", name.into())));
        assert!(library_target("rspc://library/unknown/name/label").is_err());
    }

    #[test]
    fn visually_identical_labels_keep_distinct_directory_keys() {
        let paths = Catalog::library_directories("album", vec!["A/B".into(), "A∕B".into()]);
        let names: HashSet<_> = paths
            .iter()
            .map(|path| path.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        let originals: HashSet<_> = paths
            .iter()
            .map(|path| library_target(path).unwrap().unwrap().1)
            .collect();
        assert_eq!(originals, HashSet::from(["A/B".into(), "A∕B".into()]));
    }

    #[tokio::test]
    async fn summaries_cover_virtual_folders_and_curator_profiles() {
        let catalog = Arc::new(Catalog::new(true));
        for (kind, value, key, expected) in [
            ("path", "Liked Songs", "Songs", "3"),
            ("path", "Recently Played", "Songs", "3"),
            ("path", "Playlists", "Playlists", "1"),
            ("path", "Albums", "Albums", "1"),
            ("path", "Artists", "Artists", "1"),
            ("path", "People", "People", "1"),
            (
                "path",
                "People/profile/demo-user/Example Curator",
                "Public playlists",
                "1",
            ),
            ("path", "People/playlist/demo/Demo Mix", "Songs", "3"),
            ("playlist", "Demo Mix", "Songs", "3"),
            ("album", "Example Sessions", "Songs", "3"),
            ("albumartist", "The Example Ensemble", "Songs", "3"),
            ("artist", "The Example Ensemble", "Releases", "1"),
        ] {
            let info: HashMap<_, _> = catalog
                .selection_info(kind, value)
                .await
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(
                info.get(key).map(String::as_str),
                Some(expected),
                "{kind}: {value}"
            );
        }
    }

    #[tokio::test]
    async fn recursive_selection_uses_virtual_paths_instead_of_track_uri_prefixes() {
        let catalog = Arc::new(Catalog::new(true));
        for path in [
            "Liked Songs",
            "Albums",
            "Artists",
            "Playlists",
            "People",
            "People/profile/demo-user/Example Curator",
        ] {
            assert_eq!(
                catalog.selection_tracks(path).await.unwrap().len(),
                3,
                "{path}"
            );
        }
    }
}
