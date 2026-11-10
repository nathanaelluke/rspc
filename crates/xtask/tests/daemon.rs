mod support;
use anyhow::Result;
use serde_json::{Value, json};
use std::{fs, io::BufRead, os::unix::fs::PermissionsExt, thread, time::Duration};
use support::{Harness, Protocol, contains, quote, state_is, wait};

#[test]
fn quit_reopen_and_terminal_hangup_keep_playing() -> Result<()> {
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    let before = h.status()?;
    h.tui(&[], false)?;
    thread::sleep(Duration::from_millis(400));
    let after = h.status()?;
    assert!(state_is(&after, "play"));
    assert_eq!(before["songid"], after["songid"]);
    assert!(after["elapsed"].as_f64() > before["elapsed"].as_f64());
    h.tui(&[], false)?;
    h.tui(&[], true)?;
    assert!(state_is(&h.status()?, "play"));
    assert_eq!(h.socket.metadata()?.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[test]
fn launcher_starts_daemon_automatically() -> Result<()> {
    let h = Harness::isolated(true)?;
    assert!(!h.socket.exists());
    assert_eq!(h.queue()?.as_array().unwrap().len(), 3);
    assert!(h.socket.exists());
    Ok(())
}

#[test]
fn song_info_opens_full_catalog_without_changing_playback() -> Result<()> {
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    let id = h.status()?["songid"].clone();
    let output = h.tui(
        &[b"oI", b"jjjj", b"\r", b"/", b"Morning", b"\r", b"h"],
        false,
    )?;
    assert!(contains(&output, b"Spotify catalog"));
    assert_eq!(h.status()?["songid"], id);
    Ok(())
}

#[test]
fn curator_browser_and_public_playlists() -> Result<()> {
    let h = Harness::demo()?;
    for (path, expected) in [
        ("", "directory: People"),
        ("People", "Example Curator"),
        ("People/profile/demo-user/Example Curator", "Demo Mix"),
        ("People/playlist/demo/Demo Mix", "Morning Light"),
    ] {
        assert!(
            h.raw(&format!("lsinfo {}", quote(path)))?
                .contains(expected)
        );
    }
    let output = h.tui(
        &[b"2", b"/", b"People", b"\r", b"l", b"l", b"l", b"1"],
        false,
    )?;
    assert!(contains(&output, b"Example Curator") && contains(&output, b"Demo Mix"));
    assert!(state_is(&h.status()?, "stop"));
    Ok(())
}

#[test]
fn queue_controls_browsers_and_command_lists() -> Result<()> {
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    let id = h.status()?["songid"].clone();
    h.raw("move 0 2")?;
    assert_eq!(h.status()?["songid"], id);
    h.cli(&["volume", "42"])?;
    h.cli(&["seek", "12"])?;
    h.cli(&["pause"])?;
    let status = h.status()?;
    assert_eq!(status["volume"], 42);
    assert!(state_is(&status, "pause"));
    assert!(status["elapsed"].as_f64().unwrap() >= 12.0);
    h.cli(&["play"])?;
    assert!(state_is(&h.status()?, "play"));
    assert!(h.status()?["elapsed"].as_f64().unwrap() >= 12.0);
    assert!(
        h.raw("find \"(Artist == 'The Example Ensemble')\"")?
            .contains("Morning Light")
    );
    assert!(h.raw("listplaylists")?.contains("Demo Mix"));
    assert!(
        h.raw("listplaylistinfo \"Demo Mix\" 0:1")?
            .contains("Morning Light")
    );
    assert!(h.raw("lsinfo")?.contains("directory: Liked Songs"));
    h.raw("command_list_ok_begin\nrepeat 1\nrandom 1\ncommand_list_end")?;
    let status = h.status()?;
    assert_eq!(status["repeat"], true);
    assert_eq!(status["random"], true);
    Ok(())
}

#[test]
fn paged_artist_index_and_whole_artist_queue_actions() -> Result<()> {
    let h = Harness::demo()?;
    assert!(h.raw("commands")?.contains("command: rspc_artist_albums"));
    let reply = h.raw("rspc_artist_albums \"The Example Ensemble\" 0")?;
    let page: Value = serde_json::from_str(reply.trim().strip_prefix("album_page: ").unwrap())?;
    assert!(page["next"].is_null());
    assert_eq!(page["albums"].as_array().unwrap().len(), 1);
    let uri = page["albums"][0]["uri"].as_str().unwrap();
    assert_eq!(
        h.raw(&format!("lsinfo {}", quote(uri)))?
            .matches("file: ")
            .count(),
        3
    );
    h.raw("clear")?;
    h.raw(&format!("add {}", quote(uri)))?;
    assert_eq!(h.raw("playlistinfo")?.matches("file: ").count(), 3);
    let output = h.tui(&[b"3", b"l", b"l", b"oi", b"\x1b"], false)?;
    assert!(!contains(&output, b"timed out"));
    assert!(contains(&output, b"Morning Light"));
    assert_eq!(h.raw("playlistinfo")?.matches("file: ").count(), 3);
    Ok(())
}

#[test]
fn selection_info_on_every_library_page() -> Result<()> {
    use support::TuiStep::{Key, WaitFor};

    let h = Harness::demo()?;
    let pages: &[(&[u8], &[u8])] = &[
        (b"2", b"Liked Songs"),
        (b"3", b"The Example Ensemble"),
        (b"4", b"The Example Ensemble"),
        (b"5", b"Example Sessions"),
        (b"6", b"Demo Mix"),
    ];
    for &(tab, ready) in pages {
        let mut steps = vec![Key(tab), WaitFor(ready)];
        if tab == b"2" {
            steps.extend([Key(b"/"), Key(b"Liked Songs"), Key(b"\r")]);
        }
        steps.extend([Key(b"oi"), WaitFor(b"Selection info"), Key(b"\x1b")]);
        let output = h.tui_steps(&steps, false)?;
        assert!(contains(&output, b"Selection info"));
        assert!(!contains(&output, b"timed out"));
    }
    for (kind, value, field) in [
        ("path", "Playlists", "Playlists"),
        ("path", "Albums", "Albums"),
        ("path", "People", "People"),
        (
            "path",
            "People/profile/demo-user/Example Curator",
            "Public playlists",
        ),
        ("artist", "The Example Ensemble", "Releases"),
    ] {
        let reply = h.raw(&format!(
            "rspc_selection_info {} {}",
            quote(kind),
            quote(value)
        ))?;
        let info: Vec<(String, String)> =
            serde_json::from_str(reply.trim().strip_prefix("selection_info: ").unwrap())?;
        assert!(info.iter().any(|(key, value)| key == field && value == "1"));
    }
    for path in ["Liked Songs", "Albums", "Playlists", "People"] {
        assert_eq!(
            h.raw(&format!("listallinfo {}", quote(path)))?
                .matches("file: ")
                .count(),
            3
        );
    }
    Ok(())
}

#[test]
fn sigterm_removes_socket_and_saves_private_queue() -> Result<()> {
    let mut h = Harness::demo()?;
    h.cli(&["queue"])?;
    let pid = h.daemon.as_ref().unwrap().0.id();
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    wait(|| Ok(h.daemon.as_mut().unwrap().0.try_wait()?.is_some()))?;
    assert!(!h.socket.exists());
    let path = h.path("state/rspc/demo-queue.json");
    let queue: Value = serde_json::from_slice(&fs::read(&path)?)?;
    assert_eq!(queue["entries"].as_array().unwrap().len(), 3);
    assert_eq!(path.metadata()?.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[test]
fn idle_coalesces_changes_and_supports_cancellation() -> Result<()> {
    let h = Harness::demo()?;
    let mut client = Protocol::connect(&h.socket)?;
    h.raw("move 0 2")?;
    h.cli(&["play", "0"])?;
    assert_eq!(client.command("idle playlist")?, "changed: playlist\n");
    client.send("idle database")?;
    h.cli(&["volume", "51"])?;
    client
        .reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(150)))?;
    let mut line = String::new();
    assert!(client.reader.read_line(&mut line).is_err());
    client
        .reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(3)))?;
    assert_eq!(client.command("noidle")?, "");
    Ok(())
}

#[test]
fn repeated_seeks_and_updates_keep_tui_connected() -> Result<()> {
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    h.cli(&["pause"])?;
    let id = h.status()?["songid"].clone();
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let socket = h.socket.clone();
    let worker = thread::spawn(move || -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            Protocol::connect(&socket)?.command("seekcur +0.001")?;
            thread::sleep(Duration::from_millis(4));
        }
        Ok(())
    });
    let keys: Vec<&[u8]> = (0..16)
        .flat_map(|_| [b"f".as_slice(), b"b".as_slice()])
        .collect();
    let output = h.tui(&keys, false);
    done.store(true, Ordering::Relaxed);
    worker.join().unwrap()?;
    let output = String::from_utf8_lossy(&output?).to_lowercase();
    for error in [
        "lost connection",
        "reconnected to mpd",
        "response from mpd timed out",
    ] {
        assert!(!output.contains(error));
    }
    assert_eq!(h.status()?["songid"], id);
    assert!(state_is(&h.status()?, "pause"));
    Ok(())
}

#[test]
fn local_mode_uses_existing_rmpc_config_without_creating_rspc_config() -> Result<()> {
    let h = Harness::demo()?;
    fs::create_dir_all(h.path("config/rmpc"))?;
    fs::write(
        h.path("config/rmpc/config.ron"),
        format!("(address: {},)\n", quote(h.socket.to_str().unwrap())),
    )?;
    fs::remove_dir_all(h.path("config/rspc"))?;
    let result = support::output(
        std::process::Command::new(support::executable("rspc"))
            .env_clear()
            .envs(&h.env)
            .args(["--local", "--frontend"])
            .arg(support::executable("rspc-ui"))
            .args(["--", "status"]),
        Duration::from_secs(5),
    )?;
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout)?["playlistlength"],
        3
    );
    assert!(!h.path("config/rspc").exists());
    Ok(())
}

#[test]
fn saved_playlist_edits_preserve_queue() -> Result<()> {
    let h = Harness::demo()?;
    let queue = h.queue()?;
    h.cli(&["save", "Editable Copy"])?;
    for command in [
        "playlistadd \"Editable Copy\" \"spotify:track:demo1\" 1",
        "playlistdelete \"Editable Copy\" 0",
        "playlistmove \"Editable Copy\" 0 2",
    ] {
        h.raw(command)?;
    }
    let songs = h.raw("listplaylistinfo \"Editable Copy\"")?;
    assert!(songs.find("Title: Second Window") < songs.find("Title: Morning Light"));
    h.raw("playlistmove \"Editable Copy\" 0:2 1")?;
    let songs = h.raw("listplaylistinfo \"Editable Copy\"")?;
    assert!(songs.find("Title: Morning Light") < songs.find("Title: Second Window"));
    h.raw("rename \"Editable Copy\" \"Renamed Copy\"")?;
    h.raw("playlistclear \"Renamed Copy\"")?;
    assert!(h.raw("listplaylistinfo \"Renamed Copy\"")?.is_empty());
    h.raw("rm \"Renamed Copy\"")?;
    assert!(!h.raw("listplaylists")?.contains("Renamed Copy"));
    assert_eq!(h.queue()?, queue);
    Ok(())
}

#[test]
fn passworded_config_is_accepted() -> Result<()> {
    let mut h = Harness::demo()?;
    let config = h.path("custom.ron");
    fs::write(
        &config,
        "(address: \"127.0.0.1:6600\", password: Some(\"fixture\"),)\n",
    )?;
    h.extra.extend(["--config".into(), config.into_os_string()]);
    assert_eq!(h.status()?["playlistlength"], 3);
    Ok(())
}

#[test]
fn exclusions_reload_without_restarting_playback() -> Result<()> {
    let mut h = Harness::isolated(true)?;
    let config = h.path("exclusions.ron");
    fs::write(
        &config,
        "(spotify: (exclude_folders: [], exclude_playlists: [\"Demo Mix\"]),)\n",
    )?;
    h.extra
        .extend(["--config".into(), config.clone().into_os_string()]);
    h.start()?;
    h.cli(&["play", "0"])?;
    assert!(!h.raw("listplaylists")?.contains("Demo Mix"));
    let id = h.status()?["songid"].clone();
    let mut client = Protocol::connect(&h.socket)?;
    client.send("idle stored_playlist")?;
    fs::write(
        config,
        "(spotify: (exclude_folders: [], exclude_playlists: []),)\n",
    )?;
    assert_eq!(client.response()?, "changed: stored_playlist\n");
    wait(|| Ok(h.raw("listplaylists")?.contains("Demo Mix")))?;
    assert_eq!(h.status()?["songid"], id);
    assert!(state_is(&h.status()?, "play"));
    Ok(())
}

#[test]
fn consume_once_and_single_once_advance_at_natural_end() -> Result<()> {
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    h.cli(&["consume", "oneshot"])?;
    h.cli(&["seek", "180"])?;
    wait(|| Ok(h.status()?["playlistlength"] == 2))?;
    let status = h.status()?;
    assert!(state_is(&status, "play"));
    assert!([json!("Off"), json!(false), json!(0)].contains(&status["consume"]));
    h.cli(&["single", "oneshot"])?;
    h.cli(&["seek", "210"])?;
    wait(|| Ok(state_is(&h.status()?, "stop")))?;
    let value = h.status()?["single"].clone();
    assert!([json!("Off"), json!(false), json!(0)].contains(&value));
    Ok(())
}
