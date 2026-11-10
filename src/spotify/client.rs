//! Spotify catalog transport. Reads, edits and search share one authenticated
//! session, HTTP pool, response limit and rate-limit deadline.
use super::auth::{Authentication, RequestAuth, SoloistAuth};
use super::protocol::collection::{PageRequest, PageResponse};
use librespot_core::{Error, SpotifyId, error::ErrorKind, session::Session};
use librespot_protocol::playlist4_external::{ListChanges, SelectedListContent};
use protobuf::Message;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const COLLECTION_TYPE: &str = "application/vnd.collection-v2.spotify.proto";
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

pub struct Client {
    #[cfg(test)]
    fixture_endpoint: Option<String>,
    auth: Authentication,
    pub(super) entities: super::catalog::entities::EntityCache,
    pub(super) collections: super::catalog::collection::CollectionCache,
    pub(super) playlists: super::catalog::playlists::PlaylistCache,
    pub(super) search: super::catalog::search::SearchCache,
    fallback: Mutex<Fallback>,
    cooldown: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Fallback {
    session: Option<Session>,
    retry_at: Option<Instant>,
}

impl Client {
    #[cfg(test)]
    pub(super) fn fixture(endpoint: String) -> Self {
        let mut client = Self::new(Session::new(Default::default(), None));
        client.fixture_endpoint = Some(endpoint);
        client
    }
    pub fn new(session: Session) -> Self {
        Self {
            #[cfg(test)]
            fixture_endpoint: None,
            auth: Authentication::Catalog(session),
            entities: Default::default(),
            collections: Default::default(),
            playlists: Default::default(),
            search: Default::default(),
            fallback: Mutex::new(Fallback::default()),
            cooldown: Mutex::new(None),
        }
    }

    pub(super) fn paired(auth: SoloistAuth) -> Self {
        Self {
            #[cfg(test)]
            fixture_endpoint: None,
            auth: Authentication::Soloist(Box::new(auth)),
            entities: Default::default(),
            collections: Default::default(),
            playlists: Default::default(),
            search: Default::default(),
            fallback: Mutex::new(Fallback::default()),
            cooldown: Mutex::new(None),
        }
    }
    pub fn is_paired(&self) -> bool {
        matches!(self.auth, Authentication::Soloist(_))
    }
    pub fn is_invalid(&self) -> bool {
        matches!(&self.auth, Authentication::Catalog(session) if session.is_invalid())
    }
    pub fn catalog_session(&self) -> Option<Session> {
        match &self.auth {
            Authentication::Catalog(session) => Some(session.clone()),
            _ => None,
        }
    }
    pub fn username(&self) -> String {
        #[cfg(test)]
        if self.fixture_endpoint.is_some() {
            return "fixture-user".into();
        }
        match &self.auth {
            Authentication::Catalog(session) => session.username(),
            Authentication::Soloist(auth) => auth.username().to_owned(),
        }
    }
    async fn base_url(&self) -> Result<String, Error> {
        #[cfg(test)]
        if let Some(endpoint) = &self.fixture_endpoint {
            return Ok(endpoint.clone());
        }
        match &self.auth {
            Authentication::Catalog(session) => session.spclient().base_url().await,
            Authentication::Soloist(_) => Ok("https://spclient.wg.spotify.com".into()),
        }
    }
    pub async fn prepare(&self) -> Result<(), Error> {
        if let Authentication::Soloist(auth) = &self.auth {
            let (private, public) = tokio::join!(auth.private(self), auth.public(self));
            private?;
            public?;
        }
        Ok(())
    }

    pub async fn get(&self, path: &str, content_type: &str) -> Result<Vec<u8>, Error> {
        let base = self.base_url().await?;
        self.send(
            Method::GET,
            &format!("{base}{path}"),
            content_type,
            Vec::new(),
            false,
        )
        .await
    }

    pub async fn extended_metadata(
        &self,
        request: &librespot_protocol::extended_metadata::BatchedEntityRequest,
    ) -> Result<librespot_protocol::extended_metadata::BatchedExtensionResponse, Error> {
        let bytes = self
            .post(
                "/extended-metadata/v0/extended-metadata",
                "application/x-protobuf",
                request.write_to_bytes()?,
            )
            .await?;
        Ok(
            librespot_protocol::extended_metadata::BatchedExtensionResponse::parse_from_bytes(
                &bytes,
            )?,
        )
    }

    pub async fn collection_page(
        &self,
        set: &str,
        token: &str,
        limit: i32,
    ) -> Result<PageResponse, Error> {
        let request = PageRequest {
            username: self.username(),
            set: set.to_owned(),
            pagination_token: token.to_owned(),
            limit,
            ..Default::default()
        };
        let bytes = self
            .post(
                "/collection/v2/paging",
                COLLECTION_TYPE,
                request.write_to_bytes()?,
            )
            .await?;
        Ok(PageResponse::parse_from_bytes(&bytes)?)
    }

    pub async fn playlist_changes(
        &self,
        id: &SpotifyId,
        changes: &ListChanges,
    ) -> Result<SelectedListContent, Error> {
        self.list_changes(
            &format!("/playlist/v2/playlist/{}/changes", id.to_base62()?),
            changes,
        )
        .await
    }

    pub async fn rootlist_changes(
        &self,
        changes: &ListChanges,
    ) -> Result<SelectedListContent, Error> {
        let username: String =
            url::form_urlencoded::byte_serialize(self.username().as_bytes()).collect();
        self.list_changes(
            &format!("/playlist/v2/user/{username}/rootlist/changes"),
            changes,
        )
        .await
    }

    async fn list_changes(
        &self,
        path: &str,
        changes: &ListChanges,
    ) -> Result<SelectedListContent, Error> {
        // Playlist-v2 uses this content type even though its body is protobuf.
        // No transport retry: an ambiguous response can otherwise duplicate edits.
        // Only an explicit authorization refusal may use the optional login.
        let bytes = self
            .post(
                path,
                "application/x-www-form-urlencoded",
                changes.write_to_bytes()?,
            )
            .await?;
        Ok(SelectedListContent::parse_from_bytes(&bytes)?)
    }

    async fn post(&self, path: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>, Error> {
        let base = self.base_url().await?;
        self.send(
            Method::POST,
            &format!("{base}{path}"),
            content_type,
            body,
            false,
        )
        .await
    }

    pub async fn search_query(
        &self,
        name: &str,
        hash: &str,
        variables: Value,
    ) -> Result<Value, Error> {
        let body = serde_json::to_vec(&json!({
            "operationName": name, "variables": variables,
            "extensions": {"persistedQuery": {"version": 1, "sha256Hash": hash}},
        }))?;
        let endpoint = "https://api-partner.spotify.com/pathfinder/v2/query".to_owned();
        #[cfg(test)]
        let endpoint = self
            .fixture_endpoint
            .as_ref()
            .map_or(endpoint.clone(), |base| format!("{base}/pathfinder"));
        let bytes = self
            .send(
                Method::POST,
                &endpoint,
                "application/json;charset=UTF-8",
                body,
                true,
            )
            .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        search: bool,
    ) -> Result<Vec<u8>, Error> {
        self.check_cooldown().await?;
        let body = bytes::Bytes::from(body);
        let credentials = match &self.auth {
            #[cfg(test)]
            _ if self.fixture_endpoint.is_some() => RequestAuth {
                access: "fixture-access".into(),
                client: None,
                public: false,
            },
            Authentication::Catalog(session) => Self::catalog_auth(session).await?,
            Authentication::Soloist(auth) if search || public_profile(url) => {
                match auth.public(self).await {
                    Ok(credentials) => credentials,
                    Err(error) if error.kind != ErrorKind::ResourceExhausted => {
                        match self.fallback().await? {
                            Some(session) => Self::catalog_auth(&session).await?,
                            None => return Err(error),
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            Authentication::Soloist(_) if following_list(url) => {
                let session = self.fallback().await?.ok_or_else(|| Error::permission_denied("Spotify's following list needs an optional catalog login; playlist creators remain available"))?;
                Self::catalog_auth(&session).await?
            }
            Authentication::Soloist(auth) => auth.private(self).await?,
        };
        let mut result = self
            .send_authenticated(
                method.clone(),
                url,
                content_type,
                body.clone(),
                search,
                &credentials,
            )
            .await;
        if (search || public_profile(url))
            && credentials.public
            && matches!(&result, Err(error) if error.kind == ErrorKind::Unauthenticated)
            && let Authentication::Soloist(auth) = &self.auth
        {
            auth.invalidate_public(&credentials.access).await;
            let refreshed = auth.public(self).await?;
            result = self
                .send_authenticated(
                    method.clone(),
                    url,
                    content_type,
                    body.clone(),
                    search,
                    &refreshed,
                )
                .await;
        }
        // A 403 is a definite authorization refusal. Never repeat an ambiguous
        // write (timeouts, disconnects or server failures) with another grant.
        if matches!(&result, Err(error) if error.kind == ErrorKind::PermissionDenied)
            && self.is_paired()
            && !following_list(url)
            && let Some(session) = self.fallback().await?
        {
            let credentials = Self::catalog_auth(&session).await?;
            return self
                .send_authenticated(method, url, content_type, body, search, &credentials)
                .await;
        }
        result
    }

    async fn catalog_auth(session: &Session) -> Result<RequestAuth, Error> {
        Ok(RequestAuth {
            access: session.login5().auth_token().await?.access_token,
            client: session.spclient().client_token().await.ok(),
            public: false,
        })
    }

    async fn fallback(&self) -> Result<Option<Session>, Error> {
        let mut cache = self.fallback.lock().await;
        if let Some(session) = &cache.session
            && !session.is_invalid()
        {
            return Ok(Some(session.clone()));
        }
        if cache.retry_at.is_some_and(|at| Instant::now() < at) {
            return Ok(None);
        }
        cache.retry_at = Some(Instant::now() + Duration::from_secs(30));
        cache.session = super::connection::catalog_fallback(&self.username())
            .await
            .map_err(|_| Error::unavailable("Optional catalog login could not connect"))?;
        Ok(cache.session.clone())
    }

    async fn send_authenticated(
        &self,
        method: Method,
        url: &str,
        content_type: &str,
        body: bytes::Bytes,
        search: bool,
        auth: &RequestAuth,
    ) -> Result<Vec<u8>, Error> {
        let mut request = super::http()
            .request(method, url)
            .bearer_auth(&auth.access)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::ACCEPT, content_type)
            .body(body);
        if let Some(token) = &auth.client {
            request = request.header("client-token", confidential(token)?);
        }
        if search {
            request = request
                .header(
                    "app-platform",
                    if auth.public {
                        "WebPlayer"
                    } else {
                        "Win32_x86_64"
                    },
                )
                .header(
                    "spotify-app-version",
                    librespot_core::version::spotify_semantic_version(),
                );
        }
        self.execute(request).await
    }

    async fn check_cooldown(&self) -> Result<(), Error> {
        if self
            .cooldown
            .lock()
            .await
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(Error::resource_exhausted(
                "Spotify request cooldown is active",
            ));
        }
        Ok(())
    }

    pub(super) async fn response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        self.check_cooldown().await?;
        let response = request
            .send()
            .await
            .map_err(|error| Error::unavailable(error.without_url()))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let delay = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(retry_after)
                .unwrap_or(Duration::from_secs(60))
                .max(Duration::from_secs(1));
            let mut cooldown = self.cooldown.lock().await;
            let now = Instant::now();
            let until = now
                .checked_add(delay)
                .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 60 * 60));
            *cooldown = Some(cooldown.map_or(until, |previous| previous.max(until)));
        }
        Ok(response)
    }

    async fn execute(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, Error> {
        let response = self.response(request).await?;
        let status = response.status();
        if !status.is_success() {
            // Never include upstream response bodies: login errors can contain credentials.
            return Err(Error::new(
                error_kind(status),
                format!("Spotify HTTP {status}"),
            ));
        }
        read_body(response, MAX_RESPONSE_BYTES).await
    }
}

pub(super) fn confidential(value: &str) -> Result<reqwest::header::HeaderValue, Error> {
    let mut header = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| Error::invalid_argument("Invalid Spotify authentication header"))?;
    header.set_sensitive(true);
    Ok(header)
}

pub(super) async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::data_loss("Spotify response exceeds its size limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(Error::unavailable)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(Error::data_loss("Spotify response exceeds its size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn public_profile(url: &str) -> bool {
    url.contains("/user-profile-view/") && !following_list(url)
}
fn following_list(url: &str) -> bool {
    url.split('?')
        .next()
        .is_some_and(|path| path.ends_with("/following") || path.ends_with("/followers"))
}

fn retry_after(header: &str) -> Option<Duration> {
    if let Ok(seconds) = header.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = chrono::DateTime::parse_from_rfc2822(header).ok()?;
    (deadline.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

pub(super) fn error_kind(status: StatusCode) -> ErrorKind {
    match status {
        StatusCode::UNAUTHORIZED => ErrorKind::Unauthenticated,
        StatusCode::FORBIDDEN => ErrorKind::PermissionDenied,
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::TOO_MANY_REQUESTS => ErrorKind::ResourceExhausted,
        StatusCode::PRECONDITION_FAILED => ErrorKind::FailedPrecondition,
        status if status.is_client_error() => ErrorKind::InvalidArgument,
        _ => ErrorKind::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn response(reply: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut read = BufReader::new(&mut socket);
            loop {
                let mut line = String::new();
                assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
            }
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        (url, task)
    }

    fn client() -> Client {
        Client::new(Session::new(Default::default(), None))
    }

    #[tokio::test]
    async fn rate_limit_stops_other_endpoints_without_sending_another_request() {
        let client = client();
        let (url, server) = response("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let error = client
            .execute(super::super::http().post(url))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ResourceExhausted);
        server.await.unwrap();
        // This address is deliberately unusable. A request during the cooldown
        // must return the rate-limit error before touching the network.
        let error = client
            .execute(super::super::http().get("http://127.0.0.1:1/other"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ResourceExhausted);
        assert!(
            client
                .cooldown
                .lock()
                .await
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(110)
        );
    }

    #[tokio::test]
    async fn playlist_mutations_are_not_retried_or_logged_with_response_bodies() {
        let client = client();
        let (url, server) = response("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 23\r\nConnection: close\r\n\r\nprivate-account-details").await;
        let error = client
            .execute(super::super::http().post(url))
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert!(error.to_string().contains("503"));
        assert!(!error.to_string().contains("private-account-details"));
    }

    #[tokio::test]
    async fn oversized_metadata_is_rejected_before_reading_its_body() {
        let client = client();
        let (url, server) =
            response("HTTP/1.1 200 OK\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n")
                .await;
        let error = client
            .execute(super::super::http().get(url))
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(error.kind, ErrorKind::DataLoss);
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(retry_after("172800"), Some(Duration::from_secs(172800)));
        let date = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc2822();
        let delay = retry_after(&date).unwrap();
        assert!(delay > Duration::from_secs(298) && delay <= Duration::from_secs(300));
        assert_eq!(retry_after("not a deadline"), None);
    }

    #[tokio::test]
    #[ignore = "requires a saved Spotify login and network; reads catalog only"]
    async fn live_catalog_smoke() -> anyhow::Result<()> {
        super::super::connection::ALLOW_IN_TESTS.store(true, std::sync::atomic::Ordering::Relaxed);
        let rules = crate::config::PlaylistExclusions::default();
        for path in ["me/playlists", "me/albums", "me/following"] {
            let page = super::super::get(path, &[("limit", "1".into())], &rules).await?;
            anyhow::ensure!(page.is_object(), "catalog response is not an object");
        }
        let page = super::super::get(
            "search",
            &[
                ("q", "Bach".into()),
                ("type", "track".into()),
                ("limit", "1".into()),
            ],
            &rules,
        )
        .await?;
        anyhow::ensure!(
            page["tracks"]["items"].is_array(),
            "search did not return tracks"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "uses Soloist's saved pairing and network; no catalog login, playback controls or playlist edits"]
    async fn live_single_login() -> anyhow::Result<()> {
        let auth = SoloistAuth::load()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Soloist needs pairing"))?;
        let client = Client::paired(auth);
        // Deliberately disable the optional catalog fallback. This test must
        // prove the new path works even when old credentials are present.
        client.fallback.lock().await.retry_at = Some(Instant::now() + Duration::from_secs(3600));
        client.prepare().await?;
        let paging = super::super::request::Window {
            offset: 0,
            limit: 1,
        };
        let rules = crate::config::PlaylistExclusions::default();
        let playlists =
            super::super::playlists::user_playlists(&client, &[("limit", "1".into())], &rules)
                .await?;
        anyhow::ensure!(playlists["items"].is_array(), "invalid playlist page");
        let albums = super::super::library::saved_albums(&client, paging).await?;
        anyhow::ensure!(albums["items"].is_array(), "invalid saved album page");
        let artists =
            super::super::library::followed_artists(&client, &[("limit", "1".into())]).await?;
        anyhow::ensure!(
            artists["artists"]["items"].is_array(),
            "invalid followed artist page"
        );
        let search = super::super::search::search(
            &client,
            &[
                ("q", "Bach".into()),
                ("type", "track".into()),
                ("limit", "1".into()),
            ],
        )
        .await?;
        let track = search["tracks"]["items"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or_else(|| anyhow::anyhow!("public search returned no actual tracks"))?;
        let album_id = track["album"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("search track has no album"))?;
        let tracks = super::super::catalog::album_tracks(&client, album_id, paging).await?;
        anyhow::ensure!(
            tracks["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "full album returned no tracks"
        );
        let artist_id = track["artists"][0]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("search track has no artist"))?;
        let releases = super::super::catalog::artist_albums(&client, artist_id, paging).await?;
        anyhow::ensure!(
            releases["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "full artist returned no releases"
        );
        let profile = super::super::people::profile(&client, &client.username()).await?;
        anyhow::ensure!(profile["uri"].is_string(), "public profile returned no URI");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires matching Soloist and optional catalog credentials; reads followed profiles only"]
    async fn live_optional_catalog_fallback() -> anyhow::Result<()> {
        let auth = SoloistAuth::load()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Soloist needs pairing"))?;
        let client = Client::paired(auth);
        anyhow::ensure!(
            client.fallback().await?.is_some(),
            "No matching optional catalog login"
        );
        let following = super::super::people::following(&client).await?;
        anyhow::ensure!(
            following["items"].is_array(),
            "Followed profiles did not return a list"
        );
        Ok(())
    }
}
