use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{Duration, Instant},
};

pub fn measure(socket: Option<PathBuf>, artist: Option<String>, samples: usize) -> Result<()> {
    let socket = socket.unwrap_or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| rspc::paths::state().join("run"))
            .join("rspc/mpd.sock")
    });
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    ensure!(line.starts_with("OK MPD "), "Invalid protocol greeting");
    let mut commands = vec![
        ("status".to_owned(), "status"),
        ("list Artist".into(), "list Artist"),
        ("list Album".into(), "list Album"),
        ("listplaylists".into(), "listplaylists"),
    ];
    if let Some(artist) = artist {
        commands.push((
            format!("rspc_artist_albums {} 0", serde_json::to_string(&artist)?),
            "artist release index",
        ));
    }
    for (command, label) in commands {
        let mut request = || -> Result<(f64, usize)> {
            let start = Instant::now();
            writeln!(reader.get_mut(), "{command}")?;
            let mut bytes = 0;
            loop {
                line.clear();
                ensure!(reader.read_line(&mut line)? > 0, "Daemon disconnected");
                if line == "OK\n" {
                    break;
                }
                ensure!(!line.starts_with("ACK "), "Protocol request failed");
                bytes += line.len();
            }
            Ok((start.elapsed().as_secs_f64() * 1000.0, bytes))
        };
        let (cold, _) = request()?;
        let measurements = (0..samples)
            .map(|_| request())
            .collect::<Result<Vec<_>>>()?;
        let mut times: Vec<_> = measurements.iter().map(|(time, _)| *time).collect();
        times.sort_by(f64::total_cmp);
        println!(
            "{}",
            json!({"command":label,"samples":samples,"first_ms":cold,"median_ms":times[times.len()/2],"p95_ms":times[(times.len()*95/100).min(times.len()-1)],"response_bytes":measurements.last().context("No measurements")?.1})
        );
    }
    Ok(())
}
