mod catalog;
mod engine;
mod filter;
mod local;
mod protocol;
mod queue;
use anyhow::{Context, Result};
use std::{
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{net::UnixListener, task::JoinSet};
pub struct DaemonOptions {
    pub config: Option<PathBuf>,
    pub socket: PathBuf,
    pub queue: PathBuf,
    pub demo: bool,
}
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!(
                "{} [{}] {}: {}",
                chrono::Utc::now().to_rfc3339(),
                r.level(),
                r.target(),
                r.args()
            );
        }
    }
    fn flush(&self) {}
}
static LOGGER: Logger = Logger;
pub async fn run_daemon(options: DaemonOptions) -> Result<()> {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(options.socket.with_extension("lock"))?;
    lock.try_lock().context("rspc daemon is already running")?;
    if options.socket.exists() {
        anyhow::ensure!(
            options.socket.symlink_metadata()?.file_type().is_socket(),
            "refusing to replace a non-socket path"
        );
        std::fs::remove_file(&options.socket)?;
    }
    let listener = UnixListener::bind(&options.socket)?;
    std::fs::set_permissions(&options.socket, std::fs::Permissions::from_mode(0o600))?;
    let _socket = SocketGuard(options.socket);
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    let engine = engine::Engine::new(options.demo, options.queue, tx).await?;
    engine
        .catalog
        .playlist_settings
        .lock()
        .unwrap()
        .configure(options.config)?;
    if !options.demo {
        engine.watch_local();
    }
    if !options.demo && crate::credentials_available()? {
        let catalog = Arc::clone(&engine.catalog);
        let notify = Arc::clone(&engine);
        tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            loop {
                let progress = catalog.warm().await;
                if progress.changed {
                    notify.changed(vec!["database"]);
                }
                let delay = if progress.pending { 30 } else { 15 * 60 };
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        });
    }
    if !options.demo {
        let audio = Arc::clone(&engine);
        tasks.spawn(async move {
            if let Err(e) = audio.prepare_audio().await {
                log::warn!("audio setup: {e}");
            }
        });
    }
    let mut shutdown = engine.shutdown.subscribe();
    let persist = Arc::clone(&engine);
    let (persist_stop, mut persist_end) = tokio::sync::oneshot::channel::<()>();
    let persistence = tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                _ = &mut persist_end => break,
                _ = clock.tick() => {
                    if let Err(error) = persist.flush().await {
                        log::error!("saving queue: {error}");
                    }
                }
            }
        }
    });
    let updates = Arc::clone(&engine);
    tasks.spawn(async move {
        while let Some(update) = events.recv().await {
            if let Err(error) = updates
                .audio_event(update.backend, update.epoch, update.event)
                .await
            {
                log::error!("player event: {error}");
            }
        }
    });
    let maintenance = Arc::clone(&engine);
    tasks.spawn(async move {
        let interval = if options.demo {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        let mut clock = tokio::time::interval(interval);
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            clock.tick().await;
            if let Err(error) = maintenance.tick().await {
                log::error!("player tick: {error}");
            }
        }
    });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let result = loop {
        tokio::select! {
            connection = listener.accept() => {
                let (connection, _) = match connection {
                    Ok(connection) => connection,
                    Err(error) => break Err(error.into()),
                };
                let engine = Arc::clone(&engine);
                tasks.spawn(async move {
                    if let Err(error) = protocol::serve(connection, engine).await {
                        log::debug!("frontend disconnected: {error}");
                    }
                });
            }
            Some(result) = tasks.join_next() => {
                if let Err(error) = result {
                    break Err(error.into());
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break Ok(()); }
            }
            _ = term.recv() => break Ok(()),
            result = &mut interrupt => break result.map_err(Into::into),
        }
    };
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let _ = persist_stop.send(());
    let _ = persistence.await;
    engine.stop().await;
    engine.mark_dirty();
    let saved = engine.flush().await;
    engine.shutdown_audio().await;
    drop(lock);
    result.and(saved)
}
