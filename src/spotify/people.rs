//! Listener profiles are separate from music artists and saved collections.
use super::{client::Client, request};
use anyhow::{Context, Result};
use serde_json::{Value, json};
fn profile_path(username: &str) -> String {
    let escaped: String = url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
    format!("/user-profile-view/v3/profile/{escaped}")
}
async fn fetch(client: &Client, path: &str) -> Result<Value> {
    let body = request::read(|| client.get(path, "application/json"))
        .await
        .map_err(request::upstream)?;
    Ok(serde_json::from_slice(&body)?)
}
pub async fn following(client: &Client) -> Result<Value> {
    let result = fetch(
        client,
        &format!("{}/following", profile_path(&client.username())),
    )
    .await?;
    let profiles = result
        .get("profiles")
        .or_else(|| result.get("following"))
        .and_then(Value::as_array)
        .context("Following response has no profiles")?;
    let listeners: Vec<_> = profiles
        .iter()
        .filter(|profile| {
            profile["uri"]
                .as_str()
                .is_some_and(|uri| uri.starts_with("spotify:user:"))
        })
        .cloned()
        .collect();
    Ok(json!({"items":listeners}))
}
pub async fn profile(client: &Client, username: &str) -> Result<Value> {
    let path = profile_path(username);
    let first = fetch(
        client,
        &format!("{path}?playlist_limit=1000&artist_limit=0"),
    )
    .await?;
    let count = first["total_public_playlists"]
        .as_u64()
        .unwrap_or(0)
        .min(100_000);
    if count > 1000 {
        fetch(
            client,
            &format!("{path}?playlist_limit={count}&artist_limit=0"),
        )
        .await
    } else {
        Ok(first)
    }
}
