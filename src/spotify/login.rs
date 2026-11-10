//! Catalog authorization through oauth2 and a private librespot credential cache.
use anyhow::Result;
use librespot_core::{authentication::Credentials, cache::Cache};
use oauth2::{
    AuthType, ClientId, DeviceAuthorizationUrl, EndpointNotSet, EndpointSet, Scope,
    StandardDeviceAuthorizationResponse, TokenResponse, TokenUrl, basic::BasicClient,
};
use std::{path::PathBuf, time::Duration};

type OAuth = BasicClient<EndpointNotSet, EndpointSet, EndpointNotSet, EndpointNotSet, EndpointSet>;
const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

pub fn cache_path() -> PathBuf {
    crate::paths::cache().join("streaming_cache")
}
pub fn cache() -> Result<Cache> {
    let path = cache_path();
    crate::paths::ensure_private_dir(&path)?;
    Ok(Cache::new(Some(path), None, None, None)?)
}
pub fn available() -> Result<bool> {
    Ok(super::auth::available() || catalog_available()?)
}
pub fn catalog_available() -> Result<bool> {
    Ok(cache()?.credentials().is_some())
}
#[cfg(not(test))]
pub(crate) fn cached_account() -> Option<String> {
    let paired = super::auth::cached_username().ok()?;
    match cache().ok()?.credentials() {
        Some(credentials) => {
            let username = credentials.username?;
            paired
                .as_deref()
                .is_none_or(|paired| paired == username)
                .then_some(username)
        }
        None => paired,
    }
}
pub fn forget() -> Result<()> {
    match std::fs::remove_file(cache_path().join("credentials.json")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
pub fn proxy() -> Option<url::Url> {
    for key in [
        "RSPC_STREAMING_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
    ] {
        let Some(mut url) = std::env::var(key)
            .ok()
            .and_then(|value| url::Url::parse(&value).ok())
            .filter(|url| url.scheme() == "http")
        else {
            continue;
        };
        let _ = url.set_username("");
        let _ = url.set_password(None);
        return Some(url);
    }
    None
}
pub struct DeviceLogin {
    http: reqwest::Client,
    oauth: OAuth,
    device: StandardDeviceAuthorizationResponse,
}
impl DeviceLogin {
    pub fn url(&self) -> &str {
        self.device.verification_uri().as_str()
    }
    pub fn code(&self) -> &str {
        self.device.user_code().secret()
    }
    pub async fn finish(self) -> Result<Credentials> {
        let authorized = self
            .oauth
            .exchange_device_access_token(&self.device)
            .request_async(&self.http, tokio::time::sleep, None)
            .await?;
        Ok(Credentials::with_access_token(
            authorized.access_token().secret(),
        ))
    }
}
pub async fn begin_login() -> Result<DeviceLogin> {
    let oauth = BasicClient::new(ClientId::new(CLIENT_ID.into()))
        .set_auth_type(AuthType::RequestBody)
        .set_device_authorization_url(DeviceAuthorizationUrl::new(
            "https://accounts.spotify.com/oauth2/device/authorize".into(),
        )?)
        .set_token_uri(TokenUrl::new(
            "https://accounts.spotify.com/api/token".into(),
        )?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let scopes = [
        "streaming",
        "user-library-read",
        "user-read-private",
        "user-read-currently-playing",
        "user-read-playback-state",
        "user-modify-playback-state",
    ]
    .map(|name| Scope::new(name.into()));
    let device = oauth
        .exchange_device_code()
        .add_scopes(scopes)
        .request_async(&http)
        .await?;
    Ok(DeviceLogin {
        http,
        oauth,
        device,
    })
}
