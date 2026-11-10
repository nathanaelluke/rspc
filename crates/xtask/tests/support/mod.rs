#![allow(dead_code)]
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn root() -> PathBuf {
    xtask::root()
}
pub fn executable(name: &str) -> PathBuf {
    let variable = if name == "rspc" {
        "RSPC_TEST_BINARY"
    } else {
        "RSPC_UI_TEST_BINARY"
    };
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/release").join(name))
}

pub struct Process(pub Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
pub fn output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let mut child = Process(
        command
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .spawn()?,
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(Instant::now() < deadline, "Fixture command timed out");
        thread::sleep(Duration::from_millis(10));
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    use std::io::Seek;
    let mut stdout = stdout;
    let mut stderr = stderr;
    stdout.rewind()?;
    stderr.rewind()?;
    stdout.take(2 * 1024 * 1024).read_to_end(&mut out)?;
    stderr.take(2 * 1024 * 1024).read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

pub struct Protocol {
    pub reader: BufReader<UnixStream>,
}
impl Protocol {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream);
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        ensure!(greeting.starts_with("OK MPD "), "Invalid greeting");
        Ok(Self { reader })
    }
    pub fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.reader.get_mut(), "{command}")?;
        Ok(())
    }
    pub fn response(&mut self) -> Result<String> {
        let mut reply = String::new();
        loop {
            let mut line = String::new();
            ensure!(
                self.reader.read_line(&mut line)? > 0,
                "Daemon disconnected before replying"
            );
            if line == "OK\n" {
                return Ok(reply);
            }
            ensure!(!line.starts_with("ACK "), "Protocol error: {}", line.trim());
            reply.push_str(&line);
            ensure!(reply.len() < 2 * 1024 * 1024, "Fixture reply too large");
        }
    }
    pub fn command(&mut self, command: &str) -> Result<String> {
        self.send(command)?;
        self.response()
    }
}

pub struct Harness {
    pub temp: tempfile::TempDir,
    pub env: HashMap<OsString, OsString>,
    pub socket: PathBuf,
    pub demo: bool,
    pub extra: Vec<OsString>,
    pub daemon: Option<Process>,
    pub mpd: Option<Process>,
}
impl Harness {
    pub fn isolated(demo: bool) -> Result<Self> {
        ensure!(
            executable("rspc").is_file() && executable("rspc-ui").is_file(),
            "Build both player executables first (make build), or set RSPC_TEST_BINARY and RSPC_UI_TEST_BINARY"
        );
        let temp = tempfile::tempdir()?;
        let mut env: HashMap<OsString, OsString> = std::env::vars_os().collect();
        for key in [
            "TMUX",
            "RSPC_MPD_OUTPUT",
            "RSPC_MPD_PASSWORD",
            "RSPC_MPD_ADDRESS",
            "RSPC_SOLOIST_PATH",
            "RSPC_SOLOIST_API_KEY",
        ] {
            env.remove(std::ffi::OsStr::new(key));
        }
        env.insert("TERM".into(), "xterm-256color".into());
        for (key, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let path = temp.path().join(dir);
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            env.insert(key.into(), path.into_os_string());
        }
        let socket = temp.path().join("engine.sock");
        Ok(Self {
            temp,
            env,
            socket,
            demo,
            extra: Vec::new(),
            daemon: None,
            mpd: None,
        })
    }
    pub fn demo() -> Result<Self> {
        let mut harness = Self::isolated(true)?;
        harness.start()?;
        Ok(harness)
    }
    pub fn path(&self, path: &str) -> PathBuf {
        self.temp.path().join(path)
    }
    pub fn command(&self) -> Command {
        let mut command = Command::new(executable("rspc"));
        command
            .env_clear()
            .envs(&self.env)
            .arg("--socket")
            .arg(&self.socket)
            .arg("--frontend")
            .arg(executable("rspc-ui"));
        if self.demo {
            command.arg("--demo");
        }
        command.args(&self.extra);
        command
    }
    pub fn start(&mut self) -> Result<()> {
        self.daemon = Some(Process(
            self.command()
                .arg("daemon")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        ));
        wait(|| Ok(self.socket.exists()))?;
        Ok(())
    }
    pub fn cli(&self, args: &[&str]) -> Result<String> {
        let result = output(self.command().arg("--").args(args), Duration::from_secs(20))?;
        ensure!(
            result.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(String::from_utf8(result.stdout)?)
    }
    pub fn action(&self, action: &str) -> Result<String> {
        let result = output(self.command().arg(action), Duration::from_secs(90))?;
        ensure!(result.status.success(), "rspc {action} failed");
        Ok(String::from_utf8(result.stdout)?)
    }
    pub fn raw(&self, command: &str) -> Result<String> {
        Protocol::connect(&self.socket)?.command(command)
    }
    pub fn status(&self) -> Result<Value> {
        let mut value: Value = serde_json::from_str(&self.cli(&["status"])?)?;
        if value["elapsed"].is_object() {
            value["elapsed"] = json!(
                value["elapsed"]["secs"].as_f64().unwrap_or(0.0)
                    + value["elapsed"]["nanos"].as_f64().unwrap_or(0.0) / 1e9
            );
        }
        Ok(value)
    }
    pub fn queue(&self) -> Result<Value> {
        Ok(serde_json::from_str(&self.cli(&["queue"])?)?)
    }
    pub fn doctor(&self) -> Result<String> {
        self.action("doctor")
    }
    pub fn spotify(&self) -> Value {
        fs::read(self.path("state/rspc/soloist/fixture-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn remote(&self, value: Value) -> Result<()> {
        let path = self.path("state/rspc/soloist/fixture-playback.json");
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(&value)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn mpd(&self, command: &str, partition: &str) -> Result<String> {
        let mut protocol = Protocol::connect(&self.path("mpd.sock"))?;
        protocol.command(&format!("partition {}", quote(partition)))?;
        protocol.command(command)
    }
    pub fn stop(&mut self) -> Result<()> {
        self.action("stop")?;
        wait(|| Ok(!self.socket.exists()))?;
        self.daemon.take();
        Ok(())
    }
    pub fn tui(&self, keys: &[&[u8]], hangup: bool) -> Result<Vec<u8>> {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 35,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // openpty returns two owned descriptors; neither pointer escapes the call.
        ensure!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            } == 0,
            "Cannot open test terminal"
        );
        let mut master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let mut command = self.command();
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // Only async-signal-safe libc operations run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut process = Process(command.spawn()?);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(12);
        let mut bytes = Vec::new();
        let mut populated = None;
        let mut next = None;
        let mut index = 0;
        let mut quit = false;
        let mut signalled = false;
        while Instant::now() < deadline {
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 25) } > 0 {
                let mut buffer = [0; 65536];
                match master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let chunk = &buffer[..count];
                        bytes.extend_from_slice(chunk);
                        ensure!(
                            bytes.len() < 4 * 1024 * 1024,
                            "Terminal fixture output exceeds limit"
                        );
                        if contains(chunk, b"\x1b[>q") {
                            master.write_all(b"\x1bP>|XTerm(390)\x1b\\")?;
                        }
                        if contains(chunk, b"\x1b[?u") {
                            master.write_all(b"\x1b[?0u\x1b[?1;2c")?;
                        } else if contains(chunk, b"\x1b[c") || contains(chunk, b"\x1b[0c") {
                            master.write_all(b"\x1b[?1;2c")?;
                        }
                        if contains(chunk, b"\x1b[6n") {
                            master.write_all(b"\x1b[1;1R")?;
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if populated.is_none() && contains(&bytes, b"Morning Light") {
                populated = Some(start.elapsed());
                next = Some(Instant::now() + Duration::from_millis(150));
                if hangup {
                    unsafe {
                        libc::kill(-(process.0.id() as i32), libc::SIGHUP);
                    }
                    signalled = true;
                    break;
                }
            }
            if next.is_some_and(|at| Instant::now() >= at) {
                if let Some(key) = keys.get(index) {
                    master.write_all(key)?;
                    index += 1;
                    next = Some(Instant::now() + Duration::from_millis(150));
                } else if !quit {
                    master.write_all(b"q")?;
                    quit = true;
                    next = None;
                }
            }
            if process.0.try_wait()?.is_some() {
                break;
            }
        }
        let end = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = process.0.try_wait()? {
                break status;
            }
            ensure!(Instant::now() < end, "TUI did not close");
            thread::sleep(Duration::from_millis(10));
        };
        use std::os::unix::process::ExitStatusExt;
        ensure!(
            if signalled {
                status.signal() == Some(libc::SIGHUP)
            } else {
                status.success()
            },
            "TUI exited unsuccessfully: {}",
            String::from_utf8_lossy(&bytes)
        );
        ensure!(
            populated.is_some(),
            "TUI never produced its first populated frame"
        );
        ensure!(
            !contains(&bytes, b"unsupported command"),
            "TUI used an unsupported command"
        );
        println!(
            "TUI first populated frame: {} ms",
            populated.unwrap().as_millis()
        );
        Ok(bytes)
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = output(self.command().arg("stop"), Duration::from_secs(5));
        self.daemon.take();
        self.mpd.take();
    }
}
pub fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}
pub fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("string quoting")
}
pub fn wait(mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        if predicate()? {
            return Ok(());
        }
        ensure!(Instant::now() < end, "Timed out waiting for fixture state");
        thread::sleep(Duration::from_millis(20));
    }
}
pub fn state_is(value: &Value, state: &str) -> bool {
    value["state"]
        .as_str()
        .is_some_and(|v| v.eq_ignore_ascii_case(state))
}

pub fn wav(path: &Path, seconds: f64) -> Result<()> {
    let samples = (8000.0 * seconds) as u32;
    let size = samples * 2;
    let mut data = Vec::new();
    data.extend(b"RIFF");
    data.extend((36 + size).to_le_bytes());
    data.extend(b"WAVEfmt ");
    data.extend(16u32.to_le_bytes());
    data.extend(1u16.to_le_bytes());
    data.extend(1u16.to_le_bytes());
    data.extend(8000u32.to_le_bytes());
    data.extend(16000u32.to_le_bytes());
    data.extend(2u16.to_le_bytes());
    data.extend(16u16.to_le_bytes());
    data.extend(b"data");
    data.extend(size.to_le_bytes());
    data.resize(data.len() + size as usize, 0);
    fs::write(path, data)?;
    Ok(())
}

impl Harness {
    pub fn mixed() -> Result<Self> {
        let mut h = Self::isolated(false)?;
        h.env.insert(
            "RSPC_MPD_ADDRESS".into(),
            h.path("mpd.sock").into_os_string(),
        );
        h.env.insert(
            "RSPC_SOLOIST_PATH".into(),
            env!("CARGO_BIN_EXE_soloist-fixture").into(),
        );
        h.env.insert(
            "RSPC_SOLOIST_API_KEY".into(),
            "isolated-test-fixture".into(),
        );
        fs::create_dir(h.path("music"))?;
        fs::create_dir(h.path("playlists"))?;
        for (name, seconds) in [
            ("short.wav", 0.45),
            ("long.wav", 8.0),
            ("sentinel.wav", 1.0),
        ] {
            wav(&h.path(&format!("music/{name}")), seconds)?;
        }
        let config = format!(
            "music_directory {}\ndb_file {}\nplaylist_directory {}\nbind_to_address {}\naudio_output {{\n type \"null\"\n name \"Isolated fixture\"\n mixer_type \"software\"\n}}\n",
            quote(h.path("music").to_str().unwrap()),
            quote(h.path("db").to_str().unwrap()),
            quote(h.path("playlists").to_str().unwrap()),
            quote(h.path("mpd.sock").to_str().unwrap())
        );
        fs::write(h.path("mpd.conf"), config)?;
        let log = fs::File::create(h.path("mpd.log"))?;
        h.mpd = Some(Process(
            Command::new("mpd")
                .args(["--no-daemon", "--stdout"])
                .arg(h.path("mpd.conf"))
                .env_clear()
                .envs(&h.env)
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()
                .context("MPD executable is required for mixed-source tests")?,
        ));
        wait(|| Ok(h.path("mpd.sock").exists()))?;
        h.mpd("update", "default")?;
        wait(|| Ok(h.mpd("stats", "default")?.contains("songs: 3")))?;
        h.mpd("addid \"sentinel.wav\"", "default")?;
        let entries = [
            ("spotify:track:first", "Morning Light", 1200),
            ("mpd:short.wav", "short.wav", 450),
            ("spotify:track:last", "Last Spotify Song", 1200),
            ("mpd:long.wav", "long.wav", 8000),
        ];
        let entries:Vec<_>=entries.iter().enumerate().map(|(i,(uri,name,duration))|json!({"id":i+1,"track":{"uri":uri,"name":name,"artists":["Fixture Artist"],"album":"Mixed Queue","duration_ms":duration}})).collect();
        fs::create_dir_all(h.path("state/rspc"))?;
        fs::write(
            h.path("state/rspc/queue.json"),
            serde_json::to_vec(&json!({"entries":entries,"next_id":4,"version":1,"current":null}))?,
        )?;
        h.start()?;
        wait(|| Ok(h.doctor()?.contains("logged_in: 1\n")))?;
        Ok(h)
    }
}
