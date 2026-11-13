use super::*;

#[test]
fn an_oversized_position_returns_an_error_instead_of_wrapping() {
    assert!(range(&usize::MAX.to_string(), 3).is_err());
}

#[test]
fn addition_dates_are_sent_as_standard_mpd_metadata() {
    let mut track = super::super::catalog::demo_tracks().remove(0);
    track.added_at = Some("2026-11-01T10:00:00Z".parse().unwrap());
    let reply = song(&Entry { id: 1, track }, Some(0));
    assert!(reply.contains("Added: 2026-11-01T10:00:00+00:00\n"));
    assert!(reply.contains("Date: 2026-01-01\n"));
}

#[tokio::test]
async fn oversized_commands_are_rejected_without_waiting_for_a_newline() {
    for idle in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(
            true,
            dir.path().join("q.json"),
            tokio::sync::mpsc::unbounded_channel().0,
        )
        .await
        .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve(server, engine));
        let mut greeting = [0; 14];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"OK MPD 0.24.0\n");
        if idle {
            client.write_all(b"idle player\n").await.unwrap();
        }
        client
            .write_all(&vec![b'x'; MAX_COMMAND_BYTES + 1])
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("unterminated command must still be bounded")
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("command too long"));
    }
}

#[test]
fn command_lists_limit_total_bytes_as_well_as_command_count() {
    let mut list = CommandList {
        separators: false,
        bytes: 0,
        commands: Vec::new(),
    };
    for _ in 0..MAX_COMMAND_LIST_BYTES / MAX_COMMAND_BYTES {
        list.push(vec!["ping".into()], MAX_COMMAND_BYTES).unwrap();
    }
    assert!(list.commands.len() < MAX_COMMAND_LIST_LENGTH);
    assert!(list.push(vec!["ping".into()], 1).is_err());
    assert_eq!(list.bytes, MAX_COMMAND_LIST_BYTES);
}

async fn fixture() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        true,
        dir.path().join("q.json"),
        tokio::sync::mpsc::unbounded_channel().0,
    )
    .await
    .unwrap();
    (dir, engine)
}

#[tokio::test]
async fn liking_a_song_updates_the_library_without_disturbing_playback() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    let mut track = super::super::catalog::demo_tracks().remove(0);
    track.uri = Some("spotify:track:unliked-demo".into());
    track.name = "An unliked demo song".into();
    engine.catalog.remember(vec![track]).await;
    assert!(COMMANDS.contains(&"rspc_library_add"));
    assert_eq!(
        execute(
            &engine,
            &words("rspc_library_add spotify:track:unliked-demo").unwrap()
        )
        .await
        .unwrap(),
        b"library_added: 1\n",
    );
    let liked = execute(&engine, &words("listallinfo \"Liked Songs\"").unwrap())
        .await
        .unwrap();
    assert!(
        String::from_utf8(liked)
            .unwrap()
            .contains("An unliked demo song")
    );
    let state = engine.playback().await;
    assert!(state.playing);
    assert_eq!(state.entry.unwrap().id, 1);
    assert!(
        execute(&engine, &words("rspc_library_add mpd:file.flac").unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rejected_seek_does_not_change_the_track_or_start_playback() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    execute(&engine, &words("pause 1").unwrap()).await.unwrap();
    for command in ["seekid 2 NaN", "seekid 2 invalid", "seek 1 inf"] {
        assert!(execute(&engine, &words(command).unwrap()).await.is_err());
        let state = engine.playback().await;
        assert_eq!(state.entry.unwrap().id, 1, "{command}");
        assert!(!state.playing, "{command}");
    }
}

#[tokio::test]
async fn seeking_a_stopped_track_starts_playback() {
    let (_dir, engine) = fixture().await;
    for command in ["playid 1", "stop", "seekid 1 3"] {
        execute(&engine, &words(command).unwrap()).await.unwrap();
    }
    let state = engine.playback().await;
    assert!(state.playing && !state.stopped);
    assert_eq!(state.entry.unwrap().id, 1);
    assert!(state.position_ms >= 3000);
}

#[tokio::test]
async fn boolean_modes_reject_oneshot() {
    let (_dir, engine) = fixture().await;
    for command in ["repeat oneshot", "random oneshot"] {
        assert!(execute(&engine, &words(command).unwrap()).await.is_err());
        let player = engine.player.lock().await;
        assert!(!player.repeat && !player.random);
    }
    for command in ["single oneshot", "consume oneshot"] {
        execute(&engine, &words(command).unwrap()).await.unwrap();
    }
    let player = engine.player.lock().await;
    assert_eq!(player.single, 2);
    assert!(player.queue.consume && player.queue.consume_oneshot);
}

#[tokio::test]
async fn deleting_the_paused_song_keeps_its_replacement_paused() {
    let (_dir, engine) = fixture().await;
    for command in ["playid 1", "pause 1", "deleteid 1"] {
        execute(&engine, &words(command).unwrap()).await.unwrap();
    }
    let state = engine.playback().await;
    assert_eq!(state.entry.unwrap().id, 2);
    assert!(!state.playing && !state.stopped);
    assert_eq!(engine.player.lock().await.queue.entries.len(), 2);
    for command in ["seekcur 12", "pause 0"] {
        execute(&engine, &words(command).unwrap()).await.unwrap();
    }
    let resumed = engine.playback().await;
    assert_eq!(resumed.entry.unwrap().id, 2);
    assert!(resumed.playing && resumed.position_ms >= 12_000);
}

#[tokio::test]
async fn queue_edits_are_saved_even_when_the_replacement_cannot_play() {
    let (_dir, engine) = fixture().await;
    engine.player.lock().await.queue.entries[1].track.uri = Some("unavailable:source".into());
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    engine.flush().await.unwrap();
    assert!(
        execute(&engine, &words("deleteid 1").unwrap())
            .await
            .is_err()
    );
    engine.flush().await.unwrap();
    let saved = super::super::queue::Queue::load(&engine.queue_path).unwrap();
    assert_eq!(
        saved
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(saved.current, Some(2));
    assert!(engine.playback().await.stopped);
}

#[tokio::test]
async fn extreme_volume_arguments_saturate_instead_of_overflowing() {
    let (_dir, engine) = fixture().await;
    for (value, expected) in [(i64::MAX, 100), (i64::MIN, 0), (42, 42)] {
        execute(&engine, &words(&format!("volume {value}")).unwrap())
            .await
            .unwrap();
        assert_eq!(engine.playback().await.volume, expected);
    }
}

async fn idle_cancellation_race(fragmented: bool) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        true,
        dir.path().join("q.json"),
        tokio::sync::mpsc::unbounded_channel().0,
    )
    .await
    .unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve(server, Arc::clone(&engine)));
    let (read, mut write) = client.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("OK MPD"));
    for _ in 0..32 {
        write.write_all(b"idle player\n").await.unwrap();
        if fragmented {
            write.write_all(b"noi").await.unwrap();
            // Let the server read part of the cancellation before the event wins.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        engine.changed(vec!["player"]);
        for expected in ["changed: player\n", "OK\n"] {
            line.clear();
            tokio::time::timeout(std::time::Duration::from_secs(1), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(line, expected);
        }
        // the frontend may already have sent noidle when the event completes idle.
        write
            .write_all(if fragmented {
                b"dle\nstatus\nping\n"
            } else {
                b"noidle\nstatus\nping\n"
            })
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(std::time::Duration::from_secs(1), read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(
            line.starts_with("partition: "),
            "status received a stale reply: {line:?}"
        );
        while line != "OK\n" {
            line.clear();
            tokio::time::timeout(std::time::Duration::from_secs(1), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert!(!line.is_empty() && !line.starts_with("ACK"), "{line:?}");
        }
        line.clear();
        tokio::time::timeout(std::time::Duration::from_secs(1), read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "OK\n", "ping response");
    }
    write.write_all(b"close\n").await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn late_idle_cancellation_keeps_following_commands_in_sync() {
    idle_cancellation_race(false).await;
}

#[tokio::test]
async fn fragmented_idle_cancellation_survives_a_player_event() {
    idle_cancellation_race(true).await;
}

#[test]
fn command_quoting_preserves_rmpc_filter_expressions() {
    let a = words("find \"(Artist == 'John\\\\'s Band')\"").unwrap();
    assert_eq!(a[0], "find");
    assert_eq!(a[1], "(Artist == 'John\\'s Band')");
    assert!(words("add \"unterminated").is_err());
    assert_eq!(clean("title\nOK\r\0"), "title OK  ");
}
#[tokio::test]
async fn queue_commands_preserve_ids_and_playback_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        true,
        dir.path().join("q.json"),
        tokio::sync::mpsc::unbounded_channel().0,
    )
    .await
    .unwrap();
    execute(&engine, &words("play 0").unwrap()).await.unwrap();
    let current = engine.player.lock().await.queue.current;
    execute(&engine, &words("move 0 2").unwrap()).await.unwrap();
    assert_eq!(engine.player.lock().await.queue.current, current);
    assert!(engine.playback().await.playing);
    execute(&engine, &words("pause 1").unwrap()).await.unwrap();
    assert!(!engine.playback().await.playing);
    execute(&engine, &words("setvol 42").unwrap())
        .await
        .unwrap();
    assert_eq!(engine.playback().await.volume, 42);
    assert!(
        execute(&engine, &words("playid 99999").unwrap())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn wire_errors_distinguish_unknown_commands_from_playback_failures() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        true,
        dir.path().join("q.json"),
        tokio::sync::mpsc::unbounded_channel().0,
    )
    .await
    .unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve(server, engine));
    let (read, mut write) = client.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("OK MPD"));
    for (command, expected) in [
        ("not_a_command", "ACK [5@0]"),
        ("playid 999999", "ACK [50@0]"),
        ("play 999999", "ACK [52@0]"),
        ("readpicture spotify:track:demo1 0", "ACK [50@0]"),
        ("albumart spotify:track:demo1 0", "ACK [50@0]"),
    ] {
        write
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert!(line.starts_with(expected), "{line}");
    }
    write
        .write_all(b"command_list_ok_begin\nping\nplay 999999\ncommand_list_end\n")
        .await
        .unwrap();
    line.clear();
    read.read_line(&mut line).await.unwrap();
    assert_eq!(line, "list_OK\n");
    line.clear();
    read.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("ACK [52@1] {play}"), "{line}");
    write.write_all(b"close\n").await.unwrap();
    task.await.unwrap().unwrap();
}
