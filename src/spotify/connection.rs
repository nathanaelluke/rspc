//! Own the daemon's metadata connection and reconnect policy. Audio is Soloist's.
use super::{client::Client, login};
use anyhow::{Context, Result};
use librespot_core::{authentication::Credentials, config::SessionConfig, session::Session};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    NoLogin,
    NotPremium,
    Offline,
}
impl std::fmt::Display for Unavailable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Offline => "Spotify is temporarily unavailable",
            Self::NoLogin => "Spotify authorization is missing; run rspc login",
            Self::NotPremium => "This Spotify account needs Premium for playback",
        })
    }
}
impl std::error::Error for Unavailable {}
#[derive(Default)]
struct Manager {
    client: Option<Arc<Client>>,
    refusal: Option<(Unavailable, Instant)>,
}
impl Manager {
    fn ready(&self) -> Option<Arc<Client>> {
        self.client
            .as_ref()
            .filter(|client| !client.is_invalid())
            .cloned()
    }
    fn blocked(&self, now: Instant) -> Option<Unavailable> {
        self.refusal
            .filter(|(reason, at)| {
                *reason == Unavailable::NotPremium
                    || now.duration_since(*at) < Duration::from_secs(30)
            })
            .map(|(reason, _)| reason)
    }
    fn publish(&mut self, result: Result<Option<Client>>) -> Result<Arc<Client>, Unavailable> {
        self.client = None;
        self.refusal = None;
        match result {
            Ok(Some(client)) => {
                let client = Arc::new(client);
                self.client = Some(Arc::clone(&client));
                Ok(client)
            }
            Ok(None) => Err(Unavailable::NoLogin),
            Err(error) => {
                let reason = if error.is::<PremiumRequired>() {
                    Unavailable::NotPremium
                } else {
                    Unavailable::Offline
                };
                if reason == Unavailable::NotPremium {
                    let _ = login::forget();
                }
                self.refusal = Some((reason, Instant::now()));
                log::warn!("Spotify connection: {error:#}");
                Err(reason)
            }
        }
    }
}
fn manager() -> &'static Mutex<Manager> {
    static VALUE: OnceLock<Mutex<Manager>> = OnceLock::new();
    VALUE.get_or_init(|| Mutex::new(Manager::default()))
}
#[derive(Debug)]
struct PremiumRequired;
impl std::fmt::Display for PremiumRequired {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Spotify Premium is required")
    }
}
impl std::error::Error for PremiumRequired {}

#[cfg(test)]
pub(super) static ALLOW_IN_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
fn enabled() -> bool {
    #[cfg(test)]
    {
        ALLOW_IN_TESTS.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        true
    }
}
pub async fn current() -> Result<Arc<Client>, Unavailable> {
    if !enabled() {
        return Err(Unavailable::NoLogin);
    }
    let mut state = manager().lock().await;
    if let Some(client) = state.ready() {
        return Ok(client);
    }
    if let Some(reason) = state.blocked(Instant::now()) {
        return Err(reason);
    }
    state.publish(open_cached().await)
}
pub async fn log_in(credentials: Credentials) -> Result<(), Unavailable> {
    let mut state = manager().lock().await;
    state
        .publish(open_catalog(credentials).await.map(Some))
        .map(|_| ())
}
async fn open_cached() -> Result<Option<Client>> {
    if let Some(credentials) = login::cache()?.credentials() {
        let paired = super::auth::paired_username().await?;
        anyhow::ensure!(
            paired
                .as_deref()
                .is_none_or(|user| credentials.username.as_deref() == Some(user)),
            "The catalog and Soloist belong to different Spotify accounts"
        );
        return open_catalog(credentials).await.map(Some);
    }
    Ok(super::auth::SoloistAuth::load().await?.map(Client::paired))
}
async fn open_catalog(credentials: Credentials) -> Result<Client> {
    let native = Session::new(
        SessionConfig {
            proxy: login::proxy(),
            ..Default::default()
        },
        Some(login::cache()?),
    );
    native
        .spclient()
        .set_strategy(librespot_core::spclient::RequestStrategy::TryTimes(1));
    let ready = async {
        native.connect(credentials, true).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(kind) = native.get_user_attribute("type") {
                    return kind;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("Spotify did not report the account subscription")
    };
    let result = tokio::time::timeout(Duration::from_secs(18), ready)
        .await
        .context("Spotify connection timed out")
        .and_then(|result| result);
    match result {
        Ok(kind) if kind == "premium" => Ok(Client::new(native)),
        Ok(_) => {
            native.shutdown();
            Err(PremiumRequired.into())
        }
        Err(error) => {
            native.shutdown();
            Err(error)
        }
    }
}
/// Only a saved credential for the paired account may serve a restricted request.
pub(super) async fn catalog_fallback(username: &str) -> Result<Option<Session>> {
    let credential = login::cache()?
        .credentials()
        .filter(|credential| credential.username.as_deref() == Some(username));
    match credential {
        Some(credential) => Ok(open_catalog(credential).await?.catalog_session()),
        None => Ok(None),
    }
}
pub(crate) async fn catalog_for_pairing() -> Result<Session> {
    let mut state = manager().lock().await;
    if let Some(session) = state.ready().and_then(|client| client.catalog_session()) {
        return Ok(session);
    }
    let credential = login::cache()?
        .credentials()
        .context("Run rspc login to authorize Spotify")?;
    let client = state
        .publish(open_catalog(credential).await.map(Some))
        .map_err(anyhow::Error::from)?;
    client
        .catalog_session()
        .context("Catalog connection is unavailable")
}
pub(crate) async fn maintain() -> Result<()> {
    if let Ok(client) = current().await {
        client.prepare().await?;
    }
    Ok(())
}
pub(crate) fn description() -> &'static str {
    match manager().try_lock() {
        Err(_) => "connecting",
        Ok(state) => match state.ready() {
            None => "not_connected",
            Some(client) if client.is_paired() => "soloist",
            Some(_) => "catalog",
        },
    }
}
pub(crate) async fn check_pairing() -> Result<()> {
    let client = current().await.map_err(anyhow::Error::from)?;
    let paired = super::auth::paired_username().await?;
    anyhow::ensure!(
        paired.as_deref() == Some(client.username().as_str()),
        "Soloist and the catalog need a matching Spotify login"
    );
    client.prepare().await?;
    client.collection_page("collection", "", 1).await?;
    if client.catalog_session().is_some() {
        super::people::following(&client).await?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_backoff_expires_but_a_subscription_refusal_requires_login() {
        let mut state = Manager::default();
        let now = Instant::now();
        state.refusal = Some((Unavailable::Offline, now));
        assert_eq!(
            state.blocked(now + Duration::from_secs(1)),
            Some(Unavailable::Offline)
        );
        assert_eq!(state.blocked(now + Duration::from_secs(31)), None);
        state.refusal = Some((Unavailable::NotPremium, now));
        assert_eq!(
            state.blocked(now + Duration::from_secs(60)),
            Some(Unavailable::NotPremium)
        );
    }
    #[test]
    fn no_credentials_does_not_poison_a_future_login() {
        let mut state = Manager::default();
        assert_eq!(state.publish(Ok(None)).err(), Some(Unavailable::NoLogin));
        assert!(state.blocked(Instant::now()).is_none());
    }
}
