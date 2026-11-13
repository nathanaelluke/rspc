use super::{
    catalog::Catalog,
    queue::{Entry, Queue},
};
use crate::{
    playback::{AudioEvent, AudioUpdate, Backend},
    soloist::Soloist,
};
use anyhow::{Context, Result, ensure};
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
    pub position_ms: u64,
    pub last_tick: Instant,
    pub pending_ids: Vec<u32>,
    pub error: String,
    pub backend: Option<Backend>,
    request: Option<u64>,
    external: bool,
    loading: Option<Instant>,
}
impl Player {
    pub fn playback(&self) -> Playback {
        let entry = self
            .queue
            .cursor()
            .and_then(|i| self.queue.entries.get(i).cloned());
        let position = self
            .position_ms
            .saturating_add(if self.playing && !self.stopped {
                self.last_tick.elapsed().as_millis() as u64
            } else {
                0
            });
        Playback {
            position_ms: if self.stopped {
                0
            } else {
                match entry.as_ref().map_or(0, |e| e.track.duration_ms) {
                    0 => position,
                    duration => position.min(duration),
                }
            },
            entry,
            playing: self.playing && !self.stopped,
            stopped: self.stopped,
            volume: self.volume,
            error: self.error.clone(),
        }
    }
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

enum PlayTarget {
    Position(usize),
    Id(u32),
}

impl PlayTarget {
    fn index(&self, queue: &Queue) -> Result<usize> {
        match *self {
            Self::Position(index) => Ok(index),
            Self::Id(id) => queue.index(id),
        }
    }
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
                position_ms: 0,
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
        self.player.lock().await.playback()
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
    pub async fn login_spotify(&self) -> Result<String> {
        let _control = self.control.lock().await;
        ensure!(
            !self.catalog.demo,
            "Spotify login is unavailable in demo mode"
        );
        let session = crate::spotify::connection::catalog_for_pairing().await?;
        self.soloist().await?.authenticate(&session).await?;
        crate::spotify::connection::check_pairing().await?;
        Ok("spotify_login: verified\n".into())
    }
    pub async fn play(&self, index: usize) -> Result<()> {
        self.play_target(PlayTarget::Position(index)).await
    }

    pub async fn play_id(&self, id: u32) -> Result<()> {
        self.play_target(PlayTarget::Id(id)).await
    }

    async fn play_target(&self, target: PlayTarget) -> Result<()> {
        let _control = self.control.lock().await;
        let result = self.play_inner(target).await;
        if let Err(error) = &result {
            self.player.lock().await.error = format!("{error:#}");
            self.changed(vec!["player"]);
        }
        result
    }
    // Playback and queue mutations share the control lock. State reads only
    // need the short-lived player lock, even while a backend is connecting.
    async fn play_inner(&self, target: PlayTarget) -> Result<()> {
        self.play_inner_at(target, 0).await
    }

    async fn play_inner_at(&self, target: PlayTarget, position_ms: u64) -> Result<()> {
        let (entry, sync_spotify_volume, volume) = {
            let p = self.player.lock().await;
            let index = target.index(&p.queue)?;
            (
                p.queue
                    .entries
                    .get(index)
                    .context("queue position out of range")?
                    .clone(),
                // Deferred paused selections have no active backend, but keep
                // the shared volume when their replacement is finally loaded.
                p.backend == Some(Backend::Mpd) || (!p.stopped && p.backend.is_none()),
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
                    if sync_spotify_volume {
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
            p.position_ms = position_ms;
            p.last_tick = Instant::now();
            p.loading = (!self.catalog.demo).then(Instant::now);
            p.error.clear();
            p.external = false;
            p.backend = Some(backend);
            p.request = request;
        }
        if let Some(soloist) = soloist {
            let result = async {
                soloist.load(uri).await?;
                if position_ms > 0 {
                    soloist
                        .seek(position_ms.min(u32::MAX as u64) as u32)
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                self.fail_inner(error.to_string()).await;
                return Err(error);
            }
        }
        if let Some(local) = local {
            let request = request.context("missing local playback request")?;
            if let Err(error) = local.load(uri, request, volume, position_ms).await {
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
        let _control = self.control.lock().await;
        let result = self.pause_inner(paused).await;
        if let Err(error) = &result {
            self.player.lock().await.error = format!("{error:#}");
            self.changed(vec!["player"]);
        }
        result
    }

    async fn pause_inner(&self, paused: Option<bool>) -> Result<()> {
        let state = self.playback().await;
        let pause = paused.unwrap_or(state.playing);
        let (backend, request) = {
            let p = self.player.lock().await;
            (p.backend, p.request)
        };
        if !pause && (state.stopped || backend.is_none()) {
            let index = self.player.lock().await.queue.cursor().unwrap_or(0);
            let position_ms = if state.stopped { 0 } else { state.position_ms };
            self.play_inner_at(PlayTarget::Position(index), position_ms)
                .await?;
            return Ok(());
        }
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
            p.position_ms = latest.position_ms;
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
            p.position_ms = 0;
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
    async fn fail_inner(&self, error: String) {
        self.stop_inner().await;
        self.player.lock().await.error = error;
        self.changed(vec!["player"]);
    }
    pub async fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        anyhow::ensure!(seconds.is_finite(), "invalid seek position");
        let _control = self.control.lock().await;
        self.seek_inner(seconds, relative).await
    }

    pub async fn seek_id(&self, id: u32, seconds: f64) -> Result<()> {
        self.seek_target(PlayTarget::Id(id), seconds).await
    }

    pub async fn seek_position(&self, index: usize, seconds: f64) -> Result<()> {
        self.seek_target(PlayTarget::Position(index), seconds).await
    }

    async fn seek_target(&self, target: PlayTarget, seconds: f64) -> Result<()> {
        anyhow::ensure!(seconds.is_finite(), "invalid seek position");
        let _control = self.control.lock().await;
        let id = {
            let p = self.player.lock().await;
            let entry = p
                .queue
                .entries
                .get(target.index(&p.queue)?)
                .context("queue position out of range")?;
            anyhow::ensure!(
                entry.track.duration_ms > 0,
                "this source has no known seekable duration"
            );
            (p.queue.current != Some(entry.id)).then_some(entry.id)
        };
        if let Some(id) = id {
            self.play_inner(PlayTarget::Id(id)).await?;
        }
        self.seek_inner(seconds, false).await
    }

    async fn seek_inner(&self, seconds: f64, relative: bool) -> Result<()> {
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
            p.position_ms = ms;
            p.last_tick = Instant::now();
        }
        drop(p);
        self.changed(vec!["player"]);
        Ok(())
    }
    pub async fn volume(&self, value: i64, relative: bool) {
        let _control = self.control.lock().await;
        let mut p = self.player.lock().await;
        p.volume = value
            .saturating_add(if relative { i64::from(p.volume) } else { 0 })
            .clamp(0, 100) as u8;
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
        let _control = self.control.lock().await;
        let result = self.next_inner(previous).await;
        if let Err(error) = &result {
            self.player.lock().await.error = format!("{error:#}");
            self.changed(vec!["player"]);
        }
        result
    }

    async fn next_inner(&self, previous: bool) -> Result<()> {
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
            self.play_inner(PlayTarget::Position(i)).await?;
        } else {
            self.stop_inner().await;
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

    pub async fn clear_queue(&self) {
        let _control = self.control.lock().await;
        self.stop_inner().await;
        {
            let mut player = self.player.lock().await;
            player.queue.entries.clear();
            player.queue.current = None;
            player.queue.changed();
        }
        self.queue_changed().await;
    }

    pub async fn edit_queue<T>(&self, edit: impl FnOnce(&mut Queue) -> Result<T>) -> Result<T> {
        let _control = self.control.lock().await;
        let (value, changed_current, current, stopped, paused) = {
            let mut p = self.player.lock().await;
            let old = p.queue.current;
            let value = edit(&mut p.queue)?;
            (
                value,
                old != p.queue.current,
                p.queue.current,
                p.stopped,
                !p.playing && p.loading.is_none(),
            )
        };
        let transition = if changed_current {
            match current.filter(|_| !stopped) {
                Some(_) if paused => {
                    // Selecting a replacement must not briefly start audio.
                    // Resume will load it and apply any seek made while paused.
                    self.stop_inner().await;
                    self.player.lock().await.stopped = false;
                    self.changed(vec!["player"]);
                    Ok(())
                }
                Some(id) => self.play_inner(PlayTarget::Id(id)).await,
                None => {
                    self.stop_inner().await;
                    Ok(())
                }
            }
        } else {
            Ok(())
        };
        if let Err(error) = &transition {
            self.fail_inner(format!("{error:#}")).await;
        }
        // Persist edits even when the replacement track cannot start.
        self.queue_changed().await;
        transition?;
        Ok(value)
    }
    pub async fn queue_changed(&self) {
        self.sync_pending().await;
        self.mark_dirty();
        self.changed(vec!["playlist"]);
    }
    #[cfg(test)]
    async fn finished(&self) -> Result<()> {
        let _control = self.control.lock().await;
        let result = self.finish_queue().await;
        if let Err(error) = &result {
            self.fail_inner(format!("{error:#}")).await;
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
                self.play_inner(PlayTarget::Position(index)).await?;
            } else {
                self.stop_inner().await;
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
            self.next_inner(false).await?;
        }
        Ok(())
    }
    pub async fn audio_event(&self, backend: Backend, epoch: u64, event: AudioEvent) -> Result<()> {
        if backend == Backend::Mpd && matches!(event, AudioEvent::LibraryChanged) {
            self.catalog.local.invalidate().await;
            self.changed(vec!["database", "stored_playlist", "update"]);
            return Ok(());
        }
        if backend == Backend::Spotify && self.soloist_epoch.load(Ordering::Relaxed) != epoch {
            return Ok(());
        }
        if matches!(event, AudioEvent::Inactive) {
            let player = self.player.lock().await;
            if player.stopped || player.loading.is_some() || player.backend == Some(Backend::Mpd) {
                return Ok(());
            }
        }
        let _control = if matches!(
            event,
            AudioEvent::Ended { .. } | AudioEvent::Error(_) | AudioEvent::Inactive
        ) || backend == Backend::Spotify
            && matches!(event, AudioEvent::ExternalState { playing: true, .. })
        {
            Some(self.control.lock().await)
        } else {
            None
        };
        // Check after acquiring control: a replacement player may have started
        // while this event was waiting behind another transition.
        if backend == Backend::Spotify && self.soloist_epoch.load(Ordering::Relaxed) != epoch {
            return Ok(());
        }
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
                p.position_ms = position;
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
                p.position_ms = position;
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
                p.position_ms = position;
                p.last_tick = Instant::now();
            }
            AudioEvent::Ended { request }
                if p.request == Some(request) && !p.stopped && p.loading.is_none() =>
            {
                drop(p);
                if let Err(error) = self.finish_queue().await {
                    self.fail_inner(format!("{error:#}")).await;
                    return Err(error);
                }
            }
            AudioEvent::Error(error) => {
                drop(p);
                self.fail_inner(format!(
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
            let _control = self.control.lock().await;
            let s = self.playback().await;
            if s.playing
                && s.entry
                    .is_some_and(|e| s.position_ms >= e.track.duration_ms)
                && let Err(error) = self.finish_queue().await
            {
                self.fail_inner(format!("{error:#}")).await;
                return Err(error);
            }
        } else {
            let _control = self.control.lock().await;
            let timeout = self
                .player
                .lock()
                .await
                .loading
                .is_some_and(|at| at.elapsed() > Duration::from_secs(20));
            if timeout {
                self.fail_inner("Audio did not start within 20 seconds. Check rspc doctor; retry play to reconnect.".into()).await;
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
        let catalog_auth = crate::spotify::connection::description();
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
            "engine: rspc\nactive_source: {active}\nqueue_control: {queue_control}\ncatalog_auth: {catalog_auth}\nplayer_request: {request}\nloading: {}\n{}{local}error: {error}\n",
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
