//! Optional lyric fetching and LRC export, independent of the displayed layout.
use super::{client::Client, request};
use anyhow::{Context, Result};
use librespot_core::{SpotifyId, error::ErrorKind};
use serde::Deserialize;

#[derive(Deserialize)]
struct Response {
    lyrics: Text,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Text {
    sync_type: String,
    #[serde(default)]
    lines: Vec<Line>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    start_time_ms: String,
    words: String,
}
fn clean(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}
fn lrc(payload: &[u8], track: &crate::model::TrackInfo) -> Result<Option<String>> {
    let response: Response = serde_json::from_slice(payload)?;
    if !response
        .lyrics
        .lines
        .iter()
        .any(|line| !line.words.trim().is_empty())
    {
        return Ok(None);
    }
    let mut output = format!(
        "[ti:{}]\n[ar:{}]\n[al:{}]\n",
        clean(&track.name),
        clean(&track.artists.join(", ")),
        clean(&track.album)
    );
    let synced = response.lyrics.sync_type == "LINE_SYNCED";
    let mut text_written = false;
    for line in response.lyrics.lines {
        let time = if synced {
            match line.start_time_ms.parse::<u64>() {
                Ok(time) => time,
                Err(_) => continue,
            }
        } else {
            0
        };
        text_written |= !line.words.trim().is_empty();
        output.push_str(&format!(
            "[{:02}:{:02}.{:03}]{}\n",
            time / 60000,
            time / 1000 % 60,
            time % 1000,
            clean(&line.words)
        ));
    }
    Ok(text_written.then_some(output))
}
pub async fn cache(client: &Client, track: &crate::model::TrackInfo) -> Result<()> {
    let id = track
        .uri
        .as_deref()
        .and_then(|uri| uri.strip_prefix("spotify:track:"))
        .context("Lyrics need a Spotify track")?;
    let id = SpotifyId::from_base62(id)?.to_base62()?;
    let path = crate::paths::cache()
        .join("lyrics")
        .join(format!("spotify:track:{id}.lrc"));
    if path.is_file() {
        return Ok(());
    }
    let endpoint = format!("/color-lyrics/v2/track/{id}");
    let payload = match request::read(|| client.get(&endpoint, "application/json")).await {
        Ok(payload) => payload,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(request::upstream(error)),
    };
    if let Some(text) = lrc(&payload, track)? {
        tokio::task::spawn_blocking(move || {
            crate::paths::write_private_file_atomic(&path, text.as_bytes())
        })
        .await??;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untimed_text_uses_zero_and_invalid_timed_text_is_not_cached() {
        let track = crate::model::TrackInfo::default();
        let raw = br#"{"lyrics":{"syncType":"UNSYNCED","lines":[{"startTimeMs":"bad","words":"words"}]}}"#;
        assert!(
            lrc(raw, &track)
                .unwrap()
                .unwrap()
                .contains("[00:00.000]words")
        );
        let raw = br#"{"lyrics":{"syncType":"LINE_SYNCED","lines":[{"startTimeMs":"bad","words":"words"}]}}"#;
        assert!(lrc(raw, &track).unwrap().is_none());
    }
    #[test]
    fn timed_text_preserves_empty_gaps_and_rejects_malformed_timestamps() {
        let track = crate::model::TrackInfo {
            name: "Fixture".into(),
            ..Default::default()
        };
        let payload = br#"{"lyrics":{"syncType":"LINE_SYNCED","lines":[{"startTimeMs":"1500","words":"first"},{"startTimeMs":"2500","words":""},{"startTimeMs":"bad","words":"bad"}]}}"#;
        let value = lrc(payload, &track).unwrap().unwrap();
        assert!(value.contains("[00:01.500]first\n[00:02.500]\n"));
        assert!(!value.contains("bad"));
        assert!(
            lrc(br#"{"lyrics":{"syncType":"UNSYNCED","lines":[]}}"#, &track)
                .unwrap()
                .is_none()
        );
    }
}
