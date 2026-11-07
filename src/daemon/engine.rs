use super::{
    catalog::Catalog,
    queue::{Entry, Queue},
};
use crate::{
    playback::{AudioEvent, AudioUpdate, Backend},
    soloist::Soloist,
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
#[derive(Clone, Debug)]
pub struct Playback {
    pub entry: Option<Entry>,
    pub position_ms: u64,
    pub playing: bool,
    pub stopped: bool,
    pub volume: u8,
    pub error: String,
}
pub struct Player {
    pub queue: Queue,
    pub repeat: bool,
    pub random: bool,
    pub single: u8,
    pub stopped: bool,
    pub playing: bool,
    pub volume: u8,
    pub demo_position: u64,
    pub last_tick: Instant,
    pub pending_ids: Vec<u32>,
    pub error: String,
    pub backend: Option<Backend>,
    request: Option<u64>,
    external: bool,
    loading: Option<Instant>,
}
pub struct Engine {
    pub catalog: Arc<Catalog>,
    pub art_cache: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    pub player: Mutex<Player>,
    pub events: watch::Sender<HashMap<&'static str, u64>>,
    pub shutdown: watch::Sender<bool>,
    pub queue_path: PathBuf,
    soloist: RwLock<Option<Arc<Soloist>>>,
    soloist_init: Mutex<()>,
    soloist_epoch: AtomicU64,
    local_audio: RwLock<Option<Arc<super::local::Player>>>,
    local_request: AtomicU64,
    control: Mutex<()>,
    tx: mpsc::UnboundedSender<AudioUpdate>,
    dirty: AtomicBool,
}
impl Engine {
    pub async fn new(
        demo: bool,
        queue_path: PathBuf,
        tx: mpsc::UnboundedSender<AudioUpdate>,
    ) -> Result<Arc<Self>> {
        let mut queue = Queue::load(&queue_path)?;
        let catalog = Arc::new(Catalog::new(demo));
        catalog
            .remember(queue.entries.iter().map(|e| e.track.clone()).collect())
            .await;
        if demo && queue.entries.is_empty() {
            queue.add(super::catalog::demo_tracks(), 0)?;
        }
        let (events, _) = watch::channel(HashMap::new());
        let (shutdown, _) = watch::channel(false);
        Ok(Arc::new(Self {
            catalog,
            art_cache: Mutex::new(HashMap::new()),
            player: Mutex::new(Player {
                queue,
                repeat: false,
                random: false,
                single: 0,
                stopped: true,
                playing: false,
                volume: 100,
                demo_position: 0,
                last_tick: Instant::now(),
                pending_ids: Vec::new(),
                error: String::new(),
                backend: None,
                request: None,
                external: false,
                loading: None,
            }),
            events,
            shutdown,
            queue_path,
            soloist: RwLock::new(None),
            soloist_init: Mutex::new(()),
            soloist_epoch: AtomicU64::new(0),
            local_audio: RwLock::new(None),
            local_request: AtomicU64::new(0),
            control: Mutex::new(()),
            tx,
            dirty: AtomicBool::new(false),
        }))
    }
    pub fn changed(&self, subs: Vec<&'static str>) {
        self.events.send_modify(|v| {
            for s in subs {
                let n = v.entry(s).or_default();
                *n = n.wrapping_add(1);
            }
        });
    }
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }
    pub async fn flush(&self) -> Result<()> {
        if self.dirty.swap(false, Ordering::Relaxed) {
            let queue = self.player.lock().await.queue.clone();
            let path = self.queue_path.clone();
            let result = tokio::task::spawn_blocking(move || queue.save(&path))
                .await
                .context("queue persistence task failed")
                .and_then(|result| result);
            if result.is_err() {
                self.dirty.store(true, Ordering::Relaxed);
            }
            result?;
        }
        Ok(())
    }
    pub async fn clear_error(&self) {
        self.player.lock().await.error.clear();
        self.changed(vec!["player"]);
    }
    pub async fn playback(&self) -> Playback {
        let p = self.player.lock().await;
        let entry = p
            .queue
            .cursor()
            .and_then(|i| p.queue.entries.get(i).cloned());
        let position = p.demo_position
            + if p.playing && !p.stopped {
                p.last_tick.elapsed().as_millis() as u64
            } else {
                0
            };
        Playback {
            position_ms: if p.stopped {
                0
            } else {
                match entry.as_ref().map_or(0, |e| e.track.duration_ms) {
                    0 => position,
                    duration => position.min(duration),
                }
            },
            entry,
            playing: p.playing && !p.stopped,
            stopped: p.stopped,
            volume: p.volume,
            error: p.error.clone(),
        }
    }
    async fn soloist(&self) -> Result<Arc<Soloist>> {
        if let Some(n) = self.soloist.read().await.as_ref().filter(|n| !n.invalid()) {
            return Ok(Arc::clone(n));
        }
        let _guard = self.soloist_init.lock().await;
        if let Some(n) = self.soloist.read().await.as_ref().filter(|n| !n.invalid()) {
            return Ok(Arc::clone(n));
        }
        self.soloist.write().await.take();
        let epoch = self.soloist_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let n = Arc::new(Soloist::connect(epoch, self.tx.clone()).await?);
        *self.soloist.write().await = Some(Arc::clone(&n));
        Ok(n)
    }
    async fn local(&self) -> Result<Arc<super::local::Player>> {
        if let Some(player) = self
            .local_audio
            .read()
            .await
            .as_ref()
            .filter(|p| !p.invalid())
        {
            return Ok(Arc::clone(player));
        }
        let mut slot = self.local_audio.write().await;
        if let Some(player) = slot.as_ref().filter(|p| !p.invalid()) {
            return Ok(Arc::clone(player));
        }
        if let Some(old) = slot.take() {
            old.shutdown().await;
        }
        let player =
            super::local::Player::connect(self.catalog.local.settings.clone(), self.tx.clone())
                .await?;
        *slot = Some(Arc::clone(&player));
        Ok(player)
    }
    pub async fn sync_pending(&self) {
        let mut p = self.player.lock().await;
        let mut ids = match p.queue.cursor().filter(|_| !p.stopped) {
            None => Vec::new(),
            Some(index) if p.single != 0 => {
                if p.repeat {
                    vec![p.queue.entries[index].id]
                } else {
                    Vec::new()
                }
            }
            Some(index) => {
                let mut ids: Vec<_> = p.queue.entries[index + 1..]
                    .iter()
                    .map(|entry| entry.id)
                    .collect();
                if p.repeat {
                    ids.extend(p.queue.entries[..=index].iter().map(|entry| entry.id));
                }
                ids
            }
        };
        if p.random && ids.len() > 1 {
            use rand::seq::SliceRandom;
            ids.shuffle(&mut rand::rng());
        }
        p.pending_ids = ids;
    }
    pub async fn prepare_audio(&self) -> Result<String> {
        Ok(self.soloist().await?.diagnostics())
    }
    pub async fn play(&self, index: usize) -> Result<()> {
        let result = self.play_entry(index).await;
        if let Err(error) = &result {
            self.player.lock().await.error = format!("{error:#}");
            self.changed(vec!["player"]);
        }
        result
    }
    async fn play_entry(&self, index: usize) -> Result<()> {
        let _control = self.control.lock().await;
        let (entry, previous, volume) = {
            let p = self.player.lock().await;
            (
                p.queue
                    .entries
                    .get(index)
                    .context("queue position out of range")?
                    .clone(),
                p.backend,
                p.volume,
            )
        };
        let uri = entry.track.uri.as_deref().context("track has no URI")?;
        let backend = Backend::for_uri(uri)?;
        let mut soloist = None;
        let mut local = None;
        let request = if self.catalog.demo {
            None
        } else {
            Some(match backend {
                Backend::Spotify => {
                    let player = self.soloist().await?;
                    let request = player.reserve(uri, entry.track.duration_ms)?;
                    if previous == Some(Backend::Mpd) {
                        player.volume(volume).await?;
                    }
                    if let Some(local) = self.local_audio.read().await.as_ref() {
                        local.stop().await?;
                    }
                    soloist = Some(player);
                    request
                }
                Backend::Mpd => {
                    let player = self.local().await?;
                    let _setup = self.soloist_init.lock().await;
                    if let Some(spotify) = self.soloist.read().await.as_ref() {
                        spotify.suspend().await?;
                    }
                    local = Some(player);
                    self.local_request.fetch_add(1, Ordering::Relaxed) + 1
                }
            })
        };
        {
            let mut p = self.player.lock().await;
            p.queue.current = Some(entry.id);
            p.stopped = false;
            p.playing = self.catalog.demo;
            p.demo_position = 0;
            p.last_tick = Instant::now();
            p.loading = (!self.catalog.demo).then(Instant::now);
            p.error.clear();
            p.external = false;
            p.backend = Some(backend);
            p.request = request;
        }
        if let Some(soloist) = soloist
            && let Err(error) = soloist.load(uri).await
        {
            self.fail_inner(error.to_string()).await;
            return Err(error);
        }
        if let Some(local) = local {
            let request = request.context("missing local playback request")?;
            if let Err(error) = local.load(uri, request, volume).await {
                self.fail_inner(error.to_string()).await;
                return Err(error);
            }
        }
        self.sync_pending().await;
        self.mark_dirty();
        self.changed(vec!["player"]);
        Ok(())
    }
    pub async fn pause(&self, paused: Option<bool>) -> Result<()> {
        let control = self.control.lock().await;
        let state = self.playback().await;
        let pause = paused.unwrap_or(state.playing);
        if !pause && state.stopped {
            let index = self.player.lock().await.queue.cursor().unwrap_or(0);
            drop(control);
            return self.play(index).await;
        }
        let (backend, request) = {
            let p = self.player.lock().await;
            (p.backend, p.request)
        };
        match backend {
            Some(Backend::Mpd) => {
                let local = self.local_audio.read().await.clone();
                if let Some(local) = local {
                    local.pause(pause).await?;
                }
            }
            _ => {
                let soloist = self.soloist.read().await.clone();
                if let Some(n) = soloist {
                    if pause {
                        n.pause().await?
                    } else {
                        n.play().await?
                    }
                }
            }
        }
        let latest = self.playback().await;
        let mut p = self.player.lock().await;
        if p.backend == backend
            && p.request == request
            && p.queue.current == state.entry.as_ref().map(|entry| entry.id)
        {
            p.demo_position = latest.position_ms;
            p.last_tick = Instant::now();
            p.playing = !pause;
        }
        drop(p);
        self.changed(vec!["player"]);
        Ok(())
    }
    pub async fn stop(&self) {
        let _control = self.control.lock().await;
        self.stop_inner().await;
    }
    // The caller serializes playback transitions with the control lock.
    async fn stop_inner(&self) {
        {
            let mut p = self.player.lock().await;
            p.stopped = true;
            p.playing = false;
            p.loading = None;
            p.request = None;
            p.external = false;
            p.backend = None;
            p.demo_position = 0;
            p.pending_ids.clear();
        }
        let soloist = self.soloist.read().await.clone();
        if let Some(n) = soloist {
            n.stop().await;
        }
        let local = self.local_audio.read().await.clone();
        if let Some(local) = local
            && let Err(error) = local.stop().await
        {
            log::warn!("stopping local MPD: {error}");
        }
        self.changed(vec!["player"]);
    }
    async fn fail(&self, error: String) {
        let _control = self.control.lock().await;
        self.fail_inner(error).await;
    }
    async fn fail_inner(&self, error: String) {
        self.stop_inner().await;
        self.player.lock().await.error = error;
        self.changed(vec!["player"]);
    }
    pub async fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        anyhow::ensure!(seconds.is_finite(), "invalid seek position");
        let _control = self.control.lock().await;
        let state = self.playback().await;
        let duration = state.entry.as_ref().map_or(0, |e| e.track.duration_ms);
        anyhow::ensure!(duration > 0, "this source has no known seekable duration");
        let ms = (seconds * 1000.0
            + if relative {
                state.position_ms as f64
            } else {
                0.0
            })
        .max(0.0)
        .min(duration as f64) as u64;
        let (backend, request) = {
            let p = self.player.lock().await;
            (p.backend, p.request)
        };
        match backend {
            Some(Backend::Mpd) => {
                let local = self.local_audio.read().await.clone();
                if let Some(local) = local {
                    local.seek(ms).await?;
                }
            }
            _ => {
                let soloist = self.soloist.read().await.clone();
                if let Some(n) = soloist {
                    n.seek(ms.min(u32::MAX as u64) as u32).await?;
                }
            }
        }
        let mut p = self.player.lock().await;
        if p.backend == backend
            && p.request == request
            && p.queue.current == state.entry.as_ref().map(|entry| entry.id)
        {
            p.demo_position = ms;
            p.last_tick = Instant::now();
        }
        drop(p);
        self.changed(vec!["player"]);
        Ok(())
    }
    pub async fn volume(&self, value: i64, relative: bool) {
        let mut p = self.player.lock().await;
        p.volume = (value + if relative { i64::from(p.volume) } else { 0 }).clamp(0, 100) as u8;
        let volume = p.volume;
        let backend = p.backend;
        drop(p);
        let result = if backend == Some(Backend::Mpd) {
            let local = self.local_audio.read().await.clone();
            match local {
                Some(local) => local.volume(volume).await,
                None => Ok(()),
            }
        } else {
            let soloist = self.soloist.read().await.clone();
            match soloist {
                Some(soloist) if soloist.logged_in() => soloist.volume(volume).await,
                _ => Ok(()),
            }
        };
        if let Err(error) = result {
            self.player.lock().await.error = error.to_string();
        }
        self.changed(vec!["mixer"]);
    }
    pub async fn next(&self, previous: bool) -> Result<()> {
        if self.player.lock().await.external {
            // Spotify owns a context started from a phone or restored session.
            // Keep its next/previous controls until the user selects an rspc entry.
            self.soloist().await?.skip(previous).await?;
            return Ok(());
        }
        let (next, old) = {
            let p = self.player.lock().await;
            let i = p.queue.cursor().unwrap_or(0);
            let next = if previous {
                Some(i.saturating_sub(1))
            } else if p.random && p.single == 0 && !p.pending_ids.is_empty() {
                p.queue.index(p.pending_ids[0]).ok()
            } else if i + 1 < p.queue.entries.len() {
                Some(i + 1)
            } else if p.repeat && !p.queue.entries.is_empty() {
                Some(0)
            } else {
                None
            };
            (next, p.queue.current)
        };
        if let Some(i) = next {
            self.play(i).await?;
        } else {
            self.stop().await;
        }
        if !previous {
            let mut p = self.player.lock().await;
            if p.stopped || p.queue.current != old {
                p.queue.consume_finished(old);
            }
            drop(p);
            self.queue_changed().await;
            self.changed(vec!["options", "player", "playlist"]);
        }
        Ok(())
    }
    pub async fn queue_changed(&self) {
        self.sync_pending().await;
        self.mark_dirty();
        self.changed(vec!["playlist"]);
    }
    async fn finished(&self) -> Result<()> {
        let result = self.finish_queue().await;
        if let Err(error) = &result {
            self.fail(format!("{error:#}")).await;
        }
        result
    }
    async fn finish_queue(&self) -> Result<()> {
        let (single, repeat, index) = {
            let p = self.player.lock().await;
            (p.single, p.repeat, p.queue.cursor().unwrap_or(0))
        };
        if single != 0 {
            if repeat {
                self.play(index).await?;
            } else {
                self.stop().await;
                let mut p = self.player.lock().await;
                let old = p.queue.current;
                p.queue.consume_finished(old);
                if single == 2 {
                    p.single = 0;
                }
                drop(p);
                self.queue_changed().await;
                self.changed(vec!["options", "player", "playlist"]);
            }
        } else {
            self.next(false).await?;
        }
        Ok(())
    }
    pub async fn audio_event(&self, backend: Backend, epoch: u64, event: AudioEvent) -> Result<()> {
        if backend == Backend::Mpd && matches!(event, AudioEvent::LibraryChanged) {
            self.catalog.local.invalidate().await;
            self.changed(vec!["database", "stored_playlist", "update"]);
            return Ok(());
        }
        // Initial snapshots and volume/auth changes arrive before any local Play.
        // Still reject events from an old player after reconnect/replacement.
        if backend == Backend::Spotify && self.soloist_epoch.load(Ordering::Relaxed) != epoch {
            return Ok(());
        }
        let _control = if backend == Backend::Spotify
            && matches!(event, AudioEvent::ExternalState { playing: true, .. })
        {
            Some(self.control.lock().await)
        } else {
            None
        };
        let mut p = self.player.lock().await;
        if backend == Backend::Mpd && (p.backend != Some(Backend::Mpd) || p.request != Some(epoch))
        {
            return Ok(());
        }
        if backend == Backend::Spotify && p.backend == Some(Backend::Mpd) {
            if matches!(event, AudioEvent::ExternalState { playing: true, .. }) {
                drop(p);
                if !self
                    .soloist
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|n| n.is_playing())
                {
                    return Ok(());
                }
                if let Some(local) = self.local_audio.read().await.as_ref() {
                    local.stop().await?;
                }
                p = self.player.lock().await;
            } else {
                return Ok(());
            }
        }
        match event {
            AudioEvent::Metadata { request, track } if p.request == Some(request) => {
                let Some(index) = p.queue.cursor() else {
                    return Ok(());
                };
                if p.queue.entries[index].track.uri == track.uri
                    && p.queue.entries[index].track != track
                {
                    p.queue.entries[index].track = track.clone();
                    p.queue.changed();
                    drop(p);
                    self.catalog.remember(vec![track]).await;
                    self.mark_dirty();
                    self.changed(vec!["playlist", "player"]);
                }
            }
            AudioEvent::ExternalState {
                track,
                position,
                playing,
                stopped,
            } if p.loading.is_none() => {
                let index = p
                    .queue
                    .cursor()
                    .filter(|&i| p.queue.entries[i].track.uri == track.uri)
                    .or_else(|| {
                        p.queue
                            .entries
                            .iter()
                            .position(|e| e.track.uri == track.uri)
                    });
                let (index, added) = match index {
                    Some(i) => (i, false),
                    None => {
                        let end = p.queue.entries.len();
                        p.queue.add(vec![track.clone()], end)?;
                        (end, true)
                    }
                };
                let mut queue_changed = added;
                // Keep richer catalog tags and duplicate entry IDs when possible.
                let entry = &mut p.queue.entries[index];
                if track.duration_ms > 0 && entry.track.duration_ms != track.duration_ms {
                    entry.track.duration_ms = track.duration_ms;
                    queue_changed = true;
                }
                let entry = entry.clone();
                let first = !p.external || p.queue.current != Some(entry.id);
                p.queue.current = Some(entry.id);
                p.request = None;
                p.external = true;
                p.backend = Some(Backend::Spotify);
                p.pending_ids.clear();
                p.stopped = stopped;
                p.playing = playing && !stopped;
                p.demo_position = position;
                p.last_tick = Instant::now();
                p.error.clear();
                if queue_changed && !added {
                    p.queue.changed();
                }
                drop(p);
                self.catalog.remember(vec![entry.track.clone()]).await;
                if first && playing && !self.catalog.demo {
                    Self::record_play(entry.track);
                }
                self.mark_dirty();
                let mut subs = vec!["player"];
                if queue_changed {
                    subs.push("playlist");
                }
                self.changed(subs);
            }
            AudioEvent::Authenticated => {
                p.error.clear();
                drop(p);
                self.changed(vec!["player"]);
            }
            AudioEvent::State {
                request,
                position,
                playing,
            } if p.request == Some(request) => {
                let first = playing && p.loading.is_some();
                p.demo_position = position;
                p.last_tick = Instant::now();
                p.playing = playing;
                if playing {
                    p.loading = None;
                }
                let track = if first {
                    p.queue
                        .cursor()
                        .and_then(|i| p.queue.entries.get(i))
                        .map(|e| e.track.clone())
                } else {
                    None
                };
                drop(p);
                if let Some(track) = track.filter(|_| !self.catalog.demo) {
                    Self::record_play(track);
                }
                self.changed(vec!["player"]);
            }
            AudioEvent::Position { request, position }
                if p.request == Some(request) || (request == 0 && p.external && !p.stopped) =>
            {
                p.demo_position = position;
                p.last_tick = Instant::now();
            }
            AudioEvent::Ended { request }
                if p.request == Some(request) && !p.stopped && p.loading.is_none() =>
            {
                drop(p);
                self.finished().await?;
            }
            AudioEvent::Error(error) => {
                drop(p);
                self.fail(format!(
                    "{}: {error}",
                    if backend == Backend::Spotify {
                        "Soloist"
                    } else {
                        "MPD"
                    }
                ))
                .await;
                if self
                    .soloist
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|n| n.invalid())
                {
                    self.soloist.write().await.take();
                }
            }
            AudioEvent::Inactive if p.loading.is_none() => {
                p.stopped = true;
                p.playing = false;
                p.loading = None;
                p.request = None;
                p.external = false;
                p.backend = None;
                drop(p);
                self.changed(vec!["player"]);
            }
            AudioEvent::Volume(value) => {
                p.volume = value;
                drop(p);
                self.changed(vec!["mixer"]);
            }
            _ => {}
        }
        Ok(())
    }
    fn record_play(track: crate::model::TrackInfo) {
        let history = track.clone();
        tokio::task::spawn_blocking(move || crate::spotify::record(&history));
    }
    pub async fn tick(&self) -> Result<()> {
        match self.catalog.reload_playlist_settings().await {
            Ok(true) => self.changed(vec!["stored_playlist"]),
            Err(error) => log::warn!("playlist exclusions: {error:#}"),
            _ => {}
        }
        if self.catalog.demo {
            let s = self.playback().await;
            if s.playing
                && s.entry
                    .is_some_and(|e| s.position_ms >= e.track.duration_ms)
            {
                self.finished().await?;
            }
        } else {
            let timeout = self
                .player
                .lock()
                .await
                .loading
                .is_some_and(|at| at.elapsed() > Duration::from_secs(20));
            if timeout {
                self.fail("Audio did not start within 20 seconds. Check rspc doctor; retry play to reconnect.".into()).await;
            }
        }
        Ok(())
    }
    pub async fn shutdown_audio(&self) {
        if let Some(local) = self.local_audio.write().await.take() {
            local.shutdown().await;
        }
        if let Some(soloist) = self.soloist.write().await.take() {
            soloist.shutdown().await;
        }
    }
    pub fn watch_local(&self) {
        self.catalog.local.watch(self.tx.clone());
    }
    pub async fn diagnostics(&self) -> String {
        let p = self.player.lock().await;
        let loading = p.loading.is_some();
        let error = super::protocol::clean(&p.error);
        let active = match p.backend {
            Some(Backend::Spotify) => "spotify",
            Some(Backend::Mpd) => "mpd",
            None => "none",
        };
        let queue_control = if p.external { "spotify" } else { "rspc" };
        let request = p.request.unwrap_or(0);
        drop(p);
        let n = self.soloist.read().await;
        let detail = n
            .as_ref()
            .map(|n| n.diagnostics())
            .unwrap_or_else(|| "audio_engine: soloist\nlogged_in: 0\n".into());
        let local = self
            .local_audio
            .read()
            .await
            .as_ref()
            .map(|p| p.diagnostics())
            .unwrap_or_else(|| "mpd_connected: 0\n".into());
        format!(
            "engine: rspc\nactive_source: {active}\nqueue_control: {queue_control}\nplayer_request: {request}\nloading: {}\n{}{local}error: {error}\n",
            u8::from(loading),
            detail
        )
    }
    #[cfg(test)]
    async fn soloist_event(&self, epoch: u64, event: AudioEvent) -> Result<()> {
        self.audio_event(Backend::Spotify, epoch, event).await
    }
}

#[cfg(test)]
mod tests;
