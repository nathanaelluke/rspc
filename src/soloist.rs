//! Spotify's official Soloist player, controlled over one persistent local socket.
use crate::{
    model::{ArtistRef, TrackInfo},
    playback::{AudioEvent, AudioUpdate},
};
use anyhow::{Context, Result, bail, ensure};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, oneshot},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
#[derive(Default)]
struct State {
    logged_in: bool,
    active: bool,
    uri: String,
    track: Option<TrackInfo>,
    status: String,
    position: u64,
    at: Option<Instant>,
    speed: f64,
    duration: u64,
    request: u64,
    requested: String,
    confirmed: bool,
    ended: bool,
    manual_pause: bool,
    received_auth: bool,
    awaiting_play: bool,
}
impl State {
    fn position(&self) -> u64 {
        self.position.saturating_add(self.at.map_or(0, |at| {
            (at.elapsed().as_millis() as f64 * self.speed.max(0.0)) as u64
        }))
    }
    fn reserve(&mut self, uri: &str, request: u64) {
        self.request = request;
        self.requested = uri.into();
        self.confirmed = false;
        self.ended = false;
        self.manual_pause = false;
        self.position = 0;
        self.at = Some(Instant::now());
        self.speed = 0.0;
    }
    fn set_active(&mut self, active: bool) {
        self.active = active;
        if !active && self.confirmed && !self.ended && !self.awaiting_play {
            // The daemon releases this request when Connect moves away. A later
            // snapshot must establish ownership again, even for the same URI.
            self.follow_external();
        }
    }
    fn event(&mut self, v: &Value) -> Vec<AudioEvent> {
        let mut out = Vec::new();
        let kind = v["type"].as_str().unwrap_or("");
        if kind == "auth_state" {
            self.received_auth = true;
            self.logged_in = v["logged_in"].as_bool().unwrap_or(false);
            self.set_active(v["is_active"].as_bool().unwrap_or(false));
            if self.logged_in {
                out.push(AudioEvent::Authenticated);
            }
            if !self.active {
                out.push(AudioEvent::Inactive);
            }
            return out;
        }
        if kind == "error" {
            out.push(AudioEvent::Error(
                v["message"]
                    .as_str()
                    .unwrap_or("Soloist command failed")
                    .into(),
            ));
            return out;
        }
        if kind == "volume_changed" || kind == "playback_state" {
            if let Some(n) = v["volume"].as_u64() {
                out.push(AudioEvent::Volume(n.min(100) as u8));
            }
            if kind == "volume_changed" {
                return out;
            }
        }
        if (kind == "device_changed" || kind == "playback_state")
            && let Some(active) = v["is_active"].as_bool()
        {
            self.set_active(active);
            if !active {
                out.push(AudioEvent::Inactive);
            }
        }
        // Configuration/activation can replay the restored Spotify context.
        // Those snapshots do not acknowledge the track the queue selected.
        if self.awaiting_play {
            return out;
        }
        let old_position = self.position();
        let old_duration = self.duration;
        let mut track_changed = false;
        if (kind == "playback_state" || kind == "track_changed")
            && let Some(uri) = v["item"]["uri"].as_str().filter(|uri| !uri.is_empty())
        {
            // A delayed snapshot for the old track must not replace a new
            // queue selection while its play command is still loading.
            if self.request != 0 && !self.confirmed && !self.ended && uri != self.requested {
                return out;
            }
            track_changed = uri != self.uri;
            if uri != self.uri && self.confirmed && !self.ended && self.request != 0 {
                self.ended = true;
                if old_duration > 0
                    && old_position.saturating_add((old_duration / 10).min(2000)) >= old_duration
                {
                    out.push(AudioEvent::Ended {
                        request: self.request,
                    });
                } else {
                    self.follow_external();
                }
            } else if track_changed && self.ended {
                self.follow_external();
            }
            self.uri = uri.to_owned();
            self.track = track_from_entity(&v["item"]);
            if let Some(duration) = v["item"]["decorations"]["playback"]["duration_ms"]
                .as_u64()
                .filter(|n| *n > 0)
            {
                self.duration = duration;
            } else if track_changed && uri != self.requested {
                self.duration = 0;
            }
            if uri == self.requested && self.request != 0 {
                self.confirmed = true;
            }
            if track_changed {
                self.position = 0;
                self.at = Some(Instant::now());
            }
        }
        if let Some(position) = v["position"].as_object() {
            self.position = position
                .get("position_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.speed = position.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
            let timestamp = position
                .get("timestamp_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
            let lag = now.saturating_sub(timestamp);
            if timestamp != 0 && lag < 86_400_000 {
                self.position = self
                    .position
                    .saturating_add((lag as f64 * self.speed.max(0.0)) as u64);
            }
            self.at = Some(Instant::now());
            if !self.ended && (self.confirmed || (self.request == 0 && self.active)) {
                out.push(AudioEvent::Position {
                    request: self.request,
                    position: self.position,
                });
            }
        }
        if let Some(status) = v["status"].as_str() {
            if self.ended && self.status == "paused" && status == "playing" {
                // Playback resumed from Spotify after an MPD stop.
                self.follow_external();
            }
            if self.confirmed
                && !self.ended
                && (status == "idle" || (status == "paused" && !self.manual_pause))
                && old_duration > 0
                && old_position.saturating_add(250) >= old_duration
            {
                self.ended = true;
                out.push(AudioEvent::Ended {
                    request: self.request,
                });
            }
            if v.get("position").is_none() {
                if !track_changed {
                    self.position = old_position;
                }
                self.at = Some(Instant::now());
                if status == "playing" {
                    self.speed = 1.0;
                }
            }
            self.status = status.into();
            if status != "playing" {
                self.speed = 0.0;
            }
        }
        if self.confirmed
            && !self.ended
            && self.active
            && (kind == "playback_state" || kind == "playback_changed" || kind == "track_changed")
        {
            out.push(AudioEvent::State {
                request: self.request,
                position: self.position(),
                playing: self.status == "playing",
            });
        } else if self.request == 0
            && !self.ended
            && self.active
            && matches!(
                kind,
                "playback_state" | "playback_changed" | "track_changed"
            )
            && let Some(track) = &self.track
        {
            out.push(AudioEvent::ExternalState {
                track: track.clone(),
                position: self.position(),
                playing: self.status == "playing",
                stopped: self.status == "idle",
            });
        }
        out
    }
    fn follow_external(&mut self) {
        self.request = 0;
        self.requested.clear();
        self.confirmed = false;
        self.ended = false;
        self.manual_pause = false;
    }
}
fn track_from_entity(entity: &Value) -> Option<TrackInfo> {
    let uri = entity["uri"].as_str()?;
    if !uri.starts_with("spotify:track:") && !uri.starts_with("spotify:episode:") {
        return None;
    }
    let d = &entity["decorations"];
    let parent = &d["parent"]["entity"];
    let artist_refs: Vec<_> = d["creators"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|creator| {
            let creator = &creator["entity"];
            ArtistRef {
                id: creator["uri"]
                    .as_str()
                    .and_then(|u| u.strip_prefix("spotify:artist:"))
                    .map(str::to_owned),
                name: creator["decorations"]["identity"]["name"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .into(),
            }
        })
        .collect();
    Some(TrackInfo {
        uri: Some(uri.into()),
        id: uri.strip_prefix("spotify:track:").map(str::to_owned),
        name: d["identity"]["name"].as_str().unwrap_or(uri).into(),
        artists: artist_refs.iter().map(|a| a.name.clone()).collect(),
        album_artists: parent["decorations"]["creators"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                a["entity"]["decorations"]["identity"]["name"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect(),
        artist_refs,
        album: parent["decorations"]["identity"]["name"]
            .as_str()
            .unwrap_or_default()
            .into(),
        album_id: parent["uri"]
            .as_str()
            .and_then(|u| u.strip_prefix("spotify:album:"))
            .map(str::to_owned),
        duration_ms: d["playback"]["duration_ms"].as_u64().unwrap_or(0),
        image_url: d["visual_identity"]["cover"]
            .as_array()
            .and_then(|covers| {
                covers
                    .iter()
                    .find(|c| c["size"] == "large")
                    .or_else(|| covers.first())
            })
            .and_then(|cover| cover["url"].as_str())
            .map(str::to_owned),
        is_playable: true,
        is_local: false,
        track_number: 0,
        explicit: d["playback"]["content_ratings"]
            .as_array()
            .is_some_and(|ratings| ratings.iter().any(|r| r == "explicit")),
        release_date: None,
    })
}
struct Request {
    value: Value,
    deadline: Instant,
    done: oneshot::Sender<Result<()>>,
}
pub struct Soloist {
    tx: mpsc::Sender<Request>,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
    request: AtomicU64,
    configured: AtomicBool,
    child: Mutex<Option<Child>>,
    events: tokio::task::JoinHandle<()>,
}
pub fn binary() -> PathBuf {
    std::env::var_os("RSPC_SOLOIST_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::state().join("soloist-package/bin/soloist"))
}
pub fn key_file() -> PathBuf {
    crate::paths::xdg("XDG_CONFIG_HOME", ".config").join("rspc/soloist-api-key")
}
fn api_key() -> Result<String> {
    let key = std::env::var("RSPC_SOLOIST_API_KEY")
        .ok()
        .or_else(|| std::fs::read_to_string(key_file()).ok())
        .context(
            "Soloist API key missing: save it in ~/.config/rspc/soloist-api-key and run rspc pair",
        )?;
    ensure!(!key.trim().is_empty(), "Soloist API key file is empty");
    Ok(key.trim().into())
}
impl Soloist {
    pub async fn connect(epoch: u64, tx: mpsc::UnboundedSender<AudioUpdate>) -> Result<Self> {
        let executable = binary();
        ensure!(
            executable.is_file(),
            "Soloist is missing; run scripts/setup-soloist.sh"
        );
        let key = api_key()?;
        let data = crate::paths::state().join("soloist");
        let cache = crate::paths::cache().join("soloist");
        crate::paths::ensure_private_dir(&data)?;
        crate::paths::ensure_private_dir(&cache)?;
        use std::os::unix::fs::OpenOptionsExt;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(crate::paths::state().join("soloist.log"))?;
        let mut child = Command::new(executable)
            .args(["-n", "rspc", "-k"])
            .arg(key)
            .arg("-D")
            .arg(&data)
            .arg("-C")
            .arg(cache)
            .args(["-z", "256", "-w", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("starting Soloist")?;
        let id = child.id().context("Soloist has no process id")?.to_string();
        let start = Instant::now();
        let endpoint = loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "Soloist exited with {status}; check ~/.local/state/rspc/soloist.log (exit 10 means update required)"
                );
            }
            let owner = tokio::fs::read_to_string(data.join("soloist.pid"))
                .await
                .unwrap_or_default();
            if owner.trim() == id
                && let Ok(port) = tokio::fs::read_to_string(data.join("ws.port")).await
                && let Ok(port) = port.trim().parse::<u16>()
            {
                break format!("ws://127.0.0.1:{port}");
            }
            ensure!(
                start.elapsed() < Duration::from_secs(12),
                "Soloist local API did not become ready; see soloist.log"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let (socket, _) = tokio_tungstenite::connect_async(&endpoint)
            .await
            .context("connecting to Soloist local API")?;
        let soloist = Self::from_socket(epoch, socket, Some(endpoint), Some(child), tx);
        let start = Instant::now();
        while !soloist.state.lock().unwrap().received_auth {
            ensure!(
                start.elapsed() < Duration::from_secs(2),
                "Soloist did not send its initial authentication state"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(soloist)
    }
    fn from_socket(
        epoch: u64,
        socket: Socket,
        endpoint: Option<String>,
        child: Option<Child>,
        events: mpsc::UnboundedSender<AudioUpdate>,
    ) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let closed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(32);
        let task = tokio::spawn(Self::pump(
            epoch,
            socket,
            endpoint,
            rx,
            Arc::clone(&state),
            Arc::clone(&closed),
            events,
        ));
        Self {
            tx,
            state,
            closed,
            request: AtomicU64::new(0),
            configured: AtomicBool::new(false),
            child: Mutex::new(child),
            events: task,
        }
    }
    #[cfg(test)]
    pub(crate) fn test_connection(socket: Socket, tx: mpsc::UnboundedSender<AudioUpdate>) -> Self {
        Self::from_socket(1, socket, None, None, tx)
    }
    async fn pump(
        epoch: u64,
        mut socket: Socket,
        endpoint: Option<String>,
        mut rx: mpsc::Receiver<Request>,
        state: Arc<Mutex<State>>,
        closed: Arc<AtomicBool>,
        events: mpsc::UnboundedSender<AudioUpdate>,
    ) {
        let mut pending: VecDeque<Request> = VecDeque::new();
        let mut check = tokio::time::interval(Duration::from_secs(1));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let deadline = pending.iter().map(|request| request.deadline).min();
            let failure = tokio::select! {
                request = rx.recv() => {
                    let Some(request) = request else { break };
                    if request.done.is_closed() || request.deadline <= Instant::now() {
                        let _ = request.done.send(Err(anyhow::anyhow!("Soloist command expired before dispatch")));
                        continue;
                    }
                    let message = Message::Text(request.value.to_string().into());
                    match tokio::time::timeout_at(request.deadline.into(), socket.send(message)).await {
                        Ok(Ok(())) => {
                            pending.push_back(request);
                            None
                        }
                        _ => {
                            let reason = "Soloist command could not be sent";
                            let _ = request.done.send(Err(anyhow::anyhow!(reason)));
                            Some(reason)
                        }
                    }
                }
                message = socket.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let mut failure = None;
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            match value["type"].as_str() {
                                Some("command_result") => {
                                    let command = value["command"].as_str();
                                    let index = pending.iter().position(|request| request.value["command"].as_str() == command);
                                    if let Some(request) = index.and_then(|index| pending.remove(index)) {
                                        let _ = request.done.send(Ok(()));
                                    }
                                }
                                Some("error") => {
                                    let message = value["message"].as_str().unwrap_or("command failed");
                                    if let Some(request) = pending.pop_front() {
                                        let _ = request.done.send(Err(anyhow::anyhow!("Soloist: {message}")));
                                    } else {
                                        let _ = events.send(AudioUpdate::spotify(epoch, AudioEvent::Error(message.into())));
                                    }
                                }
                                _ => {
                                    let refresh = (value["type"] == "auth_state" && value["logged_in"] == true)
                                        || (value["type"] == "device_changed" && value["is_active"] == true);
                                    let updates = state.lock().unwrap().event(&value);
                                    for update in updates {
                                        let _ = events.send(AudioUpdate::spotify(epoch, update));
                                    }
                                    // Granular login/device events may omit track metadata.
                                    if refresh && Self::request_state(&mut socket).await.is_err() {
                                        failure = Some("Soloist state refresh failed");
                                    }
                                }
                            }
                        }
                        failure
                    }
                    Some(Ok(Message::Ping(data))) => {
                        match tokio::time::timeout(COMMAND_TIMEOUT, socket.send(Message::Pong(data))).await {
                            Ok(Ok(())) => None,
                            _ => Some("Soloist ping response failed"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => Some("Soloist disconnected"),
                    _ => None,
                },
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                        None => std::future::pending().await,
                    }
                } => Some("Soloist did not acknowledge command"),
                _ = check.tick() => {
                    let near_end = {
                        let state = state.lock().unwrap();
                        state.logged_in && state.active && state.confirmed && !state.ended
                            && state.duration > 0 && state.position().saturating_add(1000) >= state.duration
                    };
                    if near_end && Self::request_state(&mut socket).await.is_err() {
                        Some("Soloist state refresh failed")
                    } else {
                        None
                    }
                }
            };
            let Some(reason) = failure else { continue };
            // Results identify only the command name. Once an acknowledgement is
            // late, reconnect before accepting another result for that command.
            for request in pending.drain(..) {
                let _ = request
                    .done
                    .send(Err(anyhow::anyhow!("{reason}; retry command")));
            }
            match Self::reconnect(endpoint.as_deref()).await {
                Some(connection) => socket = connection,
                None => {
                    closed.store(true, Ordering::Relaxed);
                    let _ = events.send(AudioUpdate::spotify(
                        epoch,
                        AudioEvent::Error(
                            "Soloist disconnected; retry play to restart the player".into(),
                        ),
                    ));
                    break;
                }
            }
        }
    }

    async fn request_state(socket: &mut Socket) -> Result<()> {
        let message = Message::Text(
            json!({"type":"command", "command":"get_state"})
                .to_string()
                .into(),
        );
        tokio::time::timeout(COMMAND_TIMEOUT, socket.send(message)).await??;
        Ok(())
    }

    async fn reconnect(endpoint: Option<&str>) -> Option<Socket> {
        let endpoint = endpoint?;
        for delay in [100, 300, 1000] {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if let Ok(Ok((socket, _))) = tokio::time::timeout(
                Duration::from_secs(1),
                tokio_tungstenite::connect_async(endpoint),
            )
            .await
            {
                return Some(socket);
            }
        }
        None
    }
    pub async fn shutdown(&self) {
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child
            && let Some(pid) = child.id()
        {
            // This PID comes from our owned child handle.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            if tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        self.events.abort();
    }
    pub fn invalid(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
    pub fn logged_in(&self) -> bool {
        self.state.lock().unwrap().logged_in
    }
    pub fn is_playing(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.active && state.status == "playing"
    }
    pub fn reserve(&self, uri: &str, duration: u64) -> Result<u64> {
        self.ensure_logged_in()?;
        let request = self.request.fetch_add(1, Ordering::Relaxed) + 1;
        let mut state = self.state.lock().unwrap();
        state.reserve(uri, request);
        state.duration = duration;
        state.awaiting_play = true;
        Ok(request)
    }
    fn ensure_logged_in(&self) -> Result<()> {
        ensure!(
            self.logged_in(),
            "Select rspc in Spotify's device menu to pair Soloist, then retry Play."
        );
        Ok(())
    }
    async fn send(&self, command: &str, extra: Value) -> Result<()> {
        self.ensure_logged_in()?;
        let mut fields = extra.as_object().cloned().unwrap_or_default();
        fields.insert("type".into(), json!("command"));
        fields.insert("command".into(), json!(command));
        let value = Value::Object(fields);
        let (done, reply) = oneshot::channel();
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let request = Request {
            value,
            deadline,
            done,
        };
        tokio::time::timeout_at(deadline.into(), async {
            self.tx
                .send(request)
                .await
                .context("Soloist command channel closed")?;
            reply.await.context("Soloist disconnected")?
        })
        .await
        .context("Soloist did not acknowledge command")?
    }
    pub async fn load(&self, uri: &str) -> Result<()> {
        let (request, duration) = {
            let state = self.state.lock().unwrap();
            (state.request, state.duration)
        };
        let active = self.state.lock().unwrap().active;
        if !active {
            self.send("activate", json!({})).await?;
        }
        if !self.configured.swap(true, Ordering::Relaxed) {
            let result = async {
                self.send("set_shuffle", json!({"enabled":false})).await?;
                self.send("set_repeat_context", json!({"enabled":false}))
                    .await?;
                self.send("set_repeat_track", json!({"enabled":false}))
                    .await
            }
            .await;
            if let Err(error) = result {
                self.configured.store(false, Ordering::Relaxed);
                return Err(error);
            }
        }
        {
            let mut state = self.state.lock().unwrap();
            state.reserve(uri, request);
            state.duration = duration;
            state.awaiting_play = false;
        }
        self.send("play", json!({"uri":uri})).await
    }
    pub async fn play(&self) -> Result<()> {
        self.set_paused(false).await
    }
    pub async fn pause(&self) -> Result<()> {
        self.set_paused(true).await
    }
    async fn set_paused(&self, paused: bool) -> Result<()> {
        let (previous, request) = {
            let mut state = self.state.lock().unwrap();
            let previous = state.manual_pause;
            state.manual_pause = paused;
            (previous, state.request)
        };
        let result = self
            .send(if paused { "pause" } else { "play" }, json!({}))
            .await;
        if result.is_err() {
            let mut state = self.state.lock().unwrap();
            if state.request == request && state.manual_pause == paused {
                state.manual_pause = previous;
            }
        }
        result
    }
    pub async fn stop(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.ended = true;
            state.awaiting_play = false;
        }
        let active = {
            let state = self.state.lock().unwrap();
            state.logged_in && state.active
        };
        if active {
            let _ = self.pause().await;
        }
    }
    pub async fn suspend(&self) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.ended = true;
            if !state.logged_in
                || !state.active
                || matches!(state.status.as_str(), "paused" | "idle")
            {
                return Ok(());
            }
        }
        self.pause().await?;
        let start = Instant::now();
        while {
            let state = self.state.lock().unwrap();
            state.active && !matches!(state.status.as_str(), "paused" | "idle")
        } {
            ensure!(
                start.elapsed() < Duration::from_secs(3),
                "Spotify did not pause before switching to local playback"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }
    pub async fn seek(&self, position: u32) -> Result<()> {
        self.send("seek", json!({"position_ms":position})).await
    }
    pub async fn skip(&self, previous: bool) -> Result<()> {
        self.send(if previous { "skip_prev" } else { "skip_next" }, json!({}))
            .await
    }
    pub async fn volume(&self, value: u8) -> Result<()> {
        self.send("set_volume", json!({"volume":value})).await
    }
    pub fn diagnostics(&self) -> String {
        let s = self.state.lock().unwrap();
        format!(
            "audio_engine: soloist\nlogged_in: {}\nactive: {}\nsoloist_status: {}\nsoloist_position_ms: {}\nsoloist_duration_ms: {}\nsoloist_request: {}\nsoloist_confirmed: {}\nsoloist_ended: {}\n",
            u8::from(s.logged_in),
            u8::from(s.active),
            s.status,
            s.position(),
            s.duration,
            s.request,
            u8::from(s.confirmed),
            u8::from(s.ended)
        )
    }
}
impl Drop for Soloist {
    fn drop(&mut self) {
        self.events.abort();
        if let Some(child) = self.child.get_mut().unwrap().as_mut() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests;
