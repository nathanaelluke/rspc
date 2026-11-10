//! Library access for the manual phone-pairing alternative. Normal sign-in uses
//! a catalog session and derives Soloist's authorization through `connect_auth`.
mod public;
mod store;

use super::{
    client::{Client, confidential, error_kind, read_body},
    protocol::soloist_session as proto,
};
use librespot_core::{Error, session::Session};
use protobuf::{Message, MessageField};
use serde::Deserialize;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const CLIENT_ID: &str = "334d00fed24f4e7b9abbe02ee28e0b5f";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const FAILURE_DELAY: Duration = Duration::from_secs(30);

pub(super) enum Authentication {
    Catalog(Session),
    Soloist(Box<SoloistAuth>),
}

pub(super) struct RequestAuth {
    pub access: String,
    pub client: Option<String>,
    pub public: bool,
}

struct Grant {
    token: String,
    until: Instant,
}
impl Grant {
    fn new(token: String, seconds: u64) -> Self {
        Self {
            token,
            until: Instant::now()
                + Duration::from_secs(seconds.min(14 * 24 * 3600).saturating_sub(30)),
        }
    }
    fn fresh(&self) -> bool {
        Instant::now() < self.until
    }
}

#[derive(Default)]
struct PrivateState {
    client: Option<Grant>,
    access: Option<Grant>,
    retry_at: Option<Instant>,
}

pub(super) struct SoloistAuth {
    data: PathBuf,
    username: String,
    version: String,
    private: Mutex<PrivateState>,
    public: public::PublicSession,
}

pub(crate) fn available() -> bool {
    crate::paths::state().join("soloist/cache/dbrts").is_file()
}

pub(super) async fn paired_username() -> anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(cached_username).await?
}

pub(super) fn cached_username() -> anyhow::Result<Option<String>> {
    store::read_username(&crate::paths::state().join("soloist"))
}

impl SoloistAuth {
    pub async fn load() -> anyhow::Result<Option<Self>> {
        let data = crate::paths::state().join("soloist");
        let source = data.clone();
        let Some(stored) = tokio::task::spawn_blocking(move || store::read(&source)).await?? else {
            return Ok(None);
        };
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::process::Command::new(crate::soloist::binary())
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        anyhow::ensure!(output.status.success(), "Cannot query Soloist's version");
        let text = std::str::from_utf8(&output.stdout)?;
        let version = regex::Regex::new(r"(?i)soloist\s+(\d+(?:\.\d+){2,3})")?
            .captures(text)
            .and_then(|m| m.get(1))
            .map(|m| m.as_str().to_owned())
            .ok_or_else(|| anyhow::anyhow!("Cannot identify the installed Soloist version"))?;
        Ok(Some(Self {
            data,
            username: stored.username.clone(),
            version,
            private: Mutex::new(PrivateState::default()),
            public: public::PublicSession::new(),
        }))
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub async fn public(&self, client: &Client) -> Result<RequestAuth, Error> {
        Ok(RequestAuth {
            access: self.public.token(client).await?,
            client: None,
            public: true,
        })
    }

    pub async fn invalidate_public(&self, rejected: &str) {
        self.public.invalidate(rejected).await;
    }

    pub async fn private(&self, http: &Client) -> Result<RequestAuth, Error> {
        let mut state = self.private.lock().await;
        if let (Some(access), Some(client)) = (&state.access, &state.client)
            && access.fresh()
            && client.fresh()
        {
            return Ok(RequestAuth {
                access: access.token.clone(),
                client: Some(client.token.clone()),
                public: false,
            });
        }
        if state.retry_at.is_some_and(|at| Instant::now() < at) {
            return Err(Error::unavailable(
                "Soloist login renewal is waiting before retrying",
            ));
        }
        state.retry_at = Some(Instant::now() + FAILURE_DELAY);
        let result = self.renew(http, &mut state, TOKEN_URL).await;
        if result.is_ok() {
            state.retry_at = None;
        }
        result
    }

    async fn renew(
        &self,
        http: &Client,
        state: &mut PrivateState,
        endpoint: &str,
    ) -> Result<RequestAuth, Error> {
        let data = self.data.clone();
        let stored = tokio::task::spawn_blocking(move || store::read(&data))
            .await
            .map_err(Error::unavailable)?
            .map_err(Error::failed_precondition)?
            .ok_or_else(|| Error::unauthenticated("Soloist needs pairing; run rspc login"))?;
        if stored.username != self.username {
            return Err(Error::unauthenticated(
                "Spotify account changed; restart rspc's daemon",
            ));
        }
        if !state.client.as_ref().is_some_and(Grant::fresh) {
            state.client = Some(self.client_token(http).await?);
        }
        let client = &state.client.as_ref().unwrap().token;
        let mut nonce = stored.nonce.clone();
        for attempt in 0..2 {
            let request = super::http()
                .post(endpoint)
                .header("client-token", confidential(client)?)
                .header(
                    "User-Agent",
                    format!("Spotify/{} Soloist/0 (Soloist)", self.version),
                )
                .header(
                    "DPoP",
                    confidential(
                        &stored
                            .proof(endpoint, &nonce)
                            .map_err(Error::failed_precondition)?,
                    )?,
                )
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", stored.refresh.as_str()),
                ]);
            let response = http.response(request).await?;
            let status = response.status();
            let next_nonce = response
                .headers()
                .get("DPoP-Nonce")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = read_body(response, RESPONSE_LIMIT).await?;
            if !status.is_success() {
                let challenge = serde_json::from_slice::<OAuthFailure>(&body)
                    .ok()
                    .is_some_and(|e| e.error == "use_dpop_nonce");
                if challenge
                    && attempt == 0
                    && let Some(value) = next_nonce
                {
                    nonce = value;
                    continue;
                }
                return Err(Error::new(
                    error_kind(status),
                    format!("Spotify login renewal HTTP {status}; run rspc doctor"),
                ));
            }
            let grant: AccessGrant = serde_json::from_slice(&body)
                .map_err(|_| Error::data_loss("Invalid Spotify login renewal response"))?;
            if grant
                .refresh_token
                .as_ref()
                .is_some_and(|value| value != stored.refresh.as_str())
            {
                // Preserve an unexpected rotated grant privately for recovery;
                // never rewrite the running player's credential store.
                crate::paths::write_private_file_atomic(
                    &self.data.with_file_name("soloist-refresh-recovery.json"),
                    &body,
                )
                .map_err(|_| Error::unavailable("Cannot save rotated Spotify credentials"))?;
                return Err(Error::failed_precondition(
                    "Spotify rotated Soloist's refresh token; recovery credentials saved; run rspc login --repair",
                ));
            }
            if grant.access_token.is_empty()
                || !grant.token_type.eq_ignore_ascii_case("bearer")
                || grant.expires_in <= 30
            {
                return Err(Error::data_loss("Unsupported Spotify access grant"));
            }
            state.access = Some(Grant::new(grant.access_token.clone(), grant.expires_in));
            return Ok(RequestAuth {
                access: grant.access_token,
                client: Some(client.clone()),
                public: false,
            });
        }
        Err(Error::unauthenticated("Spotify refused the login nonce"))
    }

    async fn client_token(&self, http: &Client) -> Result<Grant, Error> {
        let device = std::fs::read_to_string(self.data.join(".device_id"))
            .map_err(|_| Error::failed_precondition("Cannot read Soloist device identifier"))?;
        let key = crate::soloist::api_key().map_err(Error::failed_precondition)?;
        let linux = proto::LinuxData {
            system_name: "Linux".into(),
            system_release: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .into(),
            system_version: std::env::consts::OS.into(),
            hardware: std::env::consts::ARCH.into(),
            ..Default::default()
        };
        let speaker = proto::SpeakerData {
            brand_name: "Soloist".into(),
            model_name: "Soloist".into(),
            sdk_version: self.version.clone(),
            ..Default::default()
        };
        let data = proto::ConnectivityData {
            device_id: device.trim().into(),
            developer_api_key: key,
            platform_specific_data: MessageField::some(proto::PlatformData {
                desktop_linux: MessageField::some(linux),
                ..Default::default()
            }),
            application_specific_data: MessageField::some(proto::ApplicationData {
                core_speaker: MessageField::some(speaker),
                ..Default::default()
            }),
            ..Default::default()
        };
        let request = proto::ClientTokenRequest {
            request_type: 1,
            client_data: MessageField::some(proto::ClientData {
                client_id: CLIENT_ID.into(),
                client_version: self.version.clone(),
                connectivity_sdk_data: MessageField::some(data),
                ..Default::default()
            }),
            ..Default::default()
        };
        let response = http
            .response(
                super::http()
                    .post("https://clienttoken.spotify.com/v1/clienttoken")
                    .header("Accept", "application/x-protobuf")
                    .header("Content-Type", "application/x-protobuf")
                    .body(request.write_to_bytes()?),
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::new(
                error_kind(status),
                format!("Spotify client-token HTTP {status}"),
            ));
        }
        let response = proto::ClientTokenResponse::parse_from_bytes(
            &read_body(response, RESPONSE_LIMIT).await?,
        )
        .map_err(|_| Error::data_loss("Invalid Soloist client-token response"))?;
        let grant = response.granted_token.as_ref().ok_or_else(|| {
            Error::failed_precondition("Spotify did not grant a Soloist client token")
        })?;
        if response.response_type != 1
            || grant.token.is_empty()
            || grant.refresh_after_seconds <= 30
        {
            return Err(Error::failed_precondition(
                "Unsupported Soloist client-token challenge",
            ));
        }
        Ok(Grant::new(
            grant.token.clone(),
            u64::from(grant.refresh_after_seconds.min(grant.expires_after_seconds)),
        ))
    }
}

#[derive(Deserialize)]
struct OAuthFailure {
    error: String,
}
#[derive(Deserialize)]
struct AccessGrant {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn fixture(data: PathBuf) -> SoloistAuth {
        store::write_fixture(&data).unwrap();
        SoloistAuth {
            data,
            username: "test-user".into(),
            version: "1.0.0".into(),
            private: Mutex::new(PrivateState::default()),
            public: public::PublicSession::new(),
        }
    }

    fn client_grant() -> PrivateState {
        PrivateState {
            client: Some(Grant::new("unit-client-token".into(), 3600)),
            ..Default::default()
        }
    }

    async fn server(
        replies: Vec<(u16, &'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut nonces = Vec::new();
            for (code, headers, body) in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.trim().split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                        if key.eq_ignore_ascii_case("dpop") {
                            let payload = value.trim().split('.').nth(1).unwrap();
                            let claims: serde_json::Value =
                                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap())
                                    .unwrap();
                            nonces.push(claims["nonce"].as_str().unwrap().to_owned());
                        }
                    }
                }
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).await.unwrap();
                reader.get_mut().write_all(format!("HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            nonces
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn nonce_challenge_gets_one_fresh_signed_request() {
        let temp = tempfile::tempdir().unwrap();
        let auth = fixture(temp.path().join("soloist"));
        let http = Client::new(Session::new(Default::default(), None));
        let (endpoint, task) = server(vec![
            (
                400,
                "DPoP-Nonce: server-nonce\r\n",
                r#"{"error":"use_dpop_nonce"}"#,
            ),
            (
                200,
                "",
                r#"{"access_token":"unit-access","token_type":"Bearer","expires_in":3600}"#,
            ),
        ])
        .await;
        let credentials = auth
            .renew(&http, &mut client_grant(), &endpoint)
            .await
            .unwrap();
        assert_eq!(credentials.access, "unit-access");
        assert_eq!(task.await.unwrap(), ["initial-nonce", "server-nonce"]);
    }

    #[tokio::test]
    async fn generic_authentication_failures_are_not_retried() {
        let temp = tempfile::tempdir().unwrap();
        let auth = fixture(temp.path().join("soloist"));
        let http = Client::new(Session::new(Default::default(), None));
        let (endpoint, task) = server(vec![(
            400,
            "DPoP-Nonce: irrelevant\r\n",
            r#"{"error":"invalid_grant"}"#,
        )])
        .await;
        let error = auth
            .renew(&http, &mut client_grant(), &endpoint)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.kind,
            librespot_core::error::ErrorKind::InvalidArgument
        );
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unexpected_rotation_preserves_recovery_without_rewriting_the_player() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let auth = fixture(temp.path().join("soloist"));
        let original = std::fs::read(auth.data.join("cache/dbrts")).unwrap();
        let http = Client::new(Session::new(Default::default(), None));
        let (endpoint, task) = server(vec![(200, "", r#"{"access_token":"unit-access","token_type":"Bearer","expires_in":3600,"refresh_token":"unit-rotated"}"#)]).await;
        let error = auth
            .renew(&http, &mut client_grant(), &endpoint)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.kind,
            librespot_core::error::ErrorKind::FailedPrecondition
        );
        task.await.unwrap();
        assert_eq!(
            std::fs::read(auth.data.join("cache/dbrts")).unwrap(),
            original
        );
        let recovery = temp.path().join("soloist-refresh-recovery.json");
        assert!(recovery.is_file());
        assert_eq!(
            recovery.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
