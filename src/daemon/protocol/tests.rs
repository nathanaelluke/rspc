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

async fn framing_client(
    engine: Arc<Engine>,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve(server, engine));
    let (read, write) = client.into_split();
    let mut read = BufReader::new(read);
    let mut greeting = String::new();
    read.read_line(&mut greeting).await.unwrap();
    assert_eq!(greeting, "OK MPD 0.24.0\n");
    (read, write, task)
}

async fn framing_reply(read: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Vec<u8> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut reply = Vec::new();
        loop {
            let mut line = Vec::new();
            assert_ne!(read.read_until(b'\n', &mut line).await.unwrap(), 0);
            reply.extend_from_slice(&line);
            if let Some(length) = line.strip_prefix(b"binary: ") {
                let length: usize = std::str::from_utf8(length).unwrap().trim().parse().unwrap();
                let start = reply.len();
                reply.resize(start + length + 1, 0);
                read.read_exact(&mut reply[start..]).await.unwrap();
                assert_eq!(reply.last(), Some(&b'\n'));
            }
            if line == b"OK\n" || line.starts_with(b"ACK ") {
                return reply;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn binary_limits_are_per_client_and_preserve_artwork_offsets_and_framing() {
    let (dir, mut engine) = fixture().await;
    let socket = dir.path().join("artwork.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let catalog = Arc::get_mut(&mut Arc::get_mut(&mut engine).unwrap().catalog).unwrap();
    Arc::get_mut(&mut catalog.local).unwrap().settings.address =
        socket.to_string_lossy().into_owned();
    let data: Vec<u8> = (0..20000).map(|i| i as u8).collect();
    let artwork = data.clone();
    let peer = tokio::spawn(async move {
        let mut negotiated = Vec::new();
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write.write_all(b"OK MPD 0.24.0\n").await.unwrap();
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let args = words(&line).unwrap();
            if args[0] == "binarylimit" {
                negotiated.push(args[1].parse::<usize>().unwrap());
                write.write_all(b"OK\n").await.unwrap();
                continue;
            }
            assert!(matches!(args[0].as_str(), "albumart" | "readpicture"));
            assert_eq!(args[1], "fixture.flac");
            let offset: usize = args[2].parse().unwrap();
            let chunk = &artwork[offset..];
            write
                .write_all(
                    format!(
                        "size: {}\ntype: image/png\nbinary: {}\n",
                        artwork.len(),
                        chunk.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            write.write_all(chunk).await.unwrap();
            write.write_all(b"\nOK\n").await.unwrap();
        }
        assert_eq!(negotiated, [262144, 8192]);
    });
    let (mut read, mut write, task) = framing_client(Arc::clone(&engine)).await;
    let (mut other_read, mut other_write, other_task) = framing_client(Arc::clone(&engine)).await;
    write.write_all(b"binarylimit 1024\n").await.unwrap();
    assert_eq!(framing_reply(&mut read).await, b"OK\n");
    for command in ["albumart", "readpicture"] {
        for (offset, length) in [(0, 1024), (1024, 1024), (19968, 32), (20000, 0)] {
            write
                .write_all(format!("{command} mpd:fixture.flac {offset}\nping\n").as_bytes())
                .await
                .unwrap();
            let mut expected =
                format!("size: 20000\ntype: image/png\nbinary: {length}\n").into_bytes();
            expected.extend_from_slice(&data[offset..offset + length]);
            expected.extend_from_slice(b"\nOK\n");
            assert_eq!(framing_reply(&mut read).await, expected);
            assert_eq!(framing_reply(&mut read).await, b"OK\n");
        }
    }
    other_write
        .write_all(b"albumart mpd:fixture.flac 0\n")
        .await
        .unwrap();
    let mut expected = b"size: 20000\ntype: image/png\nbinary: 8192\n".to_vec();
    expected.extend_from_slice(&data[..8192]);
    expected.extend_from_slice(b"\nOK\n");
    assert_eq!(framing_reply(&mut other_read).await, expected);
    write.write_all(b"command_list_ok_begin\nbinarylimit 64\nalbumart mpd:fixture.flac 0\nping\ncommand_list_end\n").await.unwrap();
    let mut expected = b"list_OK\nsize: 20000\ntype: image/png\nbinary: 64\n".to_vec();
    expected.extend_from_slice(&data[..64]);
    expected.extend_from_slice(b"\nlist_OK\nlist_OK\nOK\n");
    assert_eq!(framing_reply(&mut read).await, expected);
    write
        .write_all(b"binarylimit 262144\nreadpicture mpd:fixture.flac 0\nping\n")
        .await
        .unwrap();
    assert_eq!(framing_reply(&mut read).await, b"OK\n");
    let mut expected = b"size: 20000\ntype: image/png\nbinary: 20000\n".to_vec();
    expected.extend_from_slice(&data);
    expected.extend_from_slice(b"\nOK\n");
    assert_eq!(framing_reply(&mut read).await, expected);
    assert_eq!(framing_reply(&mut read).await, b"OK\n");
    other_write
        .write_all(b"readpicture mpd:fixture.flac 0\n")
        .await
        .unwrap();
    let mut expected = b"size: 20000\ntype: image/png\nbinary: 8192\n".to_vec();
    expected.extend_from_slice(&data[..8192]);
    expected.extend_from_slice(b"\nOK\n");
    assert_eq!(framing_reply(&mut other_read).await, expected);
    write.write_all(b"close\n").await.unwrap();
    other_write.write_all(b"close\n").await.unwrap();
    task.await.unwrap().unwrap();
    other_task.await.unwrap().unwrap();
    drop(engine);
    peer.await.unwrap();
}

#[tokio::test]
async fn binary_limit_rejects_invalid_values_without_changing_the_client_limit() {
    let (_dir, engine) = fixture().await;
    let mut client = ClientState::new();
    client
        .execute(&engine, &words("binarylimit 1024").unwrap())
        .await
        .unwrap();
    for command in [
        "binarylimit",
        "binarylimit 0",
        "binarylimit 63",
        "binarylimit -1",
        "binarylimit 8400897",
        "binarylimit 18446744073709551616",
        "binarylimit nope",
        "binarylimit 64 128",
    ] {
        let error = client
            .execute(&engine, &words(command).unwrap())
            .await
            .unwrap_err();
        assert!(failure("binarylimit", 0, &error).starts_with("ACK [2@0] {binarylimit}"));
        assert_eq!(client.binary_limit, 1024);
    }
    for limit in [64, 5 * 1024 * 1024, 8_388_609, MAX_BINARY_LIMIT] {
        client
            .execute(&engine, &words(&format!("binarylimit {limit}")).unwrap())
            .await
            .unwrap();
        assert_eq!(client.binary_limit, limit);
    }
    assert!(
        execute(&engine, &words("binarylimit 1024").unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        execute(&engine, &words("binarylimit 63").unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn command_list_close_executes_preceding_commands_and_skips_following_commands() {
    for (begin, expected) in [
        ("command_list_begin", b"".as_slice()),
        ("command_list_ok_begin", b"list_OK\n".as_slice()),
    ] {
        let (_dir, engine) = fixture().await;
        execute(&engine, &words("repeat 1").unwrap()).await.unwrap();
        execute(&engine, &words("random 1").unwrap()).await.unwrap();
        let (mut read, mut write, task) = framing_client(Arc::clone(&engine)).await;
        write
            .write_all(format!("{begin}\nrepeat 0\nclose\nrandom 0\ncommand_list_end\n").as_bytes())
            .await
            .unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read.read_to_end(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply, expected);
        task.await.unwrap().unwrap();
        let player = engine.player.lock().await;
        assert!(!player.repeat);
        assert!(player.random);
    }
}

#[tokio::test]
async fn command_list_argument_failure_prevents_a_queued_close_and_keeps_the_stream_in_sync() {
    let (_dir, engine) = fixture().await;
    let (mut read, mut write, task) = framing_client(engine).await;
    write
        .write_all(b"command_list_ok_begin\nping\nplay 999999\nclose\ncommand_list_end\nping\n")
        .await
        .unwrap();
    let reply = framing_reply(&mut read).await;
    assert!(reply.starts_with(b"list_OK\nACK [2@1] {play} "));
    assert_eq!(framing_reply(&mut read).await, b"OK\n");
    write.write_all(b"close\n").await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn liking_a_song_updates_the_library_without_disturbing_playback() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    let mut track = super::super::catalog::demo_tracks().remove(0);
    track.uri = Some("spotify:track:unliked-demo".into());
    track.name = "An unliked demo song".into();
    engine.catalog.remember(vec![track]).await;
    let mut events = engine.events.subscribe();
    events.borrow_and_update();
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
    assert!(
        events.has_changed().unwrap(),
        "the UI must be notified immediately"
    );
    assert!(events.borrow_and_update().contains_key("database"));
    execute(
        &engine,
        &words("rspc_library_add spotify:track:unliked-demo").unwrap(),
    )
    .await
    .unwrap();
    assert!(
        !events.has_changed().unwrap(),
        "unchanged likes must not reload the library"
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
async fn saving_albums_and_following_artists_refresh_the_library_only() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    let queue = execute(&engine, &words("playlistinfo").unwrap())
        .await
        .unwrap();
    let playlist = execute(&engine, &words("listplaylistinfo \"Demo Mix\"").unwrap())
        .await
        .unwrap();
    let mut events = engine.events.subscribe();
    for uri in ["spotify:album:demo-album", "spotify:artist:demo-artist"] {
        for action in ["remove", "save"] {
            events.borrow_and_update();
            let command = words(&format!("rspc_library_update {uri} {action}")).unwrap();
            assert_eq!(
                execute(&engine, &command).await.unwrap(),
                b"library_updated: 1\n"
            );
            assert!(events.borrow_and_update().contains_key("database"));
            let items = engine
                .catalog
                .library_items("track", "spotify:track:demo1")
                .await
                .unwrap();
            assert_eq!(
                items.iter().find(|item| item.uri == uri).unwrap().saved,
                action == "save"
            );
            assert_eq!(
                execute(&engine, &command).await.unwrap(),
                b"library_updated: 0\n"
            );
            assert!(!events.has_changed().unwrap());
        }
    }
    assert!(
        execute(
            &engine,
            &words("rspc_library_update mpd:file.flac save").unwrap()
        )
        .await
        .is_err()
    );
    assert!(
        execute(
            &engine,
            &words("rspc_library_update spotify:album:demo-album invalid").unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(
        execute(&engine, &words("playlistinfo").unwrap())
            .await
            .unwrap(),
        queue
    );
    assert_eq!(
        execute(&engine, &words("listplaylistinfo \"Demo Mix\"").unwrap())
            .await
            .unwrap(),
        playlist
    );
    assert_eq!(engine.catalog.liked().await.unwrap().len(), 3);
    let state = engine.playback().await;
    assert!(state.playing);
    assert_eq!(state.entry.unwrap().id, 1);
}

#[tokio::test]
async fn unliking_a_song_refreshes_the_library_and_preserves_queue_and_playlists() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("playid 1").unwrap()).await.unwrap();
    let queue = execute(&engine, &words("playlistinfo").unwrap())
        .await
        .unwrap();
    let playlist = execute(&engine, &words("listplaylistinfo \"Demo Mix\"").unwrap())
        .await
        .unwrap();
    let mut events = engine.events.subscribe();
    assert!(COMMANDS.contains(&"rspc_library_remove"));
    assert!(
        execute(
            &engine,
            &words("rspc_library_remove spotify:track:demo1 mpd:file.flac").unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(engine.catalog.liked().await.unwrap().len(), 3);
    assert_eq!(
        execute(
            &engine,
            &words(
                "rspc_library_remove spotify:track:demo1 spotify:track:demo1 spotify:track:missing"
            )
            .unwrap()
        )
        .await
        .unwrap(),
        b"library_removed: 1\n"
    );
    assert!(events.has_changed().unwrap());
    assert!(events.borrow_and_update().contains_key("database"));
    assert_eq!(
        execute(
            &engine,
            &words("rspc_library_remove spotify:track:demo1").unwrap()
        )
        .await
        .unwrap(),
        b"library_removed: 0\n"
    );
    assert!(!events.has_changed().unwrap());
    let liked = execute(&engine, &words("listallinfo \"Liked Songs\"").unwrap())
        .await
        .unwrap();
    assert!(
        !String::from_utf8(liked)
            .unwrap()
            .contains("spotify:track:demo1")
    );
    assert_eq!(
        execute(&engine, &words("playlistinfo").unwrap())
            .await
            .unwrap(),
        queue
    );
    assert_eq!(
        execute(&engine, &words("listplaylistinfo \"Demo Mix\"").unwrap())
            .await
            .unwrap(),
        playlist
    );
    let playback = engine.playback().await;
    assert!(playback.playing);
    assert_eq!(playback.entry.unwrap().id, 1);
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
        ("play 999999", "ACK [2@0]"),
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
    assert!(line.starts_with("ACK [2@1] {play}"), "{line}");
    write.write_all(b"close\n").await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn playlist_load_honors_ranges_without_disturbing_existing_queue_entries() {
    let (_dir, engine) = fixture().await;
    let original: Vec<_> = engine
        .player
        .lock()
        .await
        .queue
        .entries
        .iter()
        .map(|entry| entry.track.clone())
        .collect();
    for (selection, expected) in [
        ("0:1", vec![0]),
        ("1", vec![1]),
        ("1:", vec![1, 2]),
        ("1:99", vec![1, 2]),
        ("3:", vec![]),
        ("4:99", vec![]),
        ("1:1", vec![]),
    ] {
        execute(&engine, &words("clear").unwrap()).await.unwrap();
        execute(&engine, &words("add spotify:track:demo3").unwrap())
            .await
            .unwrap();
        execute(
            &engine,
            &words(&format!("load \"Demo Mix\" {selection}")).unwrap(),
        )
        .await
        .unwrap();
        let entries = &engine.player.lock().await.queue.entries;
        let actual: Vec<_> = entries
            .iter()
            .map(|entry| entry.track.uri.clone())
            .collect();
        let expected: Vec<_> = std::iter::once(2)
            .chain(expected)
            .map(|index| original[index].uri.clone())
            .collect();
        assert_eq!(actual, expected, "load range {selection}");
    }
    let before = execute(&engine, &words("playlistinfo").unwrap())
        .await
        .unwrap();
    for selection in ["2:1", "invalid", "0:invalid", "-1:"] {
        assert!(
            execute(
                &engine,
                &words(&format!("load \"Demo Mix\" {selection}")).unwrap()
            )
            .await
            .is_err(),
            "{selection}"
        );
        assert_eq!(
            execute(&engine, &words("playlistinfo").unwrap())
                .await
                .unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn playlist_save_supports_create_replace_and_append_modes() {
    let (_dir, engine) = fixture().await;
    execute(&engine, &words("save Copy").unwrap())
        .await
        .unwrap();
    let original = engine.catalog.playlist_tracks("Copy").await.unwrap();
    assert!(
        execute(&engine, &words("save Copy").unwrap())
            .await
            .is_err()
    );
    assert!(
        execute(&engine, &words("save Copy create").unwrap())
            .await
            .is_err()
    );
    execute(&engine, &words("clear").unwrap()).await.unwrap();
    execute(&engine, &words("add spotify:track:demo2").unwrap())
        .await
        .unwrap();
    let queue = execute(&engine, &words("playlistinfo").unwrap())
        .await
        .unwrap();
    let mut events = engine.events.subscribe();
    events.borrow_and_update();
    execute(&engine, &words("save Copy replace").unwrap())
        .await
        .unwrap();
    assert!(events.borrow_and_update().contains_key("stored_playlist"));
    assert_eq!(
        engine.catalog.playlist_tracks("Copy").await.unwrap(),
        vec![original[1].clone()]
    );
    execute(&engine, &words("save Copy append").unwrap())
        .await
        .unwrap();
    assert_eq!(
        engine.catalog.playlist_tracks("Copy").await.unwrap(),
        vec![original[1].clone(); 2]
    );
    assert_eq!(
        execute(&engine, &words("playlistinfo").unwrap())
            .await
            .unwrap(),
        queue
    );
    execute(&engine, &words("save Explicit create").unwrap())
        .await
        .unwrap();
    assert_eq!(
        engine.catalog.playlist_tracks("Explicit").await.unwrap(),
        vec![original[1].clone()]
    );
    execute(&engine, &words("clear").unwrap()).await.unwrap();
    execute(&engine, &words("save Copy replace").unwrap())
        .await
        .unwrap();
    assert!(
        engine
            .catalog
            .playlist_tracks("Copy")
            .await
            .unwrap()
            .is_empty()
    );
    execute(&engine, &words("save Copy append").unwrap())
        .await
        .unwrap();
    assert!(
        engine
            .catalog
            .playlist_tracks("Copy")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn playlist_save_rejects_invalid_modes_and_missing_targets_without_mutations() {
    let (_dir, engine) = fixture().await;
    let original = engine.catalog.playlist_tracks("Demo Mix").await.unwrap();
    let queue = execute(&engine, &words("playlistinfo").unwrap())
        .await
        .unwrap();
    let mut events = engine.events.subscribe();
    events.borrow_and_update();
    for command in [
        "save \"Demo Mix\" invalid",
        "save Missing invalid",
        "save Missing replace",
        "save Missing append",
        "save \"Local Playlists/Missing\" invalid",
    ] {
        let error = execute(&engine, &words(command).unwrap())
            .await
            .unwrap_err();
        if command.ends_with("invalid") {
            assert!(error.to_string().contains("save mode"), "{error:#}");
            assert!(failure("save", 0, &error).starts_with("ACK [2@0] {save}"));
        }
        assert_eq!(
            engine.catalog.playlist_tracks("Demo Mix").await.unwrap(),
            original
        );
        assert!(engine.catalog.playlist_tracks("Missing").await.is_err());
        assert_eq!(
            execute(&engine, &words("playlistinfo").unwrap())
                .await
                .unwrap(),
            queue
        );
        assert!(!events.has_changed().unwrap());
    }
}
