//! MPD library reads and an isolated single-track player for the shared queue.
use crate::{
    model::TrackInfo,
    playback::{AudioEvent, AudioUpdate, Backend},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream},
    net::{TcpStream, UnixStream},
    sync::{Mutex, mpsc, watch},
};

pub const ROOT: &str = "Local Files";
pub const PLAYLISTS: &str = "Local Playlists/";
const PARTITION: &str = "rspc";

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub address: String,
    password: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            address: crate::paths::xdg("XDG_DATA_HOME", ".local/share")
                .join("mpd/socket")
                .to_string_lossy()
                .into_owned(),
            password: None,
        }
    }
}
impl Settings {
    fn discover() -> Self {
        let file = std::env::var_os("RSPC_MPD_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                crate::paths::xdg("XDG_CONFIG_HOME", ".config").join("rmpc/config.ron")
            });
        let mut value = match std::fs::read_to_string(file) {
            Ok(text) => ron::from_str(&text).unwrap_or_else(|_| {
                log::warn!("could not read MPD settings from rmpc config; using the default socket or RSPC_MPD_ADDRESS");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        if let Ok(address) = std::env::var("RSPC_MPD_ADDRESS") {
            value.address = address;
        }
        if let Ok(password) = std::env::var("RSPC_MPD_PASSWORD") {
            value.password = Some(password);
        }
        if let Some(path) = value.address.strip_prefix("~/") {
            value.address = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(path)
                .to_string_lossy()
                .into_owned();
        }
        value
    }
}
pub fn quote(text: &str) -> Result<String> {
    ensure!(!text.contains(['\n', '\r', '\0']), "invalid MPD argument");
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
pub fn playlist_uri(uri: &str) -> Result<String> {
    let file = uri.strip_prefix("mpd:").unwrap_or(uri);
    // playlistadd treats a bare absolute path as a database directory;
    // unlike addid, it needs the file URL form for files outside the library.
    if file.starts_with('/') {
        Ok(url::Url::from_file_path(file)
            .map_err(|_| anyhow::anyhow!("invalid local file path"))?
            .into())
    } else {
        Ok(file.to_owned())
    }
}
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
struct Wire {
    stream: BufStream<Box<dyn Io>>,
}
impl Wire {
    async fn open(settings: &Settings) -> Result<Self> {
        let stream: Box<dyn Io> = if settings.address.starts_with('/') {
            Box::new(
                tokio::time::timeout(
                    Duration::from_secs(2),
                    UnixStream::connect(&settings.address),
                )
                .await
                .context("MPD connection timed out")??,
            )
        } else {
            Box::new(
                tokio::time::timeout(
                    Duration::from_secs(2),
                    TcpStream::connect(&settings.address),
                )
                .await
                .context("MPD connection timed out")??,
            )
        };
        let mut wire = Self {
            stream: BufStream::new(stream),
        };
        let mut hello = String::new();
        tokio::time::timeout(Duration::from_secs(2), wire.stream.read_line(&mut hello)).await??;
        ensure!(
            hello.starts_with("OK MPD "),
            "server is not an MPD instance"
        );
        if let Some(password) = &settings.password {
            wire.command(&format!("password {}", quote(password)?))
                .await
                .context("MPD authentication failed")?;
        }
        Ok(wire)
    }
    async fn command(&mut self, command: &str) -> Result<Vec<u8>> {
        ensure!(!command.contains(['\n', '\r', '\0']), "invalid MPD command");
        tokio::time::timeout(Duration::from_secs(12), async {
            self.stream.write_all(command.as_bytes()).await?;
            self.stream.write_all(b"\n").await?;
            self.stream.flush().await?;
            self.response().await
        })
        .await
        .context("MPD request timed out")?
    }
    async fn response(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut line = Vec::new();
            ensure!(
                self.stream.read_until(b'\n', &mut line).await? > 0,
                "MPD disconnected"
            );
            if line == b"OK\n" {
                return Ok(out);
            }
            if line.starts_with(b"ACK ") {
                bail!("MPD: {}", String::from_utf8_lossy(&line).trim());
            }
            ensure!(
                out.len() + line.len() <= 32 * 1024 * 1024,
                "MPD response is too large"
            );
            let binary = line
                .strip_prefix(b"binary: ")
                .map(|n| {
                    std::str::from_utf8(n)?
                        .trim()
                        .parse::<usize>()
                        .map_err(anyhow::Error::from)
                })
                .transpose()?;
            out.extend(line);
            if let Some(n) = binary {
                ensure!(n <= 1024 * 1024, "MPD binary chunk is too large");
                let start = out.len();
                out.resize(start + n + 1, 0);
                self.stream.read_exact(&mut out[start..]).await?;
                ensure!(out.last() == Some(&b'\n'), "invalid MPD binary terminator");
            }
        }
    }
}
fn fields(bytes: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.into(), v.into())))
        .collect()
}
fn outputs(bytes: &[u8]) -> Vec<HashMap<String, String>> {
    String::from_utf8_lossy(bytes)
        .split("outputid: ")
        .skip(1)
        .map(|record| fields(format!("outputid: {record}").as_bytes()))
        .collect()
}
pub fn tracks(bytes: &[u8]) -> Vec<TrackInfo> {
    let mut records: Vec<Vec<(String, String)>> = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let Some((k, v)) = line.split_once(": ") else {
            continue;
        };
        if matches!(k, "file" | "directory" | "playlist") {
            records.push(Vec::new());
        }
        if let Some(record) = records.last_mut() {
            record.push((k.into(), v.into()));
        }
    }
    records
        .into_iter()
        .filter_map(|r| {
            let get = |key: &str| {
                r.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v.as_str())
            };
            let file = get("file")?;
            let artists: Vec<_> = r
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("Artist"))
                .map(|(_, v)| v.clone())
                .collect();
            let artists = if artists.is_empty() {
                get("AlbumArtist")
                    .map(|v| vec![v.into()])
                    .unwrap_or_default()
            } else {
                artists
            };
            let duration = get("duration")
                .or_else(|| get("Time"))
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.0)
                .unwrap_or(0.0);
            Some(TrackInfo {
                uri: Some(format!("mpd:{file}")),
                name: get("Title")
                    .unwrap_or_else(|| file.rsplit('/').next().unwrap_or(file))
                    .into(),
                artists,
                album_artists: r
                    .iter()
                    .filter(|(k, _)| k.eq_ignore_ascii_case("AlbumArtist"))
                    .map(|(_, v)| v.clone())
                    .collect(),
                album: get("Album").unwrap_or_default().into(),
                duration_ms: (duration * 1000.0) as u64,
                id: None,
                album_id: None,
                artist_refs: Vec::new(),
                is_playable: true,
                is_local: file.starts_with("file://") || !file.contains("://"),
                track_number: get("Track")
                    .and_then(|s| s.split('/').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                explicit: false,
                image_url: None,
                release_date: get("Date").map(str::to_owned),
            })
        })
        .collect()
}
pub struct Browse {
    pub directories: Vec<String>,
    pub tracks: Vec<TrackInfo>,
}
pub struct Library {
    pub settings: Settings,
    wire: Mutex<Option<Wire>>,
    cached: Mutex<Option<(Instant, Arc<Vec<TrackInfo>>)>>,
    unavailable: Mutex<Option<Instant>>,
    watching: AtomicBool,
    done: watch::Sender<bool>,
}
impl Library {
    pub fn new() -> Self {
        Self::with_settings(Settings::discover())
    }
    fn with_settings(settings: Settings) -> Self {
        let (done, _) = watch::channel(false);
        Self {
            settings,
            wire: Mutex::new(None),
            cached: Mutex::new(None),
            unavailable: Mutex::new(None),
            watching: AtomicBool::new(false),
            done,
        }
    }
    pub fn watch(self: &Arc<Self>, tx: mpsc::UnboundedSender<AudioUpdate>) {
        if self.watching.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak = Arc::downgrade(self);
        let settings = self.settings.clone();
        let mut done = self.done.subscribe();
        tokio::spawn(async move {
            loop {
                let connection = tokio::select! { _=done.changed()=>break, connection=Wire::open(&settings)=>connection };
                if let Ok(mut wire) = connection {
                    loop {
                        let response = async {
                            wire.stream
                                .write_all(b"idle database stored_playlist update\n")
                                .await?;
                            wire.stream.flush().await?;
                            wire.response().await
                        };
                        let response = tokio::select! { _=done.changed()=>return, response=response=>response };
                        if response.is_err() {
                            break;
                        }
                        if weak.upgrade().is_none() {
                            return;
                        }
                        if tx
                            .send(AudioUpdate {
                                backend: Backend::Mpd,
                                epoch: 0,
                                event: AudioEvent::LibraryChanged,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                tokio::select! { _=done.changed()=>break, _=tokio::time::sleep(Duration::from_secs(10))=>{} }
            }
        });
    }
    pub async fn request(&self, command: &str) -> Result<Vec<u8>> {
        let mut slot = self.wire.lock().await;
        if slot.is_none() {
            *slot =
                Some(Wire::open(&self.settings).await.context(
                    "Local MPD unavailable; check RSPC_MPD_ADDRESS or your rmpc config",
                )?);
        }
        let result = slot.as_mut().unwrap().command(command).await;
        if result.is_err() {
            slot.take();
        }
        result
    }
    pub async fn invalidate(&self) {
        self.cached.lock().await.take();
        self.unavailable.lock().await.take();
    }
    pub async fn all(&self) -> Result<Arc<Vec<TrackInfo>>> {
        let mut cached = self.cached.lock().await;
        if let Some((at, tracks)) = cached
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
        {
            let _ = at;
            return Ok(Arc::clone(tracks));
        }
        if self
            .unavailable
            .lock()
            .await
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            bail!("Local MPD is unavailable");
        }
        match self.request("listallinfo").await {
            Ok(reply) => {
                let tracks = Arc::new(tracks(&reply));
                *cached = Some((Instant::now(), Arc::clone(&tracks)));
                Ok(tracks)
            }
            Err(e) => {
                *self.unavailable.lock().await = Some(Instant::now());
                Err(e)
            }
        }
    }
    pub async fn browse(&self, path: &str, recursive: bool) -> Result<Browse> {
        let command = if recursive { "listallinfo" } else { "lsinfo" };
        let reply = self.request(&format!("{command} {}", quote(path)?)).await?;
        let directories = String::from_utf8_lossy(&reply)
            .lines()
            .filter_map(|l| l.strip_prefix("directory: ").map(|p| format!("{ROOT}/{p}")))
            .collect();
        Ok(Browse {
            directories,
            tracks: tracks(&reply),
        })
    }
    pub async fn resolve(&self, uri: &str) -> Result<Vec<TrackInfo>> {
        let file = uri.strip_prefix("mpd:").unwrap_or(uri);
        ensure!(!file.is_empty(), "empty local file URI");
        quote(file)?;
        if file.starts_with('/')
            || file.starts_with("file://")
            || file.starts_with("http://")
            || file.starts_with("https://")
        {
            let path = if file.starts_with("file://") {
                url::Url::parse(file)?
                    .to_file_path()
                    .map_err(|_| anyhow::anyhow!("invalid file URL"))?
                    .to_string_lossy()
                    .into_owned()
            } else {
                file.into()
            };
            if path.starts_with('/') {
                ensure!(
                    tokio::fs::metadata(&path)
                        .await
                        .context("local file not found")?
                        .is_file(),
                    "expected a local audio file"
                );
            }
            return Ok(tracks(format!("file: {path}\n").as_bytes()));
        }
        let reply = self
            .request(&format!("listallinfo {}", quote(file)?))
            .await?;
        let found = tracks(&reply);
        ensure!(
            !found.is_empty(),
            "MPD file or directory has no indexed songs"
        );
        Ok(found)
    }
    pub async fn playlists(&self) -> Result<Vec<String>> {
        let reply = self.request("listplaylists").await?;
        Ok(String::from_utf8_lossy(&reply)
            .lines()
            .filter_map(|l| {
                l.strip_prefix("playlist: ")
                    .map(|name| format!("{PLAYLISTS}{name}"))
            })
            .collect())
    }
    pub async fn playlist(&self, name: &str) -> Result<Vec<TrackInfo>> {
        Ok(tracks(
            &self
                .request(&format!("listplaylistinfo {}", quote(name)?))
                .await?,
        ))
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        self.done.send_replace(true);
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    name: String,
    origin: String,
    enabled: bool,
    volume: Option<u8>,
}
struct PlayerState {
    wire: Wire,
    lease: Option<Lease>,
    current: Option<String>,
    observed: bool,
    ended: bool,
}
pub struct Player {
    state: Mutex<PlayerState>,
    generation: AtomicU64,
    closed: AtomicBool,
    tx: mpsc::UnboundedSender<AudioUpdate>,
    done: watch::Sender<bool>,
    lease_path: PathBuf,
}
impl Player {
    pub async fn connect(
        settings: Settings,
        tx: mpsc::UnboundedSender<AudioUpdate>,
    ) -> Result<Arc<Self>> {
        let mut wire = Wire::open(&settings).await?;
        let partitions = wire.command("listpartitions").await?;
        if !String::from_utf8_lossy(&partitions)
            .lines()
            .any(|l| l == format!("partition: {PARTITION}"))
        {
            wire.command(&format!("newpartition {}", quote(PARTITION)?))
                .await?;
        }
        wire.command(&format!("partition {}", quote(PARTITION)?))
            .await?;
        let mut monitor = Wire::open(&settings).await?;
        monitor
            .command(&format!("partition {}", quote(PARTITION)?))
            .await?;
        let lease_path = crate::paths::state().join("mpd-output-lease.json");
        let lease = tokio::fs::read(&lease_path)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let (done, _) = watch::channel(false);
        let player = Arc::new(Self {
            state: Mutex::new(PlayerState {
                wire,
                lease,
                current: None,
                observed: false,
                ended: false,
            }),
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            tx,
            done,
            lease_path,
        });
        let weak = Arc::downgrade(&player);
        let mut stop = player.done.subscribe();
        tokio::spawn(async move {
            loop {
                // idle has no timeout: MPD pushes state changes. Progress is
                // interpolated by Engine between these anchors.
                let result = async {
                    monitor.stream.write_all(b"idle player mixer\n").await?;
                    monitor.stream.flush().await?;
                    monitor.response().await
                };
                let reply = tokio::select! { _=stop.changed()=>break, reply=result=>reply };
                let Some(player) = weak.upgrade() else { break };
                if let Err(error) = reply {
                    player.closed.store(true, Ordering::Relaxed);
                    player.emit(
                        player.generation.load(Ordering::Relaxed),
                        AudioEvent::Error(format!("local MPD disconnected: {error}")),
                    );
                    break;
                }
                if let Err(error) = player.observe().await {
                    player.closed.store(true, Ordering::Relaxed);
                    player.emit(
                        player.generation.load(Ordering::Relaxed),
                        AudioEvent::Error(error.to_string()),
                    );
                    break;
                }
            }
        });
        Ok(player)
    }
    fn emit(&self, epoch: u64, event: AudioEvent) {
        let _ = self.tx.send(AudioUpdate {
            backend: Backend::Mpd,
            epoch,
            event,
        });
    }
    pub fn invalid(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
    async fn claim(&self, p: &mut PlayerState) -> Result<()> {
        if let Some(lease) = &p.lease {
            let lease = lease.clone();
            p.wire
                .command(&format!("partition {}", quote(PARTITION)?))
                .await?;
            // A saved lease can survive a crash or an MPD restart. Reclaim
            // the recorded output if it has returned to the default partition.
            let mut values = outputs(&p.wire.command("outputs").await?);
            if !values.iter().any(|o| {
                o.get("outputname") == Some(&lease.name)
                    && o.get("plugin").is_none_or(|p| p != "dummy")
            }) {
                p.wire
                    .command(&format!("moveoutput {}", quote(&lease.name)?))
                    .await?;
                values = outputs(&p.wire.command("outputs").await?);
            }
            let output = values
                .iter()
                .find(|o| o.get("outputname") == Some(&lease.name))
                .context("MPD output is missing")?;
            p.wire
                .command(&format!(
                    "enableoutput {}",
                    output.get("outputid").context("MPD output has no ID")?
                ))
                .await?;
            return Ok(());
        }
        p.wire.command("partition \"default\"").await?;
        let status = fields(&p.wire.command("status").await?);
        let reply = p.wire.command("outputs").await?;
        let wanted = std::env::var("RSPC_MPD_OUTPUT").ok();
        let output = outputs(&reply)
            .into_iter()
            .find(|r| {
                r.get("plugin").is_none_or(|p| p != "dummy")
                    && wanted
                        .as_ref()
                        .map_or(r.get("outputenabled").is_some_and(|v| v == "1"), |n| {
                            r.get("outputname") == Some(n)
                        })
            })
            .context("No enabled MPD output in the default partition; configure RSPC_MPD_OUTPUT")?;
        let lease = Lease {
            name: output
                .get("outputname")
                .context("MPD output has no name")?
                .clone(),
            origin: "default".into(),
            enabled: output.get("outputenabled").is_some_and(|v| v == "1"),
            volume: status.get("volume").and_then(|v| v.parse().ok()),
        };
        if status.get("state").is_some_and(|s| s == "play") {
            p.wire.command("pause 1").await?;
        }
        crate::paths::write_private_file_atomic(&self.lease_path, &serde_json::to_vec(&lease)?)?;
        p.lease = Some(lease.clone());
        p.wire
            .command(&format!("partition {}", quote(PARTITION)?))
            .await?;
        p.wire
            .command(&format!("moveoutput {}", quote(&lease.name)?))
            .await?;
        let reply = p.wire.command("outputs").await?;
        let output = outputs(&reply)
            .into_iter()
            .find(|o| o.get("outputname") == Some(&lease.name))
            .context("MPD output move failed")?;
        let id = output.get("outputid").context("MPD output move failed")?;
        p.wire.command(&format!("enableoutput {id}")).await?;
        Ok(())
    }
    pub async fn load(&self, uri: &str, request: u64, volume: u8) -> Result<()> {
        let file = uri.strip_prefix("mpd:").context("expected an MPD URI")?;
        let mut p = self.state.lock().await;
        self.generation.store(request, Ordering::Relaxed);
        p.current = Some(format!("mpd:{file}"));
        p.observed = false;
        p.ended = false;
        self.claim(&mut p).await?;
        for command in [
            "stop".to_string(),
            "clear".into(),
            "repeat 0".into(),
            "random 0".into(),
            "single 1".into(),
            "consume 0".into(),
            format!("addid {}", quote(file)?),
            format!("setvol {volume}"),
            "play 0".into(),
        ] {
            p.wire.command(&command).await?;
        }
        drop(p);
        self.observe().await
    }
    async fn observe(&self) -> Result<()> {
        let mut p = self.state.lock().await;
        let request = self.generation.load(Ordering::Relaxed);
        if request == 0 || p.current.is_none() {
            return Ok(());
        }
        let status = fields(&p.wire.command("status").await?);
        if let Some(error) = status.get("error").filter(|e| !e.is_empty()) {
            self.emit(request, AudioEvent::Error(error.clone()));
            return Ok(());
        }
        let song = tracks(&p.wire.command("currentsong").await?)
            .into_iter()
            .next();
        if let Some(track) = song {
            if track.uri != p.current {
                bail!("MPD rspc partition was changed by another client");
            }
            self.emit(request, AudioEvent::Metadata { request, track });
        }
        if let Some(volume) = status.get("volume").and_then(|s| s.parse::<u8>().ok()) {
            self.emit(request, AudioEvent::Volume(volume.min(100)));
        }
        let state = status.get("state").map(String::as_str).unwrap_or("stop");
        let position = status
            .get("elapsed")
            .and_then(|n| n.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map_or(0, |n| (n * 1000.0) as u64);
        if state == "play" || state == "pause" {
            p.observed = true;
            self.emit(
                request,
                AudioEvent::State {
                    request,
                    position,
                    playing: state == "play",
                },
            );
        } else if p.observed && !p.ended {
            p.ended = true;
            self.emit(request, AudioEvent::Ended { request });
        }
        Ok(())
    }
    pub async fn pause(&self, paused: bool) -> Result<()> {
        self.state
            .lock()
            .await
            .wire
            .command(if paused { "pause 1" } else { "pause 0" })
            .await?;
        self.observe().await
    }
    pub async fn seek(&self, position: u64) -> Result<()> {
        self.state
            .lock()
            .await
            .wire
            .command(&format!("seekcur {:.3}", position as f64 / 1000.0))
            .await?;
        self.observe().await
    }
    pub async fn volume(&self, volume: u8) -> Result<()> {
        self.state
            .lock()
            .await
            .wire
            .command(&format!("setvol {volume}"))
            .await?;
        Ok(())
    }
    pub async fn stop(&self) -> Result<()> {
        let mut p = self.state.lock().await;
        self.generation.store(0, Ordering::Relaxed);
        p.current = None;
        p.observed = false;
        p.ended = true;
        p.wire
            .command(&format!("partition {}", quote(PARTITION)?))
            .await?;
        p.wire.command("stop").await?;
        if let Some(lease) = p.lease.clone() {
            p.wire
                .command(&format!("partition {}", quote(&lease.origin)?))
                .await?;
            let owned = outputs(&p.wire.command("outputs").await?).iter().any(|o| {
                o.get("outputname") == Some(&lease.name)
                    && o.get("plugin").is_none_or(|p| p != "dummy")
            });
            if !owned {
                p.wire
                    .command(&format!("moveoutput {}", quote(&lease.name)?))
                    .await?;
            }
            let reply = p.wire.command("outputs").await?;
            let output = outputs(&reply)
                .into_iter()
                .find(|o| o.get("outputname") == Some(&lease.name))
                .context("MPD output return failed")?;
            if !lease.enabled
                && let Some(id) = output.get("outputid")
            {
                p.wire.command(&format!("disableoutput {id}")).await?;
            }
            if let Some(volume) = lease.volume {
                p.wire.command(&format!("setvol {volume}")).await?;
            }
            p.lease = None;
            let _ = tokio::fs::remove_file(&self.lease_path).await;
        }
        p.wire
            .command(&format!("partition {}", quote(PARTITION)?))
            .await?;
        Ok(())
    }
    pub async fn shutdown(&self) {
        let _ = self.stop().await;
        self.done.send_replace(true);
    }
    pub fn diagnostics(&self) -> String {
        format!(
            "mpd_connected: {}\nmpd_partition: {PARTITION}\n",
            u8::from(!self.invalid())
        )
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.done.send_replace(true);
    }
}

#[cfg(test)]
mod tests;
