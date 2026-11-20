use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    Config, ConfigFile, DEFAULT_MPD_READ_TIMEOUT_MS, DEFAULT_MPD_WRITE_TIMEOUT_MS, MpdAddress,
    address::MpdPassword, utils::tilde_expand,
};
use crate::config::utils::tilde_expand_path;

#[derive(derive_more::Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliConfigFile {
    pub address: String,
    #[debug(skip)]
    password: Option<String>,
    cache_dir: Option<PathBuf>,
    lyrics_dir: Option<String>,
    mpd_read_timeout_ms: u64,
    mpd_write_timeout_ms: u64,
}

impl Default for CliConfigFile {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:6600".to_string(),
            password: None,
            cache_dir: None,
            lyrics_dir: None,
            mpd_read_timeout_ms: DEFAULT_MPD_READ_TIMEOUT_MS,
            mpd_write_timeout_ms: DEFAULT_MPD_WRITE_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub address: MpdAddress,
    pub password: Option<MpdPassword>,
    pub cache_dir: Option<PathBuf>,
    pub lyrics_dir: Option<String>,
    pub mpd_read_timeout: Duration,
    pub mpd_write_timeout: Duration,
}

impl Default for CliConfig {
    fn default() -> Self {
        CliConfigFile::default().into_config(None, None)
    }
}

impl From<ConfigFile> for CliConfigFile {
    fn from(value: ConfigFile) -> Self {
        Self {
            address: value.address,
            password: value.password,
            cache_dir: value.cache_dir,
            lyrics_dir: value.lyrics_dir,
            mpd_read_timeout_ms: value.mpd_read_timeout_ms,
            mpd_write_timeout_ms: value.mpd_write_timeout_ms,
        }
    }
}

impl From<Config> for CliConfig {
    fn from(value: Config) -> Self {
        Self {
            address: value.address,
            password: value.password,
            cache_dir: value.cache_dir,
            lyrics_dir: value.lyrics_dir,
            mpd_read_timeout: value.mpd_read_timeout,
            mpd_write_timeout: value.mpd_write_timeout,
        }
    }
}

impl From<&Config> for CliConfig {
    fn from(value: &Config) -> Self {
        Self {
            address: value.address.clone(),
            password: value.password.clone(),
            cache_dir: value.cache_dir.clone(),
            lyrics_dir: value.lyrics_dir.clone(),
            mpd_read_timeout: value.mpd_read_timeout,
            mpd_write_timeout: value.mpd_write_timeout,
        }
    }
}

impl CliConfigFile {
    pub fn into_config(
        self,
        address_cli: Option<String>,
        password_cli: Option<String>,
    ) -> CliConfig {
        let (address, password) =
            MpdAddress::resolve(address_cli, password_cli, self.address, self.password);

        CliConfig {
            cache_dir: self.cache_dir.map(|v| tilde_expand_path(&v)),
            lyrics_dir: self.lyrics_dir.map(|v| {
                let v = tilde_expand(&v);
                if v.ends_with('/') {
                    v.into_owned()
                } else {
                    format!("{v}/")
                }
            }),
            address,
            password,
            mpd_read_timeout: Duration::from_millis(self.mpd_read_timeout_ms),
            mpd_write_timeout: Duration::from_millis(self.mpd_write_timeout_ms),
        }
    }
}
