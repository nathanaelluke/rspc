use super::{
    router::{retry_read, upstream_error},
    session::SpotifySession,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub async fn following(session: &SpotifySession) -> Result<Value> {
    let username = session.username();
    let username: String = url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
    let spclient = session.session().spclient();
    let result = retry_read(|| spclient.get_user_following(&username))
        .await
        .map_err(upstream_error)?;
    let result: Value = serde_json::from_slice(&result)?;
    let items = result["profiles"]
        .as_array()
        .or_else(|| result["following"].as_array())
        .context("Spotify did not return followed profiles")?;
    Ok(
        json!({"items":items.iter().filter(|item|item["uri"].as_str().is_some_and(|uri|uri.starts_with("spotify:user:"))).cloned().collect::<Vec<_>>() }),
    )
}

pub async fn profile(session: &SpotifySession, username: &str) -> Result<Value> {
    let spclient = session.session().spclient();
    let first = retry_read(|| spclient.get_user_profile(username, Some(1000), Some(0)))
        .await
        .map_err(upstream_error)?;
    let first: Value = serde_json::from_slice(&first)?;
    let total = first["total_public_playlists"]
        .as_u64()
        .unwrap_or(0)
        .min(100_000) as u32;
    if total > 1000 {
        let result = retry_read(|| spclient.get_user_profile(username, Some(total), Some(0)))
            .await
            .map_err(upstream_error)?;
        return Ok(serde_json::from_slice(&result)?);
    }
    Ok(first)
}
