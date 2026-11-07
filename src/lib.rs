mod config;
mod daemon;
mod model;
pub mod paths;
mod playback;
mod soloist;
mod spotify;
pub use daemon::{DaemonOptions, run_daemon};
pub fn credentials_available() -> anyhow::Result<bool> {
    spotify::credentials::available()
}
pub async fn login() -> anyhow::Result<()> {
    let login = spotify::credentials::begin_login().await?;
    println!("Open {} and enter {}", login.url(), login.code());
    let credentials = login.finish().await?;
    spotify::session::log_in(credentials)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    println!("Spotify login saved.");
    Ok(())
}
