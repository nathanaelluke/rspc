//! Authorize Soloist from the catalog session through Spotify's Connect exchange.
//! The one-use code is bound to the receiver's DPoP key and PKCE challenge.
use super::{client::read_body, protocol::soloist_session as proto};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use librespot_core::session::Session;
use protobuf::Message;
use serde::Deserialize;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const CODE_TYPE: &str = "urn:spotify:params:oauth:authorization_code";
const RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Deserialize)]
struct Receiver {
    status: u16,
    #[serde(rename = "deviceID")]
    device_id: String,
    #[serde(rename = "activeUser", default)]
    active_user: String,
    #[serde(rename = "authInfo")]
    #[serde(default)]
    auth_info: String,
}

impl Receiver {
    fn check_account(&self, username: &str) -> Result<()> {
        ensure!(
            self.active_user.is_empty() || self.active_user == username,
            "Soloist and the catalog are signed in to different Spotify accounts"
        );
        Ok(())
    }
    fn authorization(&self, device_id: &str) -> Result<proto::AuthInfo> {
        ensure!(
            self.status == 101 && self.device_id == device_id,
            "Unexpected Soloist receiver"
        );
        ensure!(
            self.auth_info.len() <= 16 * 1024,
            "Soloist authorization info is too large"
        );
        let info = proto::AuthInfo::parse_from_bytes(&STANDARD.decode(&self.auth_info)?)
            .context("Unsupported Soloist authorization info")?;
        ensure!(
            uuid::Uuid::parse_str(&info.id).is_ok(),
            "Invalid Soloist authorization identifier"
        );
        ensure!(
            !info.client_id.is_empty() && info.client_id.len() <= 128,
            "Invalid receiver client identifier"
        );
        let method = info
            .method
            .as_ref()
            .and_then(|m| m.dpop_bound_auth_code.as_ref())
            .context("Soloist does not support key-bound Connect authorization")?;
        for value in [&method.jkt, &method.pkce_code_challenge] {
            ensure!(
                value.len() == 43
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "Invalid Soloist authorization challenge"
            );
        }
        Ok(info)
    }
}

#[derive(Deserialize)]
struct CodeGrant {
    access_token: String,
    issued_token_type: String,
}
impl Drop for CodeGrant {
    fn drop(&mut self) {
        self.access_token.zeroize();
    }
}

/// The endpoint comes from the owned player's listening socket, never a URL
/// supplied by the user or a remote device advertisement.
pub(crate) async fn authorize(
    session: &Session,
    port: u16,
    device_id: &str,
    authenticated: bool,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()?;
    let endpoint = format!("http://127.0.0.1:{port}/zc");
    let response = http
        .get(&endpoint)
        .query(&[("action", "getInfo")])
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Cannot read Soloist authorization info"
    );
    let bytes = read_body(response, RESPONSE_LIMIT).await?;
    let receiver: Receiver =
        serde_json::from_slice(&bytes).context("Invalid Soloist receiver response")?;
    ensure!(
        receiver.device_id == device_id && receiver.status == 101,
        "Unexpected Soloist receiver"
    );
    let username = session.username();
    receiver.check_account(&username)?;
    if authenticated {
        return Ok(());
    }
    let info = receiver.authorization(device_id)?;
    let access = Zeroizing::new(session.login5().auth_token().await?.access_token);
    let client_token = session.spclient().client_token().await?;
    let blob = exchange(
        super::http(),
        TOKEN_URL,
        &session.client_id(),
        &access,
        &client_token,
        &info,
    )
    .await?;
    let response = http
        .post(&endpoint)
        .form(&[
            ("action", "addUser"),
            ("version", "2.10.0"),
            ("tokenType", "auth_token"),
            ("clientKey", ""),
            ("loginId", username.as_str()),
            ("userName", username.as_str()),
            ("blob", blob.as_str()),
            ("deviceName", "rspc"),
        ])
        .send()
        .await
        .context("Sending Soloist's Connect authorization")?;
    ensure!(
        response.status().is_success(),
        "Soloist refused Connect authorization (HTTP {})",
        response.status()
    );
    let bytes = read_body(response, RESPONSE_LIMIT).await?;
    let reply: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        reply["status"] == 101 && reply["spotifyError"].as_u64().unwrap_or(0) == 0,
        "Soloist refused Connect authorization"
    );
    Ok(())
}

async fn exchange(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    access: &str,
    client_token: &str,
    info: &proto::AuthInfo,
) -> Result<Zeroizing<String>> {
    let method = info
        .method
        .as_ref()
        .and_then(|m| m.dpop_bound_auth_code.as_ref())
        .context("Missing Connect authorization method")?;
    let response = http
        .post(token_url)
        .header("client-token", super::client::confidential(client_token)?)
        .form(&[
            ("client_id", client_id),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            ("resource", "urn:spotify:resources:connect"),
            ("audience", info.client_id.as_str()),
            ("subject_token", access),
            (
                "subject_token_type",
                "urn:ietf:params:oauth:token-type:access_token",
            ),
            ("requested_token_type", CODE_TYPE),
            ("scope", "streaming"),
            ("dpop_jkt", method.jkt.as_str()),
            ("code_challenge", method.pkce_code_challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .context("Requesting Soloist's Connect authorization")?;
    // Do not include the response body: it may echo a credential on failure.
    ensure!(
        response.status().is_success(),
        "Spotify refused Connect authorization (HTTP {})",
        response.status()
    );
    let body = Zeroizing::new(read_body(response, RESPONSE_LIMIT).await?);
    let grant: CodeGrant =
        serde_json::from_slice(&body).context("Invalid Connect authorization response")?;
    ensure!(
        grant.issued_token_type == CODE_TYPE
            && !grant.access_token.is_empty()
            && grant.access_token.len() <= 16 * 1024,
        "Spotify returned an unsupported Connect credential"
    );
    let mut token = proto::AuthToken {
        auth_info_id: info.id.clone(),
        token: Some(proto::auth_token::Token::DpopBoundAuthCode(
            proto::DpopBoundAuthCode {
                value: grant.access_token.clone(),
                ..Default::default()
            },
        )),
        ..Default::default()
    };
    let encoded = Zeroizing::new(token.write_to_bytes()?);
    if let Some(proto::auth_token::Token::DpopBoundAuthCode(code)) = &mut token.token {
        code.value.zeroize();
    }
    Ok(Zeroizing::new(STANDARD.encode(&*encoded)))
}

#[cfg(test)]
mod tests;
