mod support;
use anyhow::Result;
use serde_json::{Value, json};
use std::{fs, io::BufRead, os::unix::fs::PermissionsExt, thread, time::Duration};
use support::{Harness, Protocol, contains, quote, state_is, wait};

#[test]
fn auxiliary_panels_and_song_info_survive_terminal_resizes() -> Result<()> {
    use support::TuiStep::{Key, Resize, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    h.tui_steps(
        &[
            Key(b"oi"),
            WaitFor(b"Song info"),
            Resize {
                rows: 48,
                columns: 240,
            },
            Key(b"j"),
            WaitFor(b"Song info"),
            Resize {
                rows: 20,
                columns: 64,
            },
            Key(b"k"),
            WaitFor(b"Song info"),
            Key(b"\x1b"),
            WaitUntilGone(b"Song info"),
            Key(b"?"),
            WaitFor(b"Keybinds"),
            Key(b"\x1b"),
            Key(b"oo"),
            WaitFor(b"Outputs"),
            Key(b"\r"),
            WaitFor(b"use rspc --local"),
            Key(b"\x1b"),
            Key(b"op"),
            WaitFor(b"Decoder plugins"),
            Key(b"\x1b"),
            Key(b"od"),
            WaitFor(b"No downloads"),
            Key(b"\x1b"),
            Key(b"R"),
            WaitFor(b"Count:"),
            Key(b"\x1b"),
            WaitUntilGone(b"Count:"),
        ],
        false,
    )?;
    assert_eq!(h.queue()?.as_array().unwrap().len(), 3);
    Ok(())
}

#[test]
fn random_songs_sample_the_spotify_library() -> Result<()> {
    let h = Harness::demo()?;
    h.raw("clear")?;
    h.cli(&["addrandom", "song", "2"])?;
    let queue = h.queue()?;
    let tracks = queue.as_array().unwrap();
    assert_eq!(tracks.len(), 2);
    assert!(tracks.iter().all(|track| {
        track["file"]
            .as_str()
            .unwrap()
            .starts_with("spotify:track:")
    }));
    assert_ne!(tracks[0]["file"], tracks[1]["file"]);
    Ok(())
}

#[test]
fn visual_selection_moves_and_deletes_a_contiguous_queue_block() -> Result<()> {
    let h = Harness::demo()?;
    let original = h.queue()?.as_array().unwrap().clone();
    h.tui(&[b"V", b"j", b"J"], false)?;
    let moved = h.queue()?.as_array().unwrap().clone();
    assert_eq!(
        moved.iter().map(|track| &track["file"]).collect::<Vec<_>>(),
        vec![
            &original[2]["file"],
            &original[0]["file"],
            &original[1]["file"]
        ]
    );
    h.tui(&[b"V", b"j", b"d"], false)?;
    assert_eq!(h.queue()?.as_array().unwrap().len(), 1);
    Ok(())
}

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
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    let id = h.status()?["songid"].clone();
    let output = h.tui_steps(
        &[
            Key(b"oI"),
            WaitFor(b"spotify:track:demo1"),
            Key(b"jjjj"),
            Key(b"\r"),
            WaitFor(b"Spotify catalog"),
            Key(b"/"),
            Key(b"Morning"),
            Key(b"\r"),
            Key(b"h"),
            WaitUntilGone(b"Spotify catalog"),
        ],
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
        &[b"6", b"/", b"People", b"\r", b"l", b"l", b"l", b"1"],
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
    let output = h.tui(&[b"2", b"l", b"l", b"oi", b"\x1b"], false)?;
    assert!(!contains(&output, b"timed out"));
    assert!(contains(&output, b"Morning Light"));
    assert_eq!(h.raw("playlistinfo")?.matches("file: ").count(), 3);
    Ok(())
}

#[test]
fn liked_song_shortcuts_save_the_selection_and_current_song_independently() -> Result<()> {
    use support::TuiStep::{Key, WaitFor};

    let mut h = Harness::isolated(true)?;
    let queue = h.path("state/rspc/demo-queue.json");
    fs::create_dir_all(queue.parent().unwrap())?;
    fs::write(
        &queue,
        serde_json::to_vec(&json!({
            "entries": [
                {"id":1,"track":{"uri":"spotify:track:current-fixture","name":"Current fixture","duration_ms":180000}},
                {"id":2,"track":{"uri":"spotify:track:selected-fixture","name":"Selected fixture","duration_ms":180000}},
                {"id":3,"track":{"uri":"spotify:track:demo1","name":"Morning Light","duration_ms":180000}}
            ],
            "next_id":3,"version":1,"current":null
        }))?,
    )?;
    h.start()?;
    h.cli(&["play", "0"])?;
    let before = h.status()?["songid"].clone();
    h.tui_steps(
        &[
            WaitFor(b"Selected fixture"),
            Key(b"j"),
            Key(b"\x13l"),
            Key(b"3"),
            WaitFor(b"File: spotify:track:selected-fixture"),
        ],
        false,
    )?;
    let liked = h.raw("listallinfo \"Liked Songs\"")?;
    assert!(liked.contains("spotify:track:selected-fixture"));
    assert!(!liked.contains("spotify:track:current-fixture"));
    h.tui_steps(
        &[
            WaitFor(b"Selected fixture"),
            Key(b"j"),
            Key(b"2"),
            WaitFor(b"The Example Ensemble"),
            Key(b"\x13L"),
            Key(b"3"),
            WaitFor(b"File: spotify:track:current-fixture"),
        ],
        false,
    )?;
    let liked = h.raw("listallinfo \"Liked Songs\"")?;
    assert!(liked.contains("spotify:track:current-fixture"));
    assert!(liked.contains("spotify:track:selected-fixture"));
    assert_eq!(h.status()?["songid"], before);
    assert!(state_is(&h.status()?, "play"));
    Ok(())
}

#[test]
fn changing_liked_songs_preserves_the_search_query_and_selection() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    h.tui_steps(
        &[
            Key(b"7"),
            WaitFor(b"Any Tag"),
            Key(b"i"),
            Key(b"wInDoW\r"),
            WaitFor(b"Found 1 matching songs"),
            Key(b"l"),
            Key(b"\x1a"),
            WaitFor(b"Remove from Liked Songs"),
            Key(b"/"),
            Key(b"Remove from Liked Songs"),
            Key(b"\r"),
            Key(b"\r"),
            WaitFor(b"Remove 1 song from Spotify Liked Songs?"),
            Key(b"\r"),
            WaitFor(b"Removed 1 song from Liked Songs"),
            Key(b"3"),
            WaitFor(b"Morning Light"),
            WaitUntilGone(b"Second Window"),
            Key(b"7"),
            WaitFor(b"wInDoW"),
            WaitFor(b"Second Window"),
            Key(b"\x1a"),
            WaitFor(b"Add to Liked Songs"),
            Key(b"/"),
            Key(b"Add to Liked Songs"),
            Key(b"\r"),
            Key(b"\r"),
            WaitFor(b"Added 1 song to Liked Songs"),
            Key(b"3"),
            WaitFor(b"Second Window"),
            Key(b"7"),
            WaitFor(b"wInDoW"),
            WaitFor(b"Second Window"),
            Key(b"\x13l"),
            WaitFor(b"Removed 1 song from Liked Songs"),
            WaitFor(b"wInDoW"),
            WaitFor(b"Second Window"),
            Key(b"\x13l"),
            WaitFor(b"Added 1 song to Liked Songs"),
        ],
        false,
    )?;
    assert!(h.raw("lsinfo \"Liked Songs\"")?.contains("Second Window"));
    Ok(())
}

#[test]
fn liked_song_toggle_inverts_each_marked_track() -> Result<()> {
    use support::TuiStep::{Key, WaitFor};

    let h = Harness::demo()?;
    h.raw("rspc_library_remove spotify:track:demo2")?;
    h.tui_steps(
        &[
            Key(b"mjm"),
            Key(b"\x13l"),
            WaitFor(b"Liked 1 and unliked 1 in Liked Songs"),
        ],
        false,
    )?;
    let liked = h.raw("lsinfo \"Liked Songs\"")?;
    assert!(!liked.contains("spotify:track:demo1"));
    assert!(liked.contains("spotify:track:demo2"));
    assert!(liked.contains("spotify:track:demo3"));
    Ok(())
}

#[test]
fn liked_song_menus_follow_membership_and_library_changes_stay_quiet() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};
    let h = Harness::demo()?;
    let liked_menu = h.tui_steps(
        &[
            Key(b"\x1a"),
            WaitFor(b"Remove from Liked Songs"),
            WaitUntilGone(b"Add to Liked Songs"),
            Key(b"\x1b"),
        ],
        false,
    )?;
    assert!(!contains(&liked_menu, b"Add to Liked Songs"));
    h.raw("rspc_library_remove spotify:track:demo1")?;
    let mixed_menu = h.tui_steps(
        &[
            Key(b"\x1a"),
            WaitFor(b"Add to Liked Songs"),
            WaitUntilGone(b"Remove from Liked Songs"),
            Key(b"\x1b"),
            Key(b"mjmj"),
            Key(b"\x1a"),
            WaitFor(b"Add to Liked Songs"),
            WaitFor(b"Remove from Liked Songs"),
            Key(b"/"),
            Key(b"Add to Liked Songs"),
            Key(b"\r"),
            Key(b"\r"),
            WaitUntilGone(b"Add to Liked Songs"),
            WaitFor(b"Added 1 song to Liked Songs"),
            Key(b"ggmjmj"),
            Key(b"\x1a"),
            WaitFor(b"Remove from Liked Songs"),
            WaitUntilGone(b"Add to Liked Songs"),
            Key(b"/"),
            Key(b"Remove from Liked Songs"),
            Key(b"\r"),
            Key(b"\r"),
            WaitFor(b"Remove 2 songs from Spotify Liked Songs?"),
            Key(b"\r"),
            WaitUntilGone(b"Remove 2 songs from Spotify Liked Songs?"),
            WaitFor(b"Removed 2 songs from Liked Songs"),
        ],
        false,
    )?;
    assert!(!contains(&mixed_menu, b"music database has been updated"));
    let liked = h.raw("lsinfo \"Liked Songs\"")?;
    assert_eq!(liked.matches("file: ").count(), 1);
    assert!(liked.contains("spotify:track:demo3"));
    assert_eq!(h.raw("playlistinfo")?.matches("file: ").count(), 3);
    Ok(())
}

#[test]
fn local_song_menus_do_not_offer_spotify_library_actions() -> Result<()> {
    use support::TuiStep::{Key, WaitFor};

    let mut h = Harness::isolated(true)?;
    let queue = h.path("state/rspc/demo-queue.json");
    fs::create_dir_all(queue.parent().unwrap())?;
    fs::write(
        &queue,
        serde_json::to_vec(&json!({
            "entries": [
                {"id":1,"track":{"uri":"spotify:track:demo1","name":"Morning Light","duration_ms":180000}},
                {"id":2,"track":{"uri":"mpd:local.flac","name":"Local fixture","duration_ms":180000}}
            ],
            "next_id":2,"version":1,"current":null
        }))?,
    )?;
    h.start()?;
    let output = h.tui_steps(
        &[
            WaitFor(b"Local fixture"),
            Key(b"j\x1a"),
            WaitFor(b"Add to playlist"),
            Key(b"\x1b"),
        ],
        false,
    )?;
    assert!(!contains(&output, b"Add to Liked Songs"));
    assert!(!contains(&output, b"Remove from Liked Songs"));
    assert_eq!(
        h.raw("lsinfo \"Liked Songs\"")?.matches("file: ").count(),
        3
    );
    Ok(())
}

#[test]
fn album_and_artist_menus_follow_saved_state_and_confirm_removal() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    let queue = h.raw("playlistinfo")?;
    let output = h.tui_steps(
        &[
            Key(b"\x1a"),
            WaitFor(b"Remove album: Example Sessions"),
            WaitFor(b"Unfollow artist: The Example Ensemble"),
            Key(b"/"),
            Key(b"Remove album"),
            Key(b"\r\r"),
            WaitFor(b"Remove album \"Example Sessions\"?"),
            Key(b"\x1b"),
            WaitUntilGone(b"Remove album \"Example Sessions\"?"),
            Key(b"\x1a"),
            WaitFor(b"Remove album: Example Sessions"),
            Key(b"/"),
            Key(b"Remove album"),
            Key(b"\r\r"),
            WaitFor(b"Remove album \"Example Sessions\"?"),
            Key(b"\r"),
            WaitFor(b"Removed album: Example Sessions"),
            Key(b"\x1a"),
            WaitFor(b"Save album: Example Sessions"),
            WaitUntilGone(b"Remove album: Example Sessions"),
            Key(b"/"),
            Key(b"Save album"),
            Key(b"\r\r"),
            WaitFor(b"Saved album: Example Sessions"),
            Key(b"\x1a"),
            WaitFor(b"Unfollow artist: The Example Ensemble"),
            Key(b"/"),
            Key(b"Unfollow artist"),
            Key(b"\r\r"),
            WaitFor(b"Unfollow artist \"The Example Ensemble\"?"),
            Key(b"\r"),
            WaitFor(b"Unfollowed artist: The Example Ensemble"),
            Key(b"\x1a"),
            WaitFor(b"Follow artist: The Example Ensemble"),
            WaitUntilGone(b"Unfollow artist: The Example Ensemble"),
            Key(b"/"),
            Key(b"Follow artist"),
            Key(b"\r\r"),
            WaitFor(b"Following artist: The Example Ensemble"),
            Key(b"4"),
            WaitFor(b"Example Sessions"),
            Key(b"\x1a"),
            WaitFor(b"Remove album: Example Sessions"),
            Key(b"\x1b"),
            Key(b"2"),
            WaitFor(b"The Example Ensemble"),
            Key(b"\x1a"),
            WaitFor(b"Unfollow artist: The Example Ensemble"),
            Key(b"\x1b"),
        ],
        false,
    )?;
    assert!(!contains(&output, b"music database has been updated"));
    assert_eq!(h.raw("playlistinfo")?, queue);
    assert_eq!(
        h.raw("lsinfo \"Liked Songs\"")?.matches("file: ").count(),
        3
    );
    Ok(())
}

#[test]
fn marking_with_m_keeps_the_cursor_on_the_marked_song() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    for tab in [b"1".as_slice(), b"3".as_slice()] {
        let output = h.tui_steps(
            &[
                Key(tab),
                WaitFor(b"Morning Light"),
                Key(b"mm"),
                Key(b"oi"),
                WaitFor(b"Song info"),
                WaitFor(b"spotify:track:demo1"),
                Key(b"\x1b"),
                WaitUntilGone(b"Song info"),
                Key(b"mjm"),
                Key(b"\x1a"),
                WaitFor(b"Remove from Liked Songs"),
                Key(b"/"),
                Key(b"Remove from Liked Songs"),
                Key(b"\r\r"),
                WaitFor(b"Remove 2 songs from Spotify Liked Songs?"),
                Key(b"\x1b"),
            ],
            false,
        )?;
        assert!(contains(&output, b"M "));
    }
    assert_eq!(
        h.raw("lsinfo \"Liked Songs\"")?.matches("file: ").count(),
        3
    );
    Ok(())
}

#[test]
fn liked_songs_removal_supports_confirmation_marks_sorting_and_context_menus() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    let queue = h.raw("playlistinfo")?;
    let playlist = h.raw("listplaylistinfo \"Demo Mix\"")?;
    h.tui_steps(
        &[
            Key(b"3"),
            WaitFor(b"File: spotify:track:demo1"),
            Key(b"sr"),
            Key(b"gg"),
            Key(b"mjmj"),
            Key(b"D"),
            WaitFor(b"Remove 2 songs from Spotify Liked Songs?"),
            Key(b"\x1b"),
            WaitUntilGone(b"Remove 2 songs from Spotify Liked Songs?"),
        ],
        false,
    )?;
    assert_eq!(
        h.raw("lsinfo \"Liked Songs\"")?.matches("file: ").count(),
        3
    );
    h.tui_steps(
        &[
            Key(b"3"),
            WaitFor(b"File: spotify:track:demo3"),
            Key(b"mjmj"),
            Key(b"D"),
            WaitFor(b"Remove 2 songs from Spotify Liked Songs?"),
            Key(b"\r"),
            WaitUntilGone(b"Remove 2 songs from Spotify Liked Songs?"),
            WaitFor(b"File: spotify:track:demo1"),
        ],
        false,
    )?;
    let liked = h.raw("lsinfo \"Liked Songs\"")?;
    assert_eq!(liked.matches("file: ").count(), 1);
    assert!(liked.contains("spotify:track:demo1"));
    h.tui_steps(
        &[
            Key(b"\x1a"),
            WaitFor(b"Remove from Liked Songs"),
            Key(b"/"),
            Key(b"Remove from Liked Songs"),
            Key(b"\r"),
            Key(b"\r"),
            WaitFor(b"Remove 1 song from Spotify Liked Songs?"),
            Key(b"\r"),
            WaitUntilGone(b"Remove 1 song from Spotify Liked Songs?"),
        ],
        false,
    )?;
    assert!(!h.raw("lsinfo \"Liked Songs\"")?.contains("file: "));
    assert_eq!(h.raw("playlistinfo")?, queue);
    assert_eq!(h.raw("listplaylistinfo \"Demo Mix\"")?, playlist);
    assert!(state_is(&h.status()?, "play"));
    Ok(())
}

#[test]
fn removing_a_like_from_directories_keeps_the_liked_songs_page_open() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};
    let h = Harness::demo()?;
    h.cli(&["play", "0"])?;
    h.tui_steps(
        &[
            Key(b"6"),
            WaitFor(b"Liked Songs"),
            Key(b"/"),
            Key(b"Liked Songs"),
            Key(b"\r"),
            Key(b"l"),
            WaitFor(b"File: spotify:track:demo1"),
            Key(b"D"),
            WaitFor(b"Remove 1 song from Spotify Liked Songs?"),
            Key(b"\r"),
            WaitUntilGone(b"Remove 1 song from Spotify Liked Songs?"),
            WaitFor(b"File: spotify:track:demo2"),
        ],
        false,
    )?;
    assert_eq!(
        h.raw("lsinfo \"Liked Songs\"")?.matches("file: ").count(),
        2
    );
    assert!(state_is(&h.status()?, "play"));
    Ok(())
}

#[test]
fn selection_info_on_every_library_page() -> Result<()> {
    use support::TuiStep::{Key, WaitFor, WaitUntilGone};

    let h = Harness::demo()?;
    let pages: &[(&[u8], &[u8])] = &[
        (b"6", b"Liked Songs"),
        (b"2", b"The Example Ensemble"),
        (b"3", b"Morning Light"),
        (b"4", b"Example Sessions"),
        (b"5", b"Demo Mix"),
    ];
    for &(tab, ready) in pages {
        let mut steps = vec![Key(tab), WaitFor(ready)];
        if tab == b"6" {
            steps.extend([Key(b"/"), Key(b"Liked Songs"), Key(b"\r")]);
        }
        let title = if tab == b"3" {
            b"Song info".as_slice()
        } else {
            b"Selection info".as_slice()
        };
        steps.extend([
            Key(b"oi"),
            WaitFor(title),
            Key(b"\x1b"),
            WaitUntilGone(title),
        ]);
        let output = h.tui_steps(&steps, false)?;
        assert!(contains(&output, title));
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
