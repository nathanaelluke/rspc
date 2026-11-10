mod support;
use anyhow::{Context, Result, ensure};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use support::Harness;

fn catalog_harness() -> Result<Harness> {
    let credentials = rspc::paths::cache().join("streaming_cache/credentials.json");
    ensure!(credentials.is_file(), "A saved catalog login is required");
    let mut h = Harness::isolated(false)?;
    h.env.insert(
        "RSPC_SOLOIST_PATH".into(),
        h.path("not-installed").into_os_string(),
    );
    h.env.insert(
        "RSPC_MPD_ADDRESS".into(),
        h.path("absent-mpd.sock").into_os_string(),
    );
    let cache = h.path("cache/rspc/streaming_cache");
    fs::create_dir_all(&cache)?;
    fs::copy(credentials, cache.join("credentials.json"))?;
    fs::set_permissions(
        cache.join("credentials.json"),
        fs::Permissions::from_mode(0o600),
    )?;
    Ok(h)
}

#[test]
#[ignore = "requires a real catalog login, Soloist and a private API key"]
fn one_login_bootstraps_and_restores_a_fresh_audio_receiver() -> Result<()> {
    let player = std::env::var_os("RSPC_SOLOIST_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join("soloist"))
                    .find(|file| file.is_file())
            })
        })
        .unwrap_or_else(|| rspc::paths::state().join("soloist-package/bin/soloist"));
    ensure!(player.is_file(), "Soloist must be installed");
    let key = std::env::var("RSPC_SOLOIST_API_KEY")
        .ok()
        .or_else(|| {
            fs::read_to_string(
                rspc::paths::xdg("XDG_CONFIG_HOME", ".config").join("rspc/soloist-api-key"),
            )
            .ok()
        })
        .context("Soloist key is required")?;
    let mut h = catalog_harness()?;
    h.env
        .insert("RSPC_SOLOIST_PATH".into(), player.into_os_string());
    h.env
        .insert("RSPC_SOLOIST_API_KEY".into(), key.trim().into());
    let audio = h.path("state/rspc/soloist/cache/dbrts");
    assert!(!audio.exists());
    h.action("login")?;
    let report = h.doctor()?;
    assert!(report.contains("logged_in: 1\n"));
    assert!(report.contains("catalog_auth: catalog\n"));
    assert!(audio.is_file());
    h.action("stop")?;
    h.action("login")?;
    assert!(h.doctor()?.contains("logged_in: 1\n"));
    h.action("stop")?;
    Ok(())
}

#[test]
#[ignore = "requires a saved login and contacts Spotify; prints timings only"]
fn catalog_latency_from_an_empty_cache() -> Result<()> {
    let mut h = catalog_harness()?;
    h.start()?;
    xtask::benchmark::measure(
        Some(h.socket.clone()),
        std::env::var("RSPC_BENCHMARK_ARTIST").ok(),
        10,
    )
}

#[test]
#[ignore = "creates, edits and removes a private playlist named rspc verification"]
fn private_playlist_round_trip() -> Result<()> {
    use support::quote;
    let mut h = catalog_harness()?;
    h.start()?;
    let name = "rspc verification";
    ensure!(
        !h.raw("listplaylists")?
            .lines()
            .any(|line| line == format!("playlist: {name}")),
        "Verification playlist already exists; refusing to modify it"
    );
    let mut client = support::Protocol::connect(&h.socket)?;
    client
        .reader
        .get_mut()
        .set_read_timeout(Some(std::time::Duration::from_secs(90)))?;
    let tracks = client.command("lsinfo \"Liked Songs\"")?;
    let uris: Vec<_> = tracks
        .lines()
        .filter_map(|line| line.strip_prefix("file: spotify:track:"))
        .take(2)
        .map(|id| format!("spotify:track:{id}"))
        .collect();
    ensure!(uris.len() == 2, "Two saved tracks are required");
    h.raw(&format!("save {}", quote(name)))?;
    struct Cleanup<'a>(&'a Harness, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Err(error) = self.0.raw(&format!("rm {}", quote(self.1))) {
                eprintln!("Remove the private verification playlist manually: {error}");
            }
        }
    }
    let cleanup = Cleanup(&h, name);
    let read = || -> Result<Vec<String>> {
        Ok(h.raw(&format!("listplaylist {}", quote(name)))?
            .lines()
            .filter_map(|line| line.strip_prefix("file: ").map(str::to_owned))
            .collect())
    };
    for uri in &uris {
        h.raw(&format!("playlistadd {} {}", quote(name), quote(uri)))?;
    }
    ensure!(read()? == uris, "Playlist additions did not persist");
    h.raw(&format!("playlistmove {} 0 1", quote(name)))?;
    ensure!(
        read()? == [uris[1].clone(), uris[0].clone()],
        "Playlist move did not persist"
    );
    h.raw(&format!("playlistdelete {} 0", quote(name)))?;
    ensure!(
        read()? == [uris[0].clone()],
        "Playlist deletion did not persist"
    );
    h.raw(&format!("playlistclear {}", quote(name)))?;
    ensure!(read()?.is_empty(), "Playlist clear did not persist");
    drop(cleanup);
    ensure!(
        !h.raw("listplaylists")?
            .lines()
            .any(|line| line == format!("playlist: {name}")),
        "Verification playlist was not removed"
    );
    Ok(())
}
