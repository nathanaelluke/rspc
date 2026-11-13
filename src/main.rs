use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rspc::paths::{ensure_private_dir, xdg};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Parser)]
#[command(
    version,
    about = "A terminal player for Spotify, local music and streams"
)]
struct Args {
    #[arg(
        long,
        global = true,
        help = "Use the isolated offline demonstration backend"
    )]
    demo: bool,
    #[arg(
        long,
        global = true,
        help = "Use your existing MPD and rmpc configuration for local files"
    )]
    local: bool,
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(
        short,
        long,
        global = true,
        help = "Path to the RON configuration file"
    )]
    config: Option<PathBuf>,
    #[arg(long, global = true, help = "Path to the rspc-ui executable")]
    frontend: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(last = true, help = "Arguments forwarded to rspc-ui")]
    frontend_args: Vec<OsString>,
}
#[derive(Subcommand)]
enum Commands {
    /// Run the Spotify engine in the foreground (also used by systemd).
    Daemon,
    /// Print daemon playback status without starting it.
    Status,
    /// Report direct audio output counters and backend errors.
    Doctor,
    /// Start Soloist and wait for Spotify Connect pairing.
    Pair,
    /// Stop the background engine and its playback.
    Stop,
    /// Sign in once for the Spotify library, profiles and audio playback.
    Login {
        /// Authorize only the catalog, without setting up the audio player.
        #[arg(long)]
        catalog: bool,
        /// Restart the player and renew its authorization; stops playback.
        #[arg(long, conflicts_with = "catalog")]
        repair: bool,
    },
}

struct Paths {
    socket: PathBuf,
    config: PathBuf,
    queue: PathBuf,
    log: PathBuf,
}
fn install_if_absent(path: &Path, content: &str) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(content.as_bytes())?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}
impl Paths {
    fn new(args: &Args) -> Result<Self> {
        let config_dir = xdg("XDG_CONFIG_HOME", ".config").join("rspc");
        let state_dir = xdg("XDG_STATE_HOME", ".local/state").join("rspc");
        let cache_dir = xdg("XDG_CACHE_HOME", ".cache").join("rspc");
        for dir in [&config_dir, &state_dir, &cache_dir] {
            ensure_private_dir(dir)?;
        }
        ensure_private_dir(&config_dir.join("themes"))?;
        install_if_absent(
            &config_dir.join("themes/default.ron"),
            include_str!("../config/themes/default.ron"),
        )?;
        install_if_absent(
            &config_dir.join("config.ron"),
            include_str!("../config/config.ron"),
        )?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| state_dir.join("run"));
        let runtime = runtime.join(if args.demo { "rspc-demo" } else { "rspc" });
        ensure_private_dir(&runtime)?;
        Ok(Self {
            socket: args
                .socket
                .clone()
                .unwrap_or_else(|| runtime.join("mpd.sock")),
            config: args
                .config
                .clone()
                .unwrap_or_else(|| config_dir.join("config.ron")),
            queue: state_dir.join(if args.demo {
                "demo-queue.json"
            } else {
                "queue.json"
            }),
            log: state_dir.join(if args.demo { "demo.log" } else { "daemon.log" }),
        })
    }
}
async fn request(socket: &Path, command: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(45), request_reply(socket, command))
        .await
        .context("rspc daemon response timed out")?
}
async fn request_reply(socket: &Path, command: &str) -> Result<String> {
    const MAX_REPLY: usize = 1024 * 1024;
    let stream = UnixStream::connect(socket)
        .await
        .context("rspc daemon is not running")?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut greeting = String::new();
    (&mut read).take(128).read_line(&mut greeting).await?;
    anyhow::ensure!(
        greeting.starts_with("OK MPD ") && greeting.ends_with('\n'),
        "invalid rspc daemon handshake"
    );
    write.write_all(format!("{command}\n").as_bytes()).await?;
    let mut output = String::new();
    loop {
        let mut line = String::new();
        anyhow::ensure!(
            (&mut read)
                .take((MAX_REPLY - output.len() + 1) as u64)
                .read_line(&mut line)
                .await?
                > 0,
            "daemon disconnected"
        );
        anyhow::ensure!(
            output.len() + line.len() <= MAX_REPLY && line.ends_with('\n'),
            "rspc daemon response exceeded its limit"
        );
        if line == "OK\n" {
            break;
        }
        if line.starts_with("ACK ") {
            bail!("{}", line.trim());
        }
        output.push_str(&line);
    }
    Ok(output)
}
async fn daemon_identity(socket: &Path) -> Option<String> {
    match tokio::time::timeout(Duration::from_secs(1), request(socket, "rspc_identity")).await {
        Ok(Ok(reply)) if reply.contains("name: rspc\n") => Some(reply),
        _ => None,
    }
}

async fn daemon_ready(socket: &Path, demo: bool) -> Result<bool> {
    let Some(identity) = daemon_identity(socket).await else {
        return Ok(false);
    };
    anyhow::ensure!(
        identity.contains(&format!("demo: {}\n", u8::from(demo))),
        "socket belongs to a different rspc mode; choose another socket"
    );
    Ok(true)
}
async fn start_daemon(args: &Args, paths: &Paths) -> Result<()> {
    if daemon_ready(&paths.socket, args.demo).await? {
        return Ok(());
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)?;
    let mut child = Command::new(std::env::current_exe()?);
    if let Some(config) = &args.config {
        child.env("RSPC_MPD_CONFIG", config);
    }
    child.args([
        OsString::from("--socket"),
        paths.socket.as_os_str().to_owned(),
    ]);
    child.arg("--config").arg(&paths.config);
    if args.demo {
        child.arg("--demo");
    }
    child
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // setsid is async-signal-safe; it separates engine lifetime from the terminal.
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn()?;
    for _ in 0..100 {
        if daemon_ready(&paths.socket, args.demo).await? {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("daemon exited with {status}; see {}", paths.log.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("daemon did not become ready; see {}", paths.log.display())
}
fn frontend(args: &Args) -> Result<PathBuf> {
    let path = match &args.frontend {
        Some(path) => path.clone(),
        None => std::env::current_exe()?.with_file_name("rspc-ui"),
    };
    anyhow::ensure!(
        path.is_file(),
        "Frontend missing: {}. Build with `cargo build -p rspc -p rspc-ui`.",
        path.display()
    );
    Ok(path)
}

async fn local(args: &Args) -> Result<()> {
    anyhow::ensure!(
        args.command.is_none(),
        "--local forwards frontend commands after --"
    );
    let config = args
        .config
        .clone()
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("rmpc/config.ron"));
    let mut command = tokio::process::Command::new(frontend(args)?);
    if config.is_file() {
        command.arg("--config").arg(config);
    }
    if let Some(address) = &args.socket {
        command.arg("--address").arg(address);
    }
    let status = command.args(&args.frontend_args).status().await?;
    anyhow::ensure!(status.success(), "local MPD interface exited with {status}");
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.local {
        return local(&args).await;
    }
    let paths = Paths::new(&args)?;
    match args.command {
        Some(Commands::Daemon) => {
            rspc::run_daemon(rspc::DaemonOptions {
                config: Some(paths.config),
                socket: paths.socket,
                queue: paths.queue,
                demo: args.demo,
            })
            .await
        }
        Some(Commands::Pair) => pair(&args, &paths).await,
        Some(Commands::Login {
            catalog: false,
            repair: false,
        }) => login(&args, &paths).await,
        Some(Commands::Login {
            catalog: false,
            repair: true,
        }) => {
            anyhow::ensure!(!args.demo, "Spotify login is unavailable in demo mode");
            if daemon_identity(&paths.socket).await.is_some() {
                stop(&paths).await?;
            }
            login(&args, &paths).await
        }
        Some(Commands::Doctor) => {
            print!("{}", request(&paths.socket, "rspc_diagnostics").await?);
            Ok(())
        }
        Some(Commands::Status) => {
            print!("{}", request(&paths.socket, "status").await?);
            Ok(())
        }
        Some(Commands::Stop) => stop(&paths).await,
        Some(Commands::Login { catalog: true, .. }) => {
            anyhow::ensure!(
                daemon_identity(&paths.socket).await.is_none(),
                "Stop the daemon with `rspc stop` before changing the Spotify login."
            );
            rspc::catalog_login().await
        }
        None => {
            let frontend = frontend(&args)?;
            start_daemon(&args, &paths).await?;
            let status = tokio::process::Command::new(frontend)
                .arg("--config")
                .arg(&paths.config)
                .arg("--address")
                .arg(&paths.socket)
                .args(&args.frontend_args)
                .status()
                .await?;
            if !status.success() {
                bail!("rspc interface exited with {status}");
            }
            Ok(())
        }
    }
}

async fn stop(paths: &Paths) -> Result<()> {
    request(&paths.socket, "rspc_shutdown").await?;
    for _ in 0..100 {
        if !paths.socket.exists() {
            println!("rspc daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("rspc daemon is still stopping; see {}", paths.log.display())
}

async fn pair(args: &Args, paths: &Paths) -> Result<()> {
    anyhow::ensure!(!args.demo, "Spotify login is unavailable in demo mode");
    start_daemon(args, paths).await?;
    let _ = request(&paths.socket, "rspc_pair").await?;
    println!("Select rspc in Spotify's device menu if it is not paired yet.");
    for _ in 0..300 {
        if request(&paths.socket, "rspc_diagnostics")
            .await?
            .contains("logged_in: 1\n")
        {
            let _ = request(&paths.socket, "rspc_check_spotify").await?;
            println!("Spotify pairing verified for playback and library access.");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("Soloist is still waiting for pairing; select rspc in Spotify and run rspc doctor.")
}

async fn login(args: &Args, paths: &Paths) -> Result<()> {
    anyhow::ensure!(!args.demo, "Spotify login is unavailable in demo mode");
    if daemon_identity(&paths.socket).await.is_some() {
        anyhow::ensure!(
            request(&paths.socket, "commands")
                .await?
                .contains("command: rspc_login\n"),
            "The running daemon needs an update: run rspc stop, then rspc login."
        );
    }
    rspc::ensure_catalog_login().await?;
    start_daemon(args, paths).await?;
    let _ = request(&paths.socket, "rspc_login").await?;
    println!("Spotify login verified for catalog access and Soloist playback.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn launcher_rejects_an_oversized_daemon_greeting() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(&vec![b'x'; 512]).await;
        });
        assert!(request(&socket, "status").await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn launcher_bounds_unterminated_daemon_responses() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"OK MPD 0.24.0\n").await.unwrap();
            let mut read = BufReader::new(stream);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let _ = read.get_mut().write_all(&vec![b'x'; 1024 * 1024 + 1]).await;
        });
        assert!(request(&socket, "status").await.is_err());
        server.await.unwrap();
    }
    #[test]
    fn pairing_repair_and_catalog_login_cannot_run_together() {
        assert!(Args::try_parse_from(["rspc", "login", "--repair", "--catalog"]).is_err());
        assert!(matches!(
            Args::try_parse_from(["rspc", "login"]).unwrap().command,
            Some(Commands::Login {
                catalog: false,
                repair: false
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["rspc", "login", "--repair"])
                .unwrap()
                .command,
            Some(Commands::Login {
                catalog: false,
                repair: true
            })
        ));
    }
}
