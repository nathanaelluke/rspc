use anyhow::{Context, Result};
use serde::Deserialize;
use std::{path::PathBuf, time::SystemTime};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct PlaylistExclusions {
    pub exclude_folders: Vec<String>,
    pub exclude_playlists: Vec<String>,
}

impl PlaylistExclusions {
    pub fn folder(&self, id: &str, name: &str, path: &str) -> bool {
        self.exclude_folders.iter().any(|rule| {
            if let Some(folder) = rule.strip_prefix("spotify:folder:") {
                folder == id
            } else {
                rule.to_lowercase() == name.to_lowercase()
                    || rule.to_lowercase() == path.to_lowercase()
            }
        })
    }
    pub fn playlist(&self, uri: &str, name: &str) -> bool {
        self.exclude_playlists.iter().any(|rule| {
            if rule.starts_with("spotify:playlist:") {
                rule == uri
            } else {
                rule.to_lowercase() == name.to_lowercase()
            }
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    spotify: PlaylistExclusions,
}

#[derive(Default)]
pub struct PlaylistSettings {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    pub rules: PlaylistExclusions,
}
impl PlaylistSettings {
    pub fn configure(&mut self, path: Option<PathBuf>) -> Result<()> {
        self.path = path;
        self.modified = None;
        self.refresh()?;
        Ok(())
    }
    pub fn refresh(&mut self) -> Result<bool> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let modified = std::fs::metadata(path)
            .with_context(|| format!("read rspc config {}", path.display()))?
            .modified()?;
        if self.modified == Some(modified) {
            return Ok(false);
        }
        let text = std::fs::read_to_string(path)?;
        let config: Config = ron::from_str(&text)
            .with_context(|| format!("read Spotify settings from {}", path.display()))?;
        let changed = config.spotify != self.rules;
        self.rules = config.spotify;
        self.modified = Some(modified);
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusions_match_names_paths_and_stable_spotify_uris() {
        let rules = PlaylistExclusions {
            exclude_folders: vec![
                "archive".into(),
                "Music/Old".into(),
                "spotify:folder:AbC".into(),
            ],
            exclude_playlists: vec!["Old Mix".into(), "spotify:playlist:XyZ".into()],
        };
        assert!(rules.folder("a", "ARCHIVE", "Parent/ARCHIVE"));
        assert!(rules.folder("b", "Old", "Music/Old"));
        assert!(!rules.folder("b", "Old", "Other/Old"));
        assert!(rules.folder("AbC", "Renamed", "Renamed"));
        assert!(!rules.folder("abc", "Renamed", "Renamed"));
        assert!(rules.playlist("spotify:playlist:other", "old mix"));
        assert!(rules.playlist("spotify:playlist:XyZ", "Renamed"));
        assert!(!rules.playlist("spotify:playlist:xyz", "Renamed"));
    }
    #[test]
    fn spotify_options_can_share_an_existing_rmpc_config() {
        let config:Config=ron::from_str(r##"#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(address:"/tmp/mpd.sock",password:Some("fixture"),album_art:(method:Auto),
 tabs:[(name:"Queue",pane:Pane(Queue))],spotify:(exclude_folders:["archive"],exclude_playlists:["Old Mix"]))"##).unwrap();
        assert_eq!(config.spotify.exclude_folders, ["archive"]);
        assert_eq!(config.spotify.exclude_playlists, ["Old Mix"]);
        let defaults: Config = ron::from_str("(address: \"/tmp/mpd.sock\",)").unwrap();
        assert_eq!(defaults.spotify, PlaylistExclusions::default());
    }
    #[test]
    fn original_rmpc_example_needs_no_spotify_section() {
        let config: Config =
            ron::from_str(include_str!("../crates/rspc-ui/assets/example_config.ron")).unwrap();
        assert_eq!(config.spotify, PlaylistExclusions::default());
    }
}
