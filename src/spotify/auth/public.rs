//! Anonymous access for public search and profile playlists. Bootstrap data is
//! discovered from Spotify's own web player, without browser cookies or an
//! external token service. Fail closed when its public challenge changes.
use super::super::client::{Client, error_kind, read_body};
use super::{FAILURE_DELAY, Grant, RESPONSE_LIMIT};
use librespot_core::Error;
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, time::Instant};
use tokio::sync::Mutex;

const BOOTSTRAP_LIMIT: usize = 8 * 1024 * 1024;
const WEB_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub(super) struct PublicSession {
    state: Mutex<State>,
    cache: PathBuf,
}
#[derive(Default)]
struct State {
    grant: Option<Grant>,
    retry_at: Option<Instant>,
    disk_checked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebGrant {
    access_token: String,
    access_token_expiration_timestamp_ms: u64,
    is_anonymous: bool,
}
#[derive(Deserialize, Serialize)]
struct CachedGrant {
    access_token: String,
    expires_at_ms: u64,
}

impl PublicSession {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            cache: crate::paths::cache().join("spotify-public-session.json"),
        }
    }

    pub async fn token(&self, client: &Client) -> Result<String, Error> {
        let mut state = self.state.lock().await;
        if let Some(grant) = &state.grant
            && grant.fresh()
        {
            return Ok(grant.token.clone());
        }
        if state.retry_at.is_some_and(|at| Instant::now() < at) {
            return Err(Error::unavailable(
                "Public Spotify catalog login is waiting before retrying",
            ));
        }
        let check_disk = !state.disk_checked;
        state.disk_checked = true;
        if check_disk
            && let Ok(bytes) = super::store::private_file(&self.cache)
            && bytes.len() <= RESPONSE_LIMIT
            && let Ok(saved) = serde_json::from_slice::<CachedGrant>(&bytes)
            && let Some(grant) = saved.usable()
        {
            let token = grant.token.clone();
            state.grant = Some(grant);
            return Ok(token);
        }
        state.retry_at = Some(Instant::now() + FAILURE_DELAY);
        let saved = bootstrap(client).await?;
        let grant = saved
            .usable()
            .ok_or_else(|| Error::data_loss("Spotify issued an expired anonymous grant"))?;
        // Disk persistence is an optimization; an unwritable cache must not
        // invalidate the working in-memory session.
        if let Ok(bytes) = serde_json::to_vec(&saved)
            && crate::paths::write_private_file_atomic(&self.cache, &bytes).is_err()
        {
            log::warn!("Could not cache the anonymous Spotify session");
        }
        let token = grant.token.clone();
        state.grant = Some(grant);
        state.retry_at = None;
        Ok(token)
    }

    pub async fn invalidate(&self, rejected: &str) {
        let mut state = self.state.lock().await;
        if state
            .grant
            .as_ref()
            .is_some_and(|grant| grant.token == rejected)
        {
            state.grant = None;
            state.retry_at = None;
            state.disk_checked = true;
            let _ = std::fs::remove_file(&self.cache);
        }
    }
}

impl CachedGrant {
    fn usable(&self) -> Option<Grant> {
        let remaining = self
            .expires_at_ms
            .checked_sub(chrono::Utc::now().timestamp_millis().max(0) as u64)?
            / 1000;
        (remaining > 60 && !self.access_token.is_empty())
            .then(|| Grant::new(self.access_token.clone(), remaining.min(3600)))
    }
}

async fn text(client: &Client, url: &str, limit: usize) -> Result<String, Error> {
    let response = client
        .response(
            super::super::http()
                .get(url)
                .header("User-Agent", WEB_AGENT)
                .header("Accept-Encoding", "identity"),
        )
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::new(
            error_kind(status),
            format!("Spotify public catalog HTTP {status}"),
        ));
    }
    let bytes = read_body(response, limit).await?;
    decode_text(bytes, limit)
}

fn decode_text(bytes: Vec<u8>, limit: usize) -> Result<String, Error> {
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .take((limit + 1) as u64)
            .read_to_end(&mut decoded)
            .map_err(|_| Error::data_loss("Invalid compressed Spotify bootstrap"))?;
        if decoded.len() > limit {
            return Err(Error::data_loss("Spotify bootstrap exceeds its size limit"));
        }
        decoded
    } else {
        bytes
    };
    String::from_utf8(bytes).map_err(|_| Error::data_loss("Spotify bootstrap is not UTF-8"))
}

async fn bootstrap(client: &Client) -> Result<CachedGrant, Error> {
    let page = text(client, "https://open.spotify.com/", BOOTSTRAP_LIMIT).await?;
    let url = bundle_url(&page).ok_or_else(|| {
        Error::failed_precondition("Spotify changed its public catalog bootstrap")
    })?;
    let script = text(client, &url, BOOTSTRAP_LIMIT).await?;
    let (secret, version) = challenge(&script)?;
    let time = text(
        client,
        "https://open.spotify.com/api/server-time",
        RESPONSE_LIMIT,
    )
    .await?;
    #[derive(Deserialize)]
    struct Clock {
        #[serde(rename = "serverTime")]
        server_time: u64,
    }
    let clock: Clock = serde_json::from_str(&time)
        .map_err(|_| Error::data_loss("Invalid Spotify server clock"))?;
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let mut endpoint = url::Url::parse("https://open.spotify.com/api/token").unwrap();
    endpoint.query_pairs_mut().extend_pairs([
        ("reason", "init".to_owned()),
        ("productType", "web-player".to_owned()),
        ("totp", totp(&secret, now)?),
        ("totpServer", totp(&secret, clock.server_time)?),
        ("totpVer", version.to_string()),
    ]);
    let response = text(client, endpoint.as_str(), RESPONSE_LIMIT).await?;
    let grant: WebGrant = serde_json::from_str(&response)
        .map_err(|_| Error::data_loss("Invalid anonymous Spotify grant"))?;
    if !grant.is_anonymous {
        return Err(Error::failed_precondition(
            "Spotify public catalog returned a non-anonymous session",
        ));
    }
    Ok(CachedGrant {
        access_token: grant.access_token,
        expires_at_ms: grant.access_token_expiration_timestamp_ms,
    })
}

fn bundle_url(page: &str) -> Option<String> {
    let pattern = regex::Regex::new(r#"<script[^>]+src="(https://open\.spotifycdn\.com/cdn/build/(?:web-player/web-player|mobile-web-player/mobile-web-player)\.[a-zA-Z0-9]+\.js)""#).ok()?;
    Some(pattern.captures(page)?.get(1)?.as_str().to_owned())
}

fn challenge(script: &str) -> Result<(Vec<u8>, u32), Error> {
    let pattern =
        regex::Regex::new(r#"\{secret:("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'),version:(\d+)\}"#)
            .unwrap();
    let found = pattern.captures(script).ok_or_else(|| {
        Error::failed_precondition("Spotify changed its public catalog challenge")
    })?;
    let literal = found.get(1).unwrap().as_str();
    let secret = js_string(literal)?;
    let key: String = secret
        .encode_utf16()
        .enumerate()
        .map(|(i, c)| (u32::from(c) ^ ((i % 33) as u32 + 9)).to_string())
        .collect();
    let version = found[2]
        .parse::<u32>()
        .map_err(|_| Error::data_loss("Invalid Spotify challenge version"))?;
    if secret.is_empty() || secret.len() > 4096 || version == 0 {
        return Err(Error::data_loss("Invalid Spotify challenge"));
    }
    Ok((key.into_bytes(), version))
}

fn js_string(literal: &str) -> Result<String, Error> {
    // Convert a single-quoted JS string to JSON without evaluating JavaScript.
    if literal.starts_with('"') {
        return serde_json::from_str(literal)
            .map_err(|_| Error::data_loss("Invalid Spotify challenge string"));
    }
    let mut json = String::from("\"");
    let mut chars = literal[1..literal.len() - 1].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\'') => json.push('\''),
                Some(c) => {
                    json.push('\\');
                    json.push(c);
                }
                None => return Err(Error::data_loss("Invalid Spotify challenge escape")),
            },
            '"' => json.push_str("\\\""),
            c => json.push(c),
        }
    }
    json.push('"');
    serde_json::from_str(&json).map_err(|_| Error::data_loss("Invalid Spotify challenge string"))
}

fn totp(secret: &[u8], time: u64) -> Result<String, Error> {
    let key = PKey::hmac(secret).map_err(Error::failed_precondition)?;
    let mut signer =
        Signer::new(MessageDigest::sha1(), &key).map_err(Error::failed_precondition)?;
    signer
        .update(&(time / 30).to_be_bytes())
        .map_err(Error::failed_precondition)?;
    let hash = signer.sign_to_vec().map_err(Error::failed_precondition)?;
    let offset = usize::from(hash[19] & 15);
    let value = u32::from_be_bytes(hash[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
    Ok(format!("{:06}", value % 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn totp_matches_the_rfc_6238_sha1_vector() {
        assert_eq!(totp(b"12345678901234567890", 59).unwrap(), "287082");
    }
    #[test]
    fn bootstrap_only_accepts_spotify_cdn_script_urls() {
        assert!(bundle_url(r#"<script src="https://example.com/cdn/build/web-player/web-player.abc.js"></script>"#).is_none());
        assert!(bundle_url(r#"<script src="https://open.spotifycdn.com/cdn/build/web-player/web-player.abc.js"></script>"#).is_some());
    }
    #[test]
    fn challenge_strings_are_decoded_without_executing_code() {
        assert_eq!(js_string(r#"'a\'b"c\\d'"#).unwrap(), "a'b\"c\\d");
        assert!(challenge("secret: runCode(),version:1").is_err());
        let (secret, version) = challenge("[{secret:'ABC',version:12}]").unwrap();
        assert_eq!(version, 12);
        assert_eq!(secret, b"727272");
    }
    #[test]
    fn decompression_is_bounded() {
        use std::io::Write;
        let mut zip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        zip.write_all(&[b'x'; 1000]).unwrap();
        assert!(decode_text(zip.finish().unwrap(), 50).is_err());
    }
    #[test]
    fn expired_public_grants_are_not_reused() {
        assert!(
            CachedGrant {
                access_token: "example".into(),
                expires_at_ms: 1
            }
            .usable()
            .is_none()
        );
    }

    #[tokio::test]
    async fn a_late_rejection_does_not_discard_a_newer_public_grant() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("public.json");
        std::fs::write(&cache, b"fixture").unwrap();
        let public = PublicSession {
            state: Mutex::new(State {
                grant: Some(Grant::new("new-grant".into(), 3600)),
                disk_checked: true,
                ..Default::default()
            }),
            cache,
        };
        public.invalidate("old-grant").await;
        assert!(public.state.lock().await.grant.is_some());
        assert!(public.cache.exists());
        public.invalidate("new-grant").await;
        assert!(public.state.lock().await.grant.is_none());
        assert!(!public.cache.exists());
    }
}
