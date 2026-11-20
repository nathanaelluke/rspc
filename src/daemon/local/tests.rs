use super::*;

fn expired_player(settings: Settings, lease_path: PathBuf, active: bool) -> Player {
    let (client, server) = UnixStream::pair().unwrap();
    drop(server);
    Player {
        settings,
        state: Mutex::new(PlayerState {
            wire: Wire::new(Box::new(client)),
            lease: active.then(|| Lease {
                name: "fixture".into(),
                origin: "default".into(),
                enabled: true,
                volume: None,
            }),
            current: active.then(|| "mpd:fixture.flac".into()),
            observed: active,
            ended: !active,
        }),
        generation: AtomicU64::new(1),
        closed: AtomicBool::new(false),
        tx: mpsc::unbounded_channel().0,
        done: watch::channel(false).0,
        lease_path,
    }
}

fn scripted_peer(
    listener: tokio::net::UnixListener,
    commands: Vec<(&'static str, &'static str)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufStream::new(stream);
        stream.write_all(b"OK MPD 0.24.0\n").await.unwrap();
        stream.flush().await.unwrap();
        for (expected, response) in commands {
            let mut command = String::new();
            stream.read_line(&mut command).await.unwrap();
            assert_eq!(command.trim_end(), expected);
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        }
    })
}

#[tokio::test]
async fn released_local_player_does_not_block_spotify_when_mpd_is_gone() {
    let directory = tempfile::tempdir().unwrap();
    let player = expired_player(
        Settings {
            address: directory
                .path()
                .join("missing.sock")
                .to_string_lossy()
                .into_owned(),
            password: None,
        },
        directory.path().join("lease.json"),
        false,
    );
    player.stop().await.unwrap();
    player.stop().await.unwrap();
}

#[tokio::test]
async fn handoff_reconnects_and_restores_output_after_an_idle_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let server = scripted_peer(
        listener,
        vec![
            (
                "listpartitions",
                "partition: default\npartition: rspc\nOK\n",
            ),
            ("partition \"rspc\"", "OK\n"),
            ("stop", "OK\n"),
            ("partition \"default\"", "OK\n"),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n",
            ),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n",
            ),
            ("partition \"rspc\"", "OK\n"),
        ],
    );
    let player = expired_player(
        Settings {
            address: address.to_string_lossy().into_owned(),
            password: None,
        },
        directory.path().join("lease.json"),
        true,
    );
    tokio::time::timeout(Duration::from_secs(3), player.stop())
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    let state = player.state.lock().await;
    assert!(state.current.is_none() && state.lease.is_none());
}

#[tokio::test]
async fn controls_reconnect_in_the_owned_partition_before_mutating_playback() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let server = scripted_peer(
        listener,
        vec![
            ("partition \"rspc\"", "OK\n"),
            ("pause 1", "OK\n"),
            ("ping", "OK\n"),
            ("status", "state: pause\nvolume: 40\nelapsed: 2.000\nOK\n"),
            ("currentsong", "file: fixture.flac\nOK\n"),
        ],
    );
    let player = expired_player(
        Settings {
            address: address.to_string_lossy().into_owned(),
            password: None,
        },
        directory.path().join("lease.json"),
        true,
    );
    tokio::time::timeout(Duration::from_secs(3), player.pause(true))
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn library_reads_reconnect_but_uncertain_writes_are_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let library = Library::with_settings(Settings {
        address: address.to_string_lossy().into_owned(),
        password: None,
    });
    let (client, server) = UnixStream::pair().unwrap();
    drop(server);
    *library.wire.lock().await = Some(Wire::new(Box::new(client)));
    let server = scripted_peer(listener, vec![("lsinfo \"\"", "file: fixture.flac\nOK\n")]);
    assert_eq!(
        library.request("lsinfo \"\"").await.unwrap(),
        b"file: fixture.flac\n"
    );
    server.await.unwrap();
    std::fs::remove_file(&address).unwrap();
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let applied = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        let mut command = String::new();
        server.read_line(&mut command).await.unwrap();
        assert_eq!(command, "ping\n");
        server.write_all(b"OK\n").await.unwrap();
        server.flush().await.unwrap();
        command.clear();
        server.read_line(&mut command).await.unwrap();
        assert_eq!(command, "playlistadd \"mix\" \"fixture.flac\"\n");
        // The edit applied, but the acknowledgement was lost.
    });
    *library.wire.lock().await = Some(Wire::new(Box::new(client)));
    assert!(
        library
            .request("playlistadd \"mix\" \"fixture.flac\"")
            .await
            .unwrap_err()
            .to_string()
            .contains("may have applied")
    );
    applied.await.unwrap();
    assert!(library.wire.lock().await.is_none());
    // A write must fail without attempting to open a replacement connection.
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    let (client, server) = UnixStream::pair().unwrap();
    drop(server);
    *library.wire.lock().await = Some(Wire::new(Box::new(client)));
    let server = scripted_peer(
        listener,
        vec![("playlistadd \"mix\" \"fixture.flac\"", "OK\n")],
    );
    library
        .request("playlistadd \"mix\" \"fixture.flac\"")
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn interrupted_artwork_reads_restart_the_chunk_on_a_new_connection() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let library = Library::with_settings(Settings {
        address: address.to_string_lossy().into_owned(),
        password: None,
    });
    let (client, peer) = UnixStream::pair().unwrap();
    *library.wire.lock().await = Some(Wire::new(Box::new(client)));
    let partial = tokio::spawn(async move {
        let mut peer = BufStream::new(peer);
        let mut command = String::new();
        peer.read_line(&mut command).await.unwrap();
        assert_eq!(command, "readpicture \"fixture.flac\" 0\n");
        peer.write_all(b"size: 4\nbinary: 4\nab").await.unwrap();
        peer.flush().await.unwrap();
    });
    let server = scripted_peer(
        listener,
        vec![(
            "readpicture \"fixture.flac\" 0",
            "size: 4\nbinary: 4\nabcd\nOK\n",
        )],
    );
    assert_eq!(
        library
            .request("readpicture \"fixture.flac\" 0")
            .await
            .unwrap(),
        b"size: 4\nbinary: 4\nabcd\n"
    );
    partial.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn first_local_load_preserves_requested_volume_after_reconnect() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let player = expired_player(
        Settings {
            address: address.to_string_lossy().into_owned(),
            password: None,
        },
        directory.path().join("lease.json"),
        false,
    );
    player.generation.store(0, Ordering::Relaxed);
    let (client, peer) = UnixStream::pair().unwrap();
    player.state.lock().await.wire = Wire::new(Box::new(client));
    let first = tokio::spawn(async move {
        let mut peer = BufStream::new(peer);
        for (expected, response) in [
            ("ping", "OK\n"),
            ("partition \"default\"", "OK\n"),
            ("status", "volume: 88\nstate: stop\nOK\n"),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n",
            ),
            ("partition \"rspc\"", "OK\n"),
            ("moveoutput \"fixture\"", "OK\n"),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n",
            ),
            ("enableoutput 0", "OK\n"),
            ("stop", ""),
        ] {
            let mut command = String::new();
            peer.read_line(&mut command).await.unwrap();
            assert_eq!(command.trim_end(), expected);
            peer.write_all(response.as_bytes()).await.unwrap();
            peer.flush().await.unwrap();
        }
    });
    let second = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufStream::new(stream);
        stream.write_all(b"OK MPD 0.24.0\n").await.unwrap();
        stream.flush().await.unwrap();
        for (expected, response) in [
            ("partition \"rspc\"", "OK\n"),
            ("partition \"rspc\"", "OK\n"),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n",
            ),
            ("enableoutput 0", "OK\n"),
            ("stop", "OK\n"),
            ("clear", "OK\n"),
            ("repeat 0", "OK\n"),
            ("random 0", "OK\n"),
            ("single 1", "OK\n"),
            ("consume 0", "OK\n"),
            ("addid \"next.flac\"", "Id: 1\nOK\n"),
            ("setvol 25", "OK\n"),
            ("play 0", "OK\n"),
            ("ping", "OK\n"),
            ("status", "volume: 25\nstate: play\nelapsed: 1.000\nOK\n"),
            ("currentsong", "file: next.flac\nOK\n"),
        ] {
            let mut command = String::new();
            stream.read_line(&mut command).await.unwrap();
            if command == "status\n" && expected != "status" {
                stream
                    .write_all(b"volume: 88\nstate: stop\nOK\n")
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                command.clear();
                stream.read_line(&mut command).await.unwrap();
            }
            assert_eq!(command.trim_end(), expected);
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        }
    });
    tokio::time::timeout(
        Duration::from_secs(3),
        player.load("mpd:next.flac", 2, 25, 0),
    )
    .await
    .unwrap()
    .unwrap();
    first.await.unwrap();
    second.await.unwrap();
    let state = player.state.lock().await;
    assert_eq!(state.current.as_deref(), Some("mpd:next.flac"));
    assert!(state.observed && !state.ended);
    assert_eq!(player.generation.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn reconnecting_a_load_is_atomic_with_respect_to_the_idle_observer() {
    let directory = tempfile::tempdir().unwrap();
    let address = directory.path().join("mpd.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let player = Arc::new(expired_player(
        Settings {
            address: address.to_string_lossy().into_owned(),
            password: None,
        },
        directory.path().join("lease.json"),
        true,
    ));
    let (client, peer) = UnixStream::pair().unwrap();
    player.state.lock().await.wire = Wire::new(Box::new(client));
    let (failed, failure) = tokio::sync::oneshot::channel();
    let first = tokio::spawn(async move {
        let mut peer = BufStream::new(peer);
        for (expected, response) in [
            ("ping", "OK\n"),
            ("status", "volume: 40\nOK\n"),
            ("partition \"rspc\"", "OK\n"),
            (
                "outputs",
                "outputid: 0\noutputname: fixture\nplugin: pulse\nOK\n",
            ),
            ("enableoutput 0", "OK\n"),
        ] {
            let mut command = String::new();
            peer.read_line(&mut command).await.unwrap();
            assert_eq!(command.trim_end(), expected);
            peer.write_all(response.as_bytes()).await.unwrap();
            peer.flush().await.unwrap();
        }
        let mut command = String::new();
        peer.read_line(&mut command).await.unwrap();
        assert_eq!(command, "stop\n");
        failed.send(()).unwrap();
        tokio::task::yield_now().await;
        // Drop the connection before clearing the previous song. An observer
        // must not see this half-loaded state under the new request ID.
    });
    let mut commands = vec![
        ("partition \"rspc\"", "OK\n"),
        ("status", "volume: 40\nOK\n"),
        ("partition \"rspc\"", "OK\n"),
        (
            "outputs",
            "outputid: 0\noutputname: fixture\nplugin: pulse\nOK\n",
        ),
        ("enableoutput 0", "OK\n"),
    ];
    for command in [
        "stop",
        "clear",
        "repeat 0",
        "random 0",
        "single 1",
        "consume 0",
        "addid \"next.flac\"",
        "setvol 40",
        "play 0",
    ] {
        commands.push((command, "OK\n"));
    }
    for _ in 0..2 {
        commands.extend([
            ("ping", "OK\n"),
            ("status", "volume: 40\nstate: play\nelapsed: 1.000\nOK\n"),
            ("currentsong", "file: next.flac\nOK\n"),
        ]);
    }
    let second = scripted_peer(listener, commands);
    let observer = Arc::clone(&player);
    let observe = tokio::spawn(async move {
        failure.await.unwrap();
        observer.observe().await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        player.load("mpd:next.flac", 2, 40, 0),
    )
    .await
    .unwrap()
    .unwrap();
    observe.await.unwrap().unwrap();
    first.await.unwrap();
    second.await.unwrap();
}

#[tokio::test]
async fn cancelled_commands_cannot_leave_a_late_reply_for_the_next_command() {
    let (client, server) = UnixStream::pair().unwrap();
    let (partial_tx, partial_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        let mut command = String::new();
        server.read_line(&mut command).await.unwrap();
        assert_eq!(command, "pause 1\n");
        server.write_all(b"OK").await.unwrap();
        server.flush().await.unwrap();
        partial_tx.send(()).unwrap();
        release_rx.await.unwrap();
        server.write_all(b"\n").await.unwrap();
        server.flush().await.unwrap();
        command.clear();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), server.read_line(&mut command))
                .await
                .is_err()
        );
    });
    let mut wire = Wire::new(Box::new(client));
    tokio::select! {
        result = wire.command("pause 1") => panic!("command completed before its reply: {result:?}"),
        result = partial_rx => result.unwrap(),
    }
    release_tx.send(()).unwrap();
    assert!(disconnected(&wire.command("status").await.unwrap_err()));
    task.await.unwrap();
}

#[tokio::test]
async fn complete_mpd_errors_leave_the_connection_usable() {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        for (expected, response) in [
            ("pause 1\n", "ACK [50@0] {pause} Not playing\n"),
            ("ping\n", "OK\n"),
        ] {
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            assert_eq!(command, expected);
            server.write_all(response.as_bytes()).await.unwrap();
            server.flush().await.unwrap();
        }
    });
    let mut wire = Wire::new(Box::new(client));
    let error = wire.command("pause 1").await.unwrap_err();
    assert!(!disconnected(&error));
    assert!(error.to_string().contains("Not playing"));
    wire.command("ping").await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn truncated_mpd_errors_are_transport_failures() {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        let mut command = String::new();
        server.read_line(&mut command).await.unwrap();
        assert_eq!(command, "pause 1\n");
        server
            .write_all(b"ACK [50@0] {pause} Not playing")
            .await
            .unwrap();
        server.flush().await.unwrap();
    });
    let mut wire = Wire::new(Box::new(client));
    assert!(disconnected(&wire.command("pause 1").await.unwrap_err()));
    assert!(disconnected(&wire.command("ping").await.unwrap_err()));
    task.await.unwrap();
}

#[tokio::test]
async fn startup_volume_waits_for_the_output_mixer() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(&mut server);
        for reply in ["ACK [5@0] {setvol} All outputs are disabled\n", "OK\n"] {
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            assert_eq!(command, "setvol 40\n");
            server.write_all(reply.as_bytes()).await.unwrap();
            server.flush().await.unwrap();
        }
    });
    let mut wire = Wire::new(Box::new(client));
    wire.volume(40, true).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn startup_volume_does_not_retry_other_mpd_errors() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        let mut command = String::new();
        let mut server = BufStream::new(&mut server);
        server.read_line(&mut command).await.unwrap();
        assert_eq!(command, "setvol 40\n");
        server
            .write_all(b"ACK [4@0] {setvol} permission denied\n")
            .await
            .unwrap();
        server.flush().await.unwrap();
    });
    let mut wire = Wire::new(Box::new(client));
    let error = wire.volume(40, true).await.unwrap_err();
    assert!(error.to_string().contains("permission denied"));
    task.await.unwrap();
}

#[tokio::test]
async fn startup_volume_stops_retrying_if_the_mixer_never_opens() {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        loop {
            let mut command = String::new();
            if server.read_line(&mut command).await.unwrap() == 0 {
                break;
            }
            assert_eq!(command, "setvol 40\n");
            server
                .write_all(b"ACK [5@0] {setvol} All outputs are disabled\n")
                .await
                .unwrap();
            server.flush().await.unwrap();
        }
    });
    let mut wire = Wire::new(Box::new(client));
    let error = tokio::time::timeout(Duration::from_secs(5), wire.volume(40, true))
        .await
        .unwrap()
        .unwrap_err();
    assert!(mixer_unavailable(&error));
    drop(wire);
    task.await.unwrap();
}

// Exercise the complete load sequence without touching a real MPD or audio
// device. The server checks which volume gets applied and when it is applied.
async fn check_load_volume(
    previous_status: Option<&str>,
    volume_before_play: Option<u8>,
    deferred: bool,
    position_ms: u64,
) {
    let (client, server) = UnixStream::pair().unwrap();
    let observed_volume = volume_before_play.unwrap_or(73);
    let mut commands = vec![("ping".to_owned(), "OK\n".to_owned())];
    if let Some(status) = previous_status {
        commands.push(("status".into(), format!("{status}OK\n")));
    }
    commands.extend([
        ("partition \"rspc\"".into(), "OK\n".into()),
        (
            "outputs".into(),
            "outputid: 0\noutputname: fixture\nplugin: pulse\noutputenabled: 1\nOK\n".into(),
        ),
        ("enableoutput 0".into(), "OK\n".into()),
    ]);
    for command in [
        "stop",
        "clear",
        "repeat 0",
        "random 0",
        "single 1",
        "consume 0",
        "addid \"next.flac\"",
    ] {
        commands.push((command.into(), "OK\n".into()));
    }
    if let Some(volume) = volume_before_play {
        commands.push((
            format!("setvol {volume}"),
            if deferred {
                "ACK [5@0] {setvol} All outputs are disabled\n"
            } else {
                "OK\n"
            }
            .into(),
        ));
    }
    commands.push((
        if position_ms == 0 {
            "play 0".into()
        } else {
            format!("seek 0 {:.3}", position_ms as f64 / 1000.0)
        },
        "OK\n".into(),
    ));
    if deferred {
        commands.push((
            format!("setvol {}", volume_before_play.unwrap()),
            "OK\n".into(),
        ));
    }
    commands.extend([
        ("ping".into(), "OK\n".into()),
        (
            "status".into(),
            format!("volume: {observed_volume}\nstate: play\nelapsed: 1.000\nOK\n"),
        ),
        (
            "currentsong".into(),
            "file: next.flac\nTitle: Next\nOK\n".into(),
        ),
    ]);
    let task = tokio::spawn(async move {
        let mut server = BufStream::new(server);
        for (expected, reply) in commands {
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            assert_eq!(command.trim_end(), expected);
            server.write_all(reply.as_bytes()).await.unwrap();
            server.flush().await.unwrap();
        }
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (done, _) = watch::channel(false);
    let directory = tempfile::tempdir().unwrap();
    let player = Player {
        settings: Settings::default(),
        state: Mutex::new(PlayerState {
            wire: Wire::new(Box::new(client)),
            lease: Some(Lease {
                name: "fixture".into(),
                origin: "default".into(),
                enabled: true,
                volume: None,
            }),
            current: previous_status.map(|_| "mpd:previous.flac".into()),
            observed: false,
            ended: false,
        }),
        generation: AtomicU64::new(1),
        closed: AtomicBool::new(false),
        tx,
        done,
        lease_path: directory.path().join("lease.json"),
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        player.load("mpd:next.flac", 2, 40, position_ms),
    )
    .await
    .unwrap()
    .unwrap();
    task.await.unwrap();
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok()).any(|update| update.epoch == 2
            && matches!(update.event, AudioEvent::Volume(volume) if volume == observed_volume))
    );
}

#[tokio::test]
async fn first_local_load_defers_volume_until_hardware_mixer_opens() {
    check_load_volume(None, Some(40), true, 0).await;
}

#[tokio::test]
async fn local_track_changes_preserve_desktop_volume_and_resume_offset() {
    check_load_volume(Some("volume: 73\nstate: pause\n"), Some(73), false, 1234).await;
}

#[tokio::test]
async fn natural_track_end_does_not_restore_stale_engine_volume() {
    check_load_volume(Some("volume: -1\nstate: stop\n"), None, false, 0).await;
}

#[test]
fn local_genres_survive_parsing_filtering_and_protocol_roundtrips() {
    let tracks = tracks(b"file: jazz.flac\nTitle: Fixture\nGenre: Jazz\nGenre: Fusion\n");
    assert_eq!(tracks[0].genres, ["Jazz", "Fusion"]);
    let filters = crate::daemon::filter::parse("(Genre == 'Fusion')").unwrap();
    assert!(crate::daemon::filter::matches(&tracks[0], &filters, false).unwrap());
    let entry = crate::daemon::queue::Entry {
        id: 1,
        track: tracks[0].clone(),
    };
    let reply = crate::daemon::protocol::song(&entry, Some(0));
    assert!(reply.contains("Genre: Jazz\nGenre: Fusion\n"));
}
#[test]
fn quoting_roundtrips_paths_and_rejects_command_injection() {
    let name = "音楽/A \"song\"\\mix.flac";
    let args = super::super::protocol::words(&format!("addid {}", quote(name).unwrap())).unwrap();
    assert_eq!(args, ["addid", name]);
    for invalid in ["file\nclear", "file\rstop", "file\0"] {
        assert!(quote(invalid).is_err());
    }
}
#[test]
fn mpd_metadata_keeps_multiple_artists_and_namespaces_file_uris() {
    let songs = tracks(b"directory: Album\nfile: Album/song.flac\nArtist: First\nArtist: Second\nTitle: Local Song\nAlbum: Album\nTrack: 2/10\nDate: 2020\nTime: 120\nduration: 120.250\nfile: https://example.test/radio\nName: Station\n");
    assert_eq!(songs.len(), 2);
    assert_eq!(songs[0].uri.as_deref(), Some("mpd:Album/song.flac"));
    assert_eq!(songs[0].artists, ["First", "Second"]);
    assert_eq!(songs[0].duration_ms, 120250);
    assert_eq!(songs[0].track_number, 2);
    assert_eq!(songs[0].release_date.as_deref(), Some("2020"));
    assert!(songs[0].is_local);
    assert!(!songs[1].is_local);
    assert_eq!(songs[1].duration_ms, 0);
}
#[test]
fn settings_read_rmpc_ron_with_unrelated_layout_fields() {
    let text = r##"#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(address:"/tmp/mpd.sock", password:Some("fixture"), theme:Some("gray"),tabs:[(name:"Queue",pane:Split(direction:Horizontal,panes:[(size:"100%",pane:Pane(Queue))]))])"##;
    let settings: Settings = ron::from_str(text).unwrap();
    assert_eq!(settings.address, "/tmp/mpd.sock");
    assert_eq!(settings.password.as_deref(), Some("fixture"));
    let settings: Settings = ron::from_str("(theme:Some(\"gray\"))").unwrap();
    assert_eq!(settings.address, "127.0.0.1:6600");
}
#[tokio::test]
async fn binary_artwork_preserves_bytes_and_the_next_response() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        server
            .write_all(b"size: 3\nbinary: 3\n\x00\xff\n\nOK\nvolume: 40\nOK\n")
            .await
            .unwrap();
    });
    let mut wire = Wire::new(Box::new(client));
    assert_eq!(
        wire.response().await.unwrap(),
        b"size: 3\nbinary: 3\n\x00\xff\n\n"
    );
    assert_eq!(wire.response().await.unwrap(), b"volume: 40\n");
    task.await.unwrap();
}
#[tokio::test]
async fn oversized_binary_chunks_are_rejected_before_allocation() {
    let (client, mut server) = UnixStream::pair().unwrap();
    server.write_all(b"binary: 1073741824\n").await.unwrap();
    let mut wire = Wire::new(Box::new(client));
    assert!(
        wire.response()
            .await
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
}
