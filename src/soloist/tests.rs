use super::*;

#[test]
fn package_player_precedes_legacy_install_but_not_an_explicit_override() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let bin = temp.path().join("package bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(state.join("soloist-package/bin")).unwrap();
    let legacy = state.join("soloist-package/bin/soloist");
    let packaged = bin.join("soloist");
    std::fs::write(&legacy, "legacy").unwrap();
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&packaged, "packaged").unwrap();
    let search = Some(std::env::join_paths([&bin]).unwrap());
    assert_eq!(find_binary(None, search.clone(), &state), legacy);
    std::fs::set_permissions(&packaged, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(find_binary(None, search.clone(), &state), packaged);
    let explicit = temp.path().join("missing-override");
    assert_eq!(
        find_binary(Some(explicit.clone().into_os_string()), search, &state),
        explicit
    );
    assert_eq!(find_binary(None, None, &state), legacy);
}

async fn authenticated(soloist: &Soloist) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !soloist.logged_in() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn saved_session_restoration_waits_past_the_initial_logged_out_event() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, done) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"auth_state","logged_in":false})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        socket
            .send(Message::Text(
                json!({"type":"auth_state","logged_in":true,"is_active":false})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        done.await.unwrap();
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let soloist = Soloist::from_socket(1, socket, None, None, tx);
    assert!(!soloist.logged_in());
    soloist.wait_for_restored_login().await;
    assert!(soloist.logged_in());
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn seek_after_play_waits_for_the_selected_track_to_be_ready() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, mut ready) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"auth_state","logged_in":true,"is_active":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let mut track_ready = false;
        loop {
            tokio::select! {
                _ = &mut ready, if !track_ready => {
                    track_ready = true;
                    socket.send(Message::Text(json!({"type":"playback_state","status":"playing","item":{"uri":"spotify:track:test"}}).to_string().into())).await.unwrap();
                }
                message = socket.next() => {
                    let Some(Ok(Message::Text(text))) = message else { break };
                    let value: Value = serde_json::from_str(&text).unwrap();
                    if value["command"] == "seek" {
                        assert!(track_ready, "seek was dispatched before the track was ready");
                        socket.send(Message::Text(json!({"type":"command_result","command":"seek"}).to_string().into())).await.unwrap();
                        break;
                    } else if value["command"] != "get_state" {
                        socket.send(Message::Text(json!({"type":"command_result","command":value["command"]}).to_string().into())).await.unwrap();
                    }
                }
            }
        }
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let soloist = Arc::new(Soloist::from_socket(1, socket, None, None, tx));
    authenticated(&soloist).await;
    soloist.reserve("spotify:track:test", 10_000).unwrap();
    soloist.load("spotify:track:test").await.unwrap();
    let player = Arc::clone(&soloist);
    let seek = tokio::spawn(async move { player.seek(1000).await });
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !seek.is_finished(),
        "pending seek must wait without blocking the socket pump"
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), seek)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn stopping_an_inactive_device_does_not_send_playback_controls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"auth_state", "logged_in":true, "is_active":false})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        while let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(100), socket.next()).await
        {
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                value["command"], "get_state",
                "inactive devices must not receive controls"
            );
        }
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let soloist = Soloist::from_socket(1, socket, None, None, tx);
    authenticated(&soloist).await;
    soloist.stop().await;
    server.await.unwrap();
}

#[tokio::test]
async fn a_late_acknowledgement_cannot_complete_the_next_command() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = format!("ws://{address}");
    let (reconnected, connected) = oneshot::channel();
    let (next_sent, next_received) = oneshot::channel();
    let (release, acknowledge) = oneshot::channel();
    let server = tokio::spawn(async move {
        let auth = || {
            Message::Text(
                json!({"type":"auth_state", "logged_in":true, "is_active":true})
                    .to_string()
                    .into(),
            )
        };
        let (stream, _) = listener.accept().await.unwrap();
        let mut old = tokio_tungstenite::accept_async(stream).await.unwrap();
        old.send(auth()).await.unwrap();
        while let Some(Ok(Message::Text(text))) = old.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["command"] == "seek" {
                break;
            }
        }
        // Leave the first command unacknowledged until the client reconnects.
        let (stream, _) = listener.accept().await.unwrap();
        let mut fresh = tokio_tungstenite::accept_async(stream).await.unwrap();
        fresh.send(auth()).await.unwrap();
        reconnected.send(()).unwrap();
        while let Some(Ok(Message::Text(text))) = fresh.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["command"] == "seek" {
                let result = || {
                    Message::Text(
                        json!({"type":"command_result", "command":"seek"})
                            .to_string()
                            .into(),
                    )
                };
                let _ = old.send(result()).await;
                next_sent.send(()).unwrap();
                acknowledge.await.unwrap();
                fresh.send(result()).await.unwrap();
                return;
            }
        }
        panic!("second seek was not dispatched");
    });
    let (socket, _) = tokio_tungstenite::connect_async(&endpoint).await.unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let soloist = Arc::new(Soloist::from_socket(1, socket, Some(endpoint), None, tx));
    authenticated(&soloist).await;
    assert!(soloist.seek(100).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), connected)
        .await
        .unwrap()
        .unwrap();
    let player = Arc::clone(&soloist);
    let mut second = tokio::spawn(async move { player.seek(200).await });
    tokio::time::timeout(Duration::from_secs(1), next_received)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    second.await.unwrap().unwrap();
    server.await.unwrap();
}
fn playing() -> State {
    let mut s = State {
        active: true,
        ..State::default()
    };
    s.reserve("spotify:track:a", 7);
    let e=s.event(&json!({"type":"playback_state","status":"playing","item":{"uri":"spotify:track:a","decorations":{"playback":{"duration_ms":1000}}},"position":{"position_ms":0,"speed":1.0}}));
    assert!(e.iter().any(|e| matches!(
        e,
        AudioEvent::State {
            request: 7,
            playing: true,
            ..
        }
    )));
    s
}
#[test]
fn old_position_anchors_are_interpolated_on_reconnect() {
    let mut s = playing();
    s.duration = 10000;
    let at = chrono::Utc::now().timestamp_millis() as u64 - 4000;
    s.event(&json!({"type":"position_sync","position":{"position_ms":1000,"timestamp_ms":at,"speed":1.0}}));
    assert!((4900..5500).contains(&s.position()));
}
#[test]
fn natural_end_is_emitted_once() {
    let mut s = playing();
    s.position = 1000;
    s.at = None;
    let e = s.event(&json!({"type":"playback_changed","status":"idle"}));
    assert_eq!(
        e.iter()
            .filter(|e| matches!(e, AudioEvent::Ended { request: 7 }))
            .count(),
        1
    );
    assert!(
        s.event(&json!({"type":"playback_changed","status":"idle"}))
            .is_empty()
    );
}
#[test]
fn manual_pause_at_end_does_not_skip() {
    let mut s = playing();
    s.position = 999;
    s.at = None;
    s.manual_pause = true;
    let e = s.event(&json!({"type":"playback_changed","status":"paused"}));
    assert!(!e.iter().any(|e| matches!(e, AudioEvent::Ended { .. })));
}
#[test]
fn stale_track_during_load_is_not_the_new_track() {
    let mut s = playing();
    s.reserve("spotify:track:b", 8);
    let e = s.event(
        &json!({"type":"playback_state","status":"playing","item":{"uri":"spotify:track:a"}}),
    );
    assert!(e.is_empty());
    assert!(!s.confirmed);
}
#[test]
fn external_context_change_is_not_a_natural_end() {
    let mut s = playing();
    let e = s.event(&json!({"type":"track_changed","item":{"uri":"spotify:track:other"}}));
    assert!(e.iter().any(|e| matches!(e, AudioEvent::ExternalState { track, .. } if track.uri.as_deref() == Some("spotify:track:other"))));
    assert!(!e.iter().any(|e| matches!(e, AudioEvent::Ended { .. })));
    assert_eq!(s.request, 0);
}
#[test]
fn partial_track_updates_preserve_duration_for_natural_end_detection() {
    let mut state = playing();
    state.event(&json!({"type":"track_changed","item":{"uri":"spotify:track:a","decorations":{"identity":{"name":"Track"}}}}));
    assert_eq!(state.duration, 1000);
    state.position = 1000;
    state.at = None;
    let events = state.event(&json!({"type":"track_changed","item":{"uri":"spotify:track:next"}}));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AudioEvent::Ended { request: 7 }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { .. }))
    );
}
#[test]
fn setup_snapshots_and_empty_buffering_items_do_not_steal_queue_control() {
    let mut state = playing();
    state.reserve("spotify:track:a", 8);
    state.awaiting_play = true;
    assert!(state.event(&json!({"type":"playback_state","status":"playing","item":{"uri":"spotify:track:a"}})).is_empty());
    assert!(
        state
            .event(&json!({"type":"track_changed","item":{"uri":"spotify:track:old-context"}}))
            .is_empty()
    );
    assert_eq!(state.request, 8);
    assert!(!state.confirmed);
    state.awaiting_play = false;
    state.event(
        &json!({"type":"playback_state","status":"playing","item":{"uri":"spotify:track:a"}}),
    );
    let events =
        state.event(&json!({"type":"playback_state","status":"buffering","item":{"uri":""}}));
    assert_eq!(state.request, 8);
    assert!(state.confirmed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { .. }))
    );
}
fn external_snapshot() -> Value {
    json!({"type":"playback_state", "status":"playing", "is_active":true, "volume":40,
        "item":{"uri":"spotify:track:phone", "entity_type":"track", "decorations":{
            "identity":{"name":"Phone song"},
            "creators":[{"entity":{"uri":"spotify:artist:artist1", "decorations":{"identity":{"name":"Artist"}}}}],
            "parent":{"entity":{"uri":"spotify:album:album1", "decorations":{"identity":{"name":"Album"}}}},
            "visual_identity":{"cover":[{"size":"large","url":"https://example.test/cover"}]},
            "playback":{"duration_ms":20000, "content_ratings":["explicit"]}}},
        "position":{"position_ms":5000,"speed":1.0}})
}
#[test]
fn restored_and_phone_playback_emit_metadata_without_a_local_request() {
    let mut state = State::default();
    let events = state.event(&external_snapshot());
    assert!(events.iter().any(|e| matches!(e, AudioEvent::Volume(40))));
    let track = events
        .iter()
        .find_map(|e| match e {
            AudioEvent::ExternalState {
                track,
                position,
                playing: true,
                stopped: false,
            } => {
                assert!((5000..5100).contains(position));
                Some(track)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(track.name, "Phone song");
    assert_eq!(track.artists, ["Artist"]);
    assert_eq!(track.artist_refs[0].id.as_deref(), Some("artist1"));
    assert_eq!(track.album, "Album");
    assert_eq!(track.album_id.as_deref(), Some("album1"));
    assert_eq!(track.duration_ms, 20000);
    assert!(track.explicit);
    assert_eq!(
        track.image_url.as_deref(),
        Some("https://example.test/cover")
    );
}
#[test]
fn phone_pause_seek_and_track_change_keep_following_remote_playback() {
    let mut state = State::default();
    state.event(&external_snapshot());
    let paused = state.event(&json!({"type":"playback_changed","status":"paused"}));
    assert!(paused.iter().any(|e| matches!(
        e,
        AudioEvent::ExternalState {
            playing: false,
            stopped: false,
            ..
        }
    )));
    let seek =
        state.event(&json!({"type":"position_sync","position":{"position_ms":10000,"speed":0}}));
    assert!(seek.iter().any(|e| matches!(
        e,
        AudioEvent::Position {
            request: 0,
            position: 10000
        }
    )));
    let next = state.event(&json!({"type":"track_changed","item":{"uri":"spotify:track:next","decorations":{"identity":{"name":"Next"},"playback":{"duration_ms":10000}}}}));
    assert!(
        next.iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { position: 0, .. }))
    );
    assert!(!next.iter().any(|e| matches!(e, AudioEvent::Ended { .. })));
}
#[test]
fn device_reactivation_reestablishes_the_same_track_without_a_new_command() {
    for kind in ["auth_state", "device_changed", "playback_state"] {
        for status in ["playing", "paused"] {
            let mut state = playing();
            let events = state.event(&json!({"type":kind, "logged_in":true, "is_active":false}));
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AudioEvent::Inactive))
            );
            state.event(&json!({"type":"device_changed", "is_active":true}));
            let events = state.event(&json!({
                "type":"playback_state", "is_active":true, "status":status,
                "item":{"uri":"spotify:track:a", "decorations":{"playback":{"duration_ms":1000}}},
                "position":{"position_ms":200, "speed":0.0}
            }));
            assert!(events.iter().any(|event| matches!(event,
                AudioEvent::ExternalState { track, playing, .. }
                    if track.uri.as_deref() == Some("spotify:track:a") && *playing == (status == "playing")
            )), "{kind} -> {status}: {events:?}");
            assert_eq!(state.request, 0);
        }
    }
}
#[test]
fn inactive_setup_snapshots_preserve_the_pending_play_request() {
    for kind in ["auth_state", "device_changed", "playback_state"] {
        let mut state = playing();
        state.reserve("spotify:track:b", 8);
        state.awaiting_play = true;
        state.event(&json!({"type":kind, "logged_in":true, "is_active":false}));
        assert_eq!(state.request, 8);
        assert_eq!(state.requested, "spotify:track:b");
        assert!(state.awaiting_play);
    }
}
#[test]
fn inactive_snapshots_do_not_undo_an_explicit_stop() {
    let mut state = playing();
    state.ended = true;
    state.event(&json!({"type":"playback_changed", "status":"paused"}));
    state.event(&json!({"type":"device_changed", "is_active":false}));
    state.event(&json!({"type":"device_changed", "is_active":true}));
    let events = state.event(&json!({
        "type":"playback_state", "is_active":true, "status":"paused",
        "item":{"uri":"spotify:track:a"}
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AudioEvent::ExternalState { .. }))
    );
    assert!(state.ended);
}
#[test]
fn inactive_device_does_not_report_a_different_devices_music_as_local() {
    let mut state = State::default();
    let mut snapshot = external_snapshot();
    snapshot["is_active"] = json!(false);
    let events = state.event(&snapshot);
    assert!(events.iter().any(|e| matches!(e, AudioEvent::Inactive)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { .. }))
    );
}
#[test]
fn local_stop_is_preserved_until_spotify_resumes_playback() {
    let mut state = State::default();
    state.event(&external_snapshot());
    state.ended = true;
    let events = state.event(&json!({"type":"playback_changed","status":"paused"}));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { .. }))
    );
    let events = state.event(&json!({"type":"playback_changed","status":"playing"}));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AudioEvent::ExternalState { playing: true, .. }))
    );
}
#[tokio::test]
async fn rejected_controls_restore_the_manual_pause_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for command in ["pause", "play"] {
            let Some(Ok(Message::Text(text))) = socket.next().await else {
                panic!("expected {command} command");
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(request["command"], command);
            socket
                .send(Message::Text(
                    json!({"type":"error", "message":"rejected control"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let soloist = Soloist::test_connection(socket, tx);
    soloist.state.lock().unwrap().logged_in = true;
    assert!(soloist.pause().await.is_err());
    assert!(!soloist.state.lock().unwrap().manual_pause);
    soloist.state.lock().unwrap().manual_pause = true;
    assert!(soloist.play().await.is_err());
    assert!(soloist.state.lock().unwrap().manual_pause);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn websocket_controls_wait_for_ack_and_receive_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"auth_state","logged_in":true,"is_active":false})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            let name = v["command"].as_str().unwrap();
            seen.push(name.to_owned());
            if name == "set_shuffle" {
                socket.send(Message::Text(json!({"type":"playback_state","status":"playing","is_active":true,"item":{"uri":"spotify:track:test"}}).to_string().into())).await.unwrap();
            }
            if name == "set_repeat_context" {
                socket.send(Message::Text(json!({"type":"track_changed","item":{"uri":"spotify:track:restored-context"}}).to_string().into())).await.unwrap();
            }
            socket
                .send(Message::Text(
                    json!({"type":"command_result","command":name})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if name == "play" {
                socket.send(Message::Text(json!({"type":"playback_state","is_active":true,"status":"playing","item":{"uri":v["uri"],"decorations":{"playback":{"duration_ms":10000}}},"position":{"position_ms":25,"speed":1.0}}).to_string().into())).await.unwrap();
            }
            if name == "pause" {
                break;
            }
        }
        seen
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let soloist = Soloist::from_socket(3, socket, None, None, tx);
    for _ in 0..100 {
        if soloist.logged_in() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(soloist.logged_in());
    let id = soloist.reserve("spotify:track:test", 10000).unwrap();
    soloist.load("spotify:track:test").await.unwrap();
    loop {
        let AudioUpdate { epoch, event, .. } =
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
        if matches!(event,AudioEvent::State{request,playing:true,..}if request==id) {
            assert_eq!(epoch, 3);
            break;
        }
    }
    soloist.seek(2000).await.unwrap();
    soloist.volume(42).await.unwrap();
    soloist.pause().await.unwrap();
    let commands = server.await.unwrap();
    assert!(
        commands.iter().position(|c| c == "activate").unwrap()
            < commands.iter().position(|c| c == "play").unwrap()
    );
    assert!(commands.iter().any(|c| c == "play"));
    assert!(commands.iter().any(|c| c == "seek"));
}
