mod support;
use anyhow::Result;
use serde_json::json;
use std::{
    fs, thread,
    time::{Duration, Instant},
};
use support::{Harness, Protocol, quote, state_is, wait};

#[test]
fn selection_info_preserves_default_mpd_queue_outputs_and_state() -> Result<()> {
    let h = Harness::mixed()?;
    let queue = h.mpd("playlistinfo", "default")?;
    let outputs = h.mpd("outputs", "default")?;
    let state = h.mpd("status", "default")?;
    let reply = h.raw("rspc_selection_info \"path\" \"Local Files\"")?;
    let info: Vec<(String, String)> =
        serde_json::from_str(reply.trim().strip_prefix("selection_info: ").unwrap())?;
    assert!(info.contains(&("Songs".into(), "3".into())));
    assert_eq!(
        h.raw("listallinfo \"Local Files\"")?
            .matches("file: ")
            .count(),
        3
    );
    h.mpd("playlistadd \"Folder Info\" \"long.wav\"", "default")?;
    for (kind, path, field) in [
        ("path", "Local Playlists", "Playlists"),
        ("playlist", "Local Playlists/Folder Info", "Songs"),
    ] {
        let reply = h.raw(&format!(
            "rspc_selection_info {} {}",
            quote(kind),
            quote(path)
        ))?;
        let info: Vec<(String, String)> =
            serde_json::from_str(reply.trim().strip_prefix("selection_info: ").unwrap())?;
        assert!(info.contains(&(field.into(), "1".into())));
    }
    assert_eq!(h.mpd("playlistinfo", "default")?, queue);
    assert_eq!(h.mpd("outputs", "default")?, outputs);
    assert_eq!(h.mpd("status", "default")?, state);
    Ok(())
}

#[test]
fn natural_spotify_local_spotify_handoff_preserves_queue() -> Result<()> {
    let h = Harness::mixed()?;
    h.cli(&["play", "0"])?;
    wait(|| {
        let state = h.status()?;
        Ok(state["songid"] == 2 && state_is(&state, "play"))
    })?;
    assert!([json!("paused"), json!("idle")].contains(&h.spotify()["status"]));
    assert!(h.mpd("status", "rspc")?.contains("state: play"));
    wait(|| {
        let state = h.status()?;
        Ok(state["songid"] == 3 && state_is(&state, "play"))
    })?;
    assert!(h.mpd("status", "rspc")?.contains("state: stop"));
    assert_eq!(h.queue()?.as_array().unwrap().len(), 4);
    assert_eq!(
        h.mpd("playlistinfo", "default")?
            .matches("file: sentinel.wav")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn slow_handoff_does_not_block_new_connections() -> Result<()> {
    let h = Harness::mixed()?;
    fs::write(
        h.path("state/rspc/soloist/fixture-delays.json"),
        r#"{"play:spotify:track:last":1.5}"#,
    )?;
    h.cli(&["play", "0"])?;
    wait(|| Ok(h.path("state/rspc/soloist/fixture-pending.json").exists()))?;
    let start = Instant::now();
    h.raw("ping")?;
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_eq!(h.queue()?.as_array().unwrap().len(), 4);
    Ok(())
}

#[test]
fn deleting_paused_tracks_keeps_both_players_paused() -> Result<()> {
    let h = Harness::mixed()?;
    h.raw("playid 4")?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    h.raw("setvol 23")?;
    h.raw("pause 1")?;
    let uri = h.spotify()["uri"].clone();
    h.raw("moveid 4 0")?;
    h.raw("deleteid 4")?;
    wait(|| Ok(h.status()?["songid"] == 1))?;
    assert!(state_is(&h.status()?, "pause"));
    assert_eq!(h.spotify()["uri"], uri);
    assert!(h.mpd("status", "rspc")?.contains("state: stop"));
    h.raw("pause 0")?;
    wait(|| Ok(h.spotify()["volume"] == 23 && h.spotify()["uri"] == "spotify:track:first"))?;
    h.raw("pause 1")?;
    h.raw("deleteid 1")?;
    wait(|| Ok(h.status()?["songid"] == 2))?;
    assert!(state_is(&h.status()?, "pause"));
    assert!(h.mpd("status", "rspc")?.contains("state: stop"));
    h.raw("seekcur 0.1")?;
    h.raw("pause 0")?;
    wait(|| {
        let state = h.status()?;
        Ok(state["songid"] == 2
            && state_is(&state, "play")
            && state["elapsed"].as_f64().unwrap_or(0.0) >= 0.1)
    })?;
    assert!(h.status()?["elapsed"].as_f64().unwrap() >= 0.1);
    assert!(h.mpd("status", "rspc")?.contains("volume: 23"));
    assert_eq!(
        h.mpd("playlistinfo", "default")?
            .matches("file: sentinel.wav")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn connect_reactivation_restores_song_and_notifies_ui_without_controls() -> Result<()> {
    let h = Harness::mixed()?;
    h.cli(&["play", "0"])?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    let original: Vec<_> = h
        .queue()?
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].clone())
        .collect();
    h.remote(json!({"active":false,"status":"paused","position":200,"duration":60000}))?;
    wait(|| Ok(state_is(&h.status()?, "stop")))?;
    let mut client = Protocol::connect(&h.socket)?;
    client.command("status")?;
    client.send("idle player")?;
    h.remote(json!({"active":true,"status":"playing","position":500}))?;
    loop {
        assert_eq!(client.response()?, "changed: player\n");
        if state_is(&h.status()?, "play") {
            break;
        }
        client.send("idle player")?;
    }
    assert_eq!(h.status()?["songid"], 1);
    assert!(h.raw("currentsong")?.contains("file: spotify:track:first"));
    let current: Vec<_> = h
        .queue()?
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].clone())
        .collect();
    assert_eq!(current, original);
    Ok(())
}

#[test]
fn controls_and_ui_exit_preserve_local_playback_and_handoff() -> Result<()> {
    let h = Harness::mixed()?;
    h.cli(&["play", "3"])?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    h.cli(&["volume", "23"])?;
    h.cli(&["seek", "2"])?;
    h.cli(&["pause"])?;
    let before = h.status()?;
    thread::sleep(Duration::from_millis(100));
    let after = h.status()?;
    assert!(state_is(&after, "pause"));
    assert!(
        (after["elapsed"].as_f64().unwrap() - before["elapsed"].as_f64().unwrap()).abs() < 0.05
    );
    assert!(h.mpd("status", "rspc")?.contains("volume: 23"));
    h.cli(&["play"])?;
    h.tui(&[], false)?;
    let before = h.status()?;
    thread::sleep(Duration::from_millis(150));
    assert!(h.status()?["elapsed"].as_f64() > before["elapsed"].as_f64());
    assert!(h.doctor()?.contains("active_source: mpd"));
    h.raw("playid 3")?;
    wait(|| Ok(h.doctor()?.contains("active_source: spotify") && state_is(&h.status()?, "play")))?;
    wait(|| Ok(h.spotify()["volume"] == 23))?;
    assert!(h.mpd("status", "rspc")?.contains("state: stop"));
    assert_eq!(
        h.mpd("playlistinfo", "default")?
            .matches("file: sentinel.wav")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn local_browsing_search_and_queue_edits_need_no_catalog_login() -> Result<()> {
    let h = Harness::mixed()?;
    assert!(h.raw("lsinfo")?.contains("directory: Local Files"));
    assert!(
        h.raw("lsinfo \"Local Files\"")?
            .contains("file: mpd:long.wav")
    );
    assert!(
        h.raw("find \"(file == 'mpd:long.wav')\"")?
            .contains("Title: long.wav")
    );
    assert!(
        h.raw("search \"(any contains 'long')\"")?
            .contains("file: mpd:long.wav")
    );
    h.cli(&["add", h.path("music/long.wav").to_str().unwrap()])?;
    assert_eq!(h.status()?["playlistlength"], 5);
    let reply = h.raw("addid \"mpd:long.wav\"")?;
    let id = reply.trim().strip_prefix("Id: ").unwrap().parse::<u64>()?;
    h.raw(&format!("moveid {id} 0"))?;
    h.raw(&format!("playid {id}"))?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    assert_eq!(h.status()?["songid"], id);
    h.raw("move 0 3")?;
    assert_eq!(h.status()?["songid"], id);
    h.raw(&format!("deleteid {id}"))?;
    assert!(
        !h.queue()?
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id)
    );
    Ok(())
}

#[test]
fn missing_local_file_reports_error_without_skipping_queue() -> Result<()> {
    let h = Harness::mixed()?;
    h.cli(&["play", "3"])?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    let queue = h.queue()?;
    fs::remove_file(h.path("music/long.wav"))?;
    h.cli(&["stop"])?;
    let _ = h.raw("playid 4");
    wait(|| {
        let state = h.status()?;
        Ok(state_is(&state, "stop") && state["error"].as_str().is_some_and(|e| !e.is_empty()))
    })?;
    assert_eq!(h.queue()?, queue);
    assert!(
        h.mpd("outputs", "default")?
            .contains("outputname: Isolated fixture")
    );
    Ok(())
}

#[test]
fn local_playback_works_without_soloist_or_spotify_credentials() -> Result<()> {
    let mut h = Harness::mixed()?;
    h.stop()?;
    h.env.insert(
        "RSPC_SOLOIST_PATH".into(),
        h.path("not-installed").into_os_string(),
    );
    h.env.remove(std::ffi::OsStr::new("RSPC_SOLOIST_API_KEY"));
    h.start()?;
    h.cli(&["play", "3"])?;
    wait(|| Ok(state_is(&h.status()?, "play")))?;
    assert!(h.doctor()?.contains("active_source: mpd"));
    h.cli(&["pause"])?;
    assert!(state_is(&h.status()?, "pause"));
    h.cli(&["play"])?;
    assert!(state_is(&h.status()?, "play"));
    assert!(
        h.raw("lsinfo \"Local Files\"")?
            .contains("file: mpd:long.wav")
    );
    Ok(())
}

#[test]
fn local_playlist_edits_need_no_spotify_login() -> Result<()> {
    let h = Harness::mixed()?;
    h.cli(&["clear"])?;
    h.cli(&["add", h.path("music/long.wav").to_str().unwrap()])?;
    h.cli(&["save", "Local Playlists/Fixture"])?;
    h.raw("playlistadd \"Local Playlists/Fixture\" \"mpd:short.wav\"")?;
    assert!(
        h.raw("listplaylistinfo \"Local Playlists/Fixture\"")?
            .contains("file: mpd:short.wav")
    );
    h.raw("playlistmove \"Local Playlists/Fixture\" 1 0")?;
    h.raw("playlistdelete \"Local Playlists/Fixture\" 1")?;
    assert!(
        !h.raw("listplaylistinfo \"Local Playlists/Fixture\"")?
            .contains("long.wav")
    );
    assert_eq!(h.status()?["playlistlength"], 1);
    h.raw("rename \"Local Playlists/Fixture\" \"Local Playlists/Renamed\"")?;
    h.raw("rm \"Local Playlists/Renamed\"")?;
    assert!(!h.raw("listplaylists")?.contains("Renamed"));
    Ok(())
}

#[test]
fn audio_url_uses_mpd_in_the_same_queue() -> Result<()> {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    let h = Harness::mixed()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let bytes = fs::read(h.path("music/long.wav"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&stop);
    let server = thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set fixture socket read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .expect("set fixture socket write timeout");
                if let Ok(clone) = stream.try_clone() {
                    let mut input = BufReader::new(clone);
                    let mut header = String::new();
                    while input.read_line(&mut header).unwrap_or(0) > 0 {
                        if header.ends_with("\r\n\r\n") || header.len() > 65536 {
                            break;
                        }
                    }
                    let _ = write!(
                        stream,
                        "HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(&bytes);
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let result = (|| -> Result<()> {
        let uri = format!("http://127.0.0.1:{port}/long.wav");
        let reply = h.raw(&format!("addid {}", quote(&uri)))?;
        let id = reply.trim().strip_prefix("Id: ").unwrap();
        h.raw(&format!("playid {id}"))?;
        wait(|| Ok(state_is(&h.status()?, "play")))?;
        assert!(h.doctor()?.contains("active_source: mpd"));
        assert!(h.raw("currentsong")?.contains(&format!("file: mpd:{uri}")));
        assert_eq!(h.status()?["playlistlength"], 5);
        h.cli(&["stop"])?;
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    result
}

#[test]
fn mpd_database_changes_notify_unified_browser() -> Result<()> {
    let h = Harness::mixed()?;
    h.raw("search \"(any contains 'long')\"")?;
    fs::copy(h.path("music/long.wav"), h.path("music/new.wav"))?;
    let mut client = Protocol::connect(&h.socket)?;
    client.send("idle database")?;
    h.mpd("update", "default")?;
    assert_eq!(client.response()?, "changed: database\n");
    wait(|| Ok(h.mpd("stats", "default")?.contains("songs: 4")))?;
    wait(|| {
        Ok(h.raw("search \"(any contains 'new.wav')\"")?
            .contains("file: mpd:new.wav"))
    })?;
    Ok(())
}
