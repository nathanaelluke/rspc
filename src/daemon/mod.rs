mod catalog;
mod engine;
mod filter;
mod local;
mod protocol;
mod queue;
use anyhow::{Context, Result};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
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
struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl SocketGuard {
    fn new(path: PathBuf) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn lock_path(socket: &Path) -> PathBuf {
    let mut path = socket.as_os_str().to_os_string();
    path.push(".lock");
    path.into()
}
async fn remove_stale_socket(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_socket(),
        "refusing to replace a non-socket path"
    );
    match tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::UnixStream::connect(path),
    )
    .await
    {
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
        Ok(Ok(_)) => anyhow::bail!("socket is already in use: {}", path.display()),
        Ok(Err(error)) => return Err(error).context("could not verify whether socket is stale"),
        Err(_) => anyhow::bail!(
            "timed out checking whether socket is in use: {}",
            path.display()
        ),
    }
    let current = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        current.file_type().is_socket()
            && current.dev() == metadata.dev()
            && current.ino() == metadata.ino(),
        "socket changed while checking whether it is stale"
    );
    std::fs::remove_file(path)?;
    Ok(())
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
        .open(lock_path(&options.socket))?;
    lock.try_lock().context("rspc daemon is already running")?;
    remove_stale_socket(&options.socket).await?;
    let listener = UnixListener::bind(&options.socket)?;
    let socket = SocketGuard::new(options.socket.clone())?;
    std::fs::set_permissions(&options.socket, std::fs::Permissions::from_mode(0o600))?;
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
    if !options.demo {
        let catalog = Arc::clone(&engine.catalog);
        let notify = Arc::clone(&engine);
        tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            loop {
                if !crate::credentials_available().unwrap_or(false) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                let progress = catalog.warm().await;
                if progress.changed {
                    notify.changed(vec!["database"]);
                }
                let delay = if progress.pending { 5 } else { 15 * 60 };
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        });
        tasks.spawn(async move {
            let mut clock = tokio::time::interval(Duration::from_secs(30));
            loop {
                clock.tick().await;
                if crate::credentials_available().unwrap_or(false)
                    && let Err(error) = crate::spotify::connection::maintain().await
                {
                    log::debug!("Spotify session maintenance: {error}");
                }
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
    drop(listener);
    drop(socket);
    drop(lock);
    result.and(saved)
}

#[cfg(test)]
mod startup_socket_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn options(dir: &Path, socket: &str) -> DaemonOptions {
        DaemonOptions {
            config: None,
            socket: dir.join(socket),
            queue: dir.join(format!("{socket}.json")),
            demo: true,
        }
    }

    async fn wait_ready(path: &Path) {
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon did not start: {}", path.display());
    }

    async fn shutdown(path: &Path) {
        let stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        stream
            .get_mut()
            .write_all(b"rspc_shutdown\n")
            .await
            .unwrap();
        line.clear();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, "OK\n");
    }

    #[tokio::test]
    async fn startup_socket_refuses_live_listener_and_reuses_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let inode = std::fs::symlink_metadata(&path).unwrap().ino();
        let error = run_daemon(options(dir.path(), "mpd.sock"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("socket is already in use"));
        assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), inode);
        assert!(tokio::net::UnixStream::connect(&path).await.is_ok());
        drop(listener);
        let daemon = tokio::spawn(run_daemon(options(dir.path(), "mpd.sock")));
        wait_ready(&path).await;
        shutdown(&path).await;
        daemon.await.unwrap().unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn startup_socket_guard_preserves_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpd.sock");
        let daemon = tokio::spawn(run_daemon(options(dir.path(), "mpd.sock")));
        wait_ready(&path).await;
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        stream
            .get_mut()
            .write_all(b"rspc_shutdown\n")
            .await
            .unwrap();
        daemon.await.unwrap().unwrap();
        assert!(tokio::net::UnixStream::connect(&path).await.is_ok());
        drop(replacement);
    }

    #[tokio::test]
    async fn startup_socket_names_have_distinct_locks() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("player.sock");
        let second = dir.path().join("player.socket");
        assert_ne!(lock_path(&first), lock_path(&second));
        let a = tokio::spawn(run_daemon(options(dir.path(), "player.sock")));
        let b = tokio::spawn(run_daemon(options(dir.path(), "player.socket")));
        wait_ready(&first).await;
        wait_ready(&second).await;
        shutdown(&first).await;
        a.await.unwrap().unwrap();
        assert!(tokio::net::UnixStream::connect(&second).await.is_ok());
        shutdown(&second).await;
        b.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn startup_socket_can_restart_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("player.sock");
        let a = tokio::spawn(run_daemon(options(dir.path(), "player.sock")));
        wait_ready(&path).await;
        shutdown(&path).await;
        a.await.unwrap().unwrap();
        let b = tokio::spawn(run_daemon(options(dir.path(), "player.sock")));
        wait_ready(&path).await;
        assert!(path.exists());
        shutdown(&path).await;
        b.await.unwrap().unwrap();
    }
}
