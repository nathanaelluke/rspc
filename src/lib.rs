mod config;
mod daemon;
mod model;
pub mod paths;
mod playback;
mod soloist;
mod spotify;
pub use daemon::{DaemonOptions, run_daemon};
/// Identify the owned player's Connect socket without inspecting its command line.
pub fn pairing_port(pid: u32) -> anyhow::Result<u16> {
    soloist::discovery::port(pid)
}
pub fn credentials_available() -> anyhow::Result<bool> {
    spotify::login::available()
}
pub async fn catalog_login() -> anyhow::Result<()> {
    let login = spotify::login::begin_login().await?;
    println!("Open {} and enter {}", login.url(), login.code());
    let credentials = login.finish().await?;
    spotify::connection::log_in(credentials)
        .await
        .map_err(anyhow::Error::from)?;
    println!("Spotify login saved.");
    Ok(())
}
/// Ensure catalog credentials and Soloist prerequisites are available.
pub async fn ensure_catalog_login() -> anyhow::Result<()> {
    anyhow::ensure!(
        soloist::binary().is_file(),
        "Soloist is missing; install spotify-soloist-bin (AUR), spotify-soloist (Homebrew cask), or use the rspc Nix package. See README.md for source installs."
    );
    let _ = soloist::api_key()?;
    if !spotify::login::catalog_available()? {
        catalog_login().await?;
    }
    Ok(())
}
