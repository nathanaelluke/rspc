//! Pairing flow adapted from Spotatui's MIT-licensed credentials module.
use anyhow::Result;
use librespot_core::{authentication::Credentials, cache::Cache};
use librespot_oauth::{DeviceAuthClient, DeviceAuthClientBuilder, DeviceAuthorization};
use std::path::PathBuf;
const CLIENT: &str = "65b708073fc0480ea92a077233ca87bd";
const SCOPES: [&str; 6] = [
    "streaming",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-library-read",
    "user-read-private",
];
pub fn get_default_cache_path() -> Option<PathBuf> {
    Some(crate::paths::cache().join("streaming_cache"))
}
pub fn streaming_proxy_from_env() -> Option<url::Url> {
    [
        "RSPC_STREAMING_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok())
    .filter_map(|s| url::Url::parse(&s).ok())
    .find_map(|mut u| {
        if u.scheme() != "http" {
            return None;
        }
        let _ = u.set_username("");
        let _ = u.set_password(None);
        Some(u)
    })
}
pub fn clear_cached_streaming_credentials(path: &Option<PathBuf>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p.join("credentials.json"));
    }
}
pub fn available() -> Result<bool> {
    let path = get_default_cache_path().unwrap();
    crate::paths::ensure_private_dir(&path)?;
    let target = path.join("credentials.json");
    if !target.exists() {
        let old = crate::paths::xdg("XDG_CACHE_HOME", ".cache")
            .join("spotatui/streaming_cache/credentials.json");
        if old.is_file() {
            crate::paths::write_private_file_atomic(&target, &std::fs::read(old)?)?;
        }
    }
    Ok(Cache::new(Some(path), None, None, None)?
        .credentials()
        .is_some())
}
pub struct PendingLogin {
    client: DeviceAuthClient,
    authorization: DeviceAuthorization,
}
impl PendingLogin {
    pub fn url(&self) -> &str {
        self.authorization.url()
    }
    pub fn code(&self) -> &str {
        self.authorization.user_code()
    }
    pub async fn finish(self) -> Result<Credentials> {
        Ok(Credentials::with_access_token(
            self.client
                .poll_for_token_async(&self.authorization)
                .await?
                .access_token,
        ))
    }
}
pub async fn begin_login() -> Result<PendingLogin> {
    let client = DeviceAuthClientBuilder::new(CLIENT, SCOPES.to_vec()).build()?;
    let authorization = client.request_device_code_async().await?;
    Ok(PendingLogin {
        client,
        authorization,
    })
}
