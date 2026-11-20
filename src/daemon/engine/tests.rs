use super::*;

async fn fixture() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(true, dir.path().join("q.json"), mpsc::unbounded_channel().0)
        .await
        .unwrap();
    (dir, engine)
}

#[tokio::test]
async fn deferred_seeks_leave_spotify_untouched() {
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"auth_state", "logged_in":true, "is_active":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let request = loop {
            let request = tokio::time::timeout(Duration::from_millis(200), socket.next()).await;
            if let Ok(Some(Ok(Message::Text(text)))) = &request
                && serde_json::from_str::<serde_json::Value>(text).unwrap()["command"]
                    == "get_state"
            {
                continue;
            }
            break request;
        };
        if request.is_ok() {
            socket
                .send(Message::Text(
                    json!({"type":"error", "message":"unexpected backend command"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        assert!(
            request.is_err(),
            "deferred selection sent a backend command: {request:?}"
        );
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let engine = Engine::new(true, dir.path().join("q.json"), tx.clone())
        .await
        .unwrap();
    engine.play(0).await.unwrap();
    let soloist = Arc::new(Soloist::test_connection(socket, tx));
    let deadline = Instant::now() + Duration::from_secs(1);
    while !soloist.logged_in() {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    *engine.soloist.write().await = Some(soloist);
    {
        let mut player = engine.player.lock().await;
        player.backend = None;
        player.playing = false;
        player.queue.entries[0].track.uri = Some("mpd:deferred.wav".into());
    }
    engine.seek(3.0, false).await.unwrap();
    let state = engine.playback().await;
    assert!(!state.playing && !state.stopped);
    assert_eq!(state.position_ms, 3000);
    server.await.unwrap();
}

#[tokio::test]
async fn queued_play_ids_survive_an_earlier_queue_reorder() {
    let (_dir, engine) = fixture().await;
    let control = engine.control.lock().await;
    let pending_engine = Arc::clone(&engine);
    let (started, ready) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        started.send(()).unwrap();
        super::super::protocol::execute(&pending_engine, &["playid".into(), "2".into()]).await
    });
    ready.await.unwrap();
    engine
        .player
        .lock()
        .await
        .queue
        .move_range(1, 2, 0)
        .unwrap();
    drop(control);
    pending.await.unwrap().unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, 2);
}

#[tokio::test]
async fn delayed_end_events_do_not_advance_a_new_play_request() {
    let (_dir, engine) = fixture().await;
    engine.play(0).await.unwrap();
    engine.player.lock().await.request = Some(7);
    let control = engine.control.lock().await;
    let pending_engine = Arc::clone(&engine);
    let (started, ready) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        started.send(()).unwrap();
        pending_engine
            .soloist_event(0, AudioEvent::Ended { request: 7 })
            .await
    });
    ready.await.unwrap();
    {
        let mut player = engine.player.lock().await;
        player.queue.current = Some(2);
        player.request = Some(8);
    }
    drop(control);
    pending.await.unwrap().unwrap();
    let player = engine.player.lock().await;
    assert_eq!(player.queue.current, Some(2));
    assert_eq!(player.request, Some(8));
    assert!(player.playing);
}

#[tokio::test]
async fn failed_queue_save_is_retried_without_another_queue_change() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("blocked");
    std::fs::write(&parent, b"not a directory").unwrap();
    let path = parent.join("queue.json");
    let (tx, _) = mpsc::unbounded_channel();
    let engine = Engine::new(true, path.clone(), tx).await.unwrap();
    engine.mark_dirty();
    assert!(engine.flush().await.is_err());
    std::fs::remove_file(&parent).unwrap();
    engine.flush().await.unwrap();
    assert_eq!(Queue::load(&path).unwrap().entries.len(), 3);
}

#[tokio::test]
async fn slow_playback_controls_leave_player_state_readable() {
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    for (command, accepted) in [
        ("pause", true),
        ("seek", true),
        ("pause", false),
        ("seek", false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let (release, acknowledge) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"type":"auth_state", "logged_in":true, "is_active":true})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                if request["command"] == command {
                    sent.send(()).unwrap();
                    acknowledge.await.unwrap();
                    socket
                        .send(Message::Text(
                            (if accepted {
                                json!({"type":"command_result", "command":command})
                            } else {
                                json!({"type":"error", "message":"control rejected"})
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                    return;
                }
            }
            panic!("control command was not sent");
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (tx, _updates) = mpsc::unbounded_channel();
        let engine = Engine::new(true, dir.path().join("q.json"), tx.clone())
            .await
            .unwrap();
        engine.play(0).await.unwrap();
        let soloist = Arc::new(Soloist::test_connection(socket, tx));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !soloist.logged_in() {
            assert!(Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        *engine.soloist.write().await = Some(soloist);
        let control = Arc::clone(&engine);
        let pending = tokio::spawn(async move {
            if command == "pause" {
                control.pause(Some(true)).await
            } else {
                control.seek(1.0, false).await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), received)
            .await
            .unwrap()
            .unwrap();
        let readable = tokio::time::timeout(Duration::from_millis(100), engine.playback()).await;
        release.send(()).unwrap();
        let result = pending.await.unwrap();
        assert_eq!(result.is_ok(), accepted);
        let state = engine.playback().await;
        assert_eq!(state.playing, command != "pause" || !accepted);
        if command == "seek" {
            assert_eq!(state.position_ms >= 1000, accepted);
        }
        server.await.unwrap();
        assert!(
            readable.is_ok(),
            "{command} held player state while waiting for the backend"
        );
    }
}
#[tokio::test]
async fn failed_natural_handoff_stops_and_reports_the_error_without_skipping() {
    let (_dir, engine) = fixture().await;
    engine.player.lock().await.queue.entries[1].track.uri = Some("unavailable:source".into());
    engine.play(0).await.unwrap();
    let id = engine.player.lock().await.queue.current;
    assert!(engine.finished().await.is_err());
    let state = engine.playback().await;
    assert!(state.stopped);
    assert!(state.error.contains("unsupported playback URI"));
    assert_eq!(engine.player.lock().await.queue.current, id);
    assert_eq!(engine.player.lock().await.queue.entries.len(), 3);
}
#[tokio::test]
async fn source_and_request_guards_keep_late_events_out_of_the_shared_player() {
    let (_dir, engine) = fixture().await;
    let mut track = super::super::catalog::demo_tracks().remove(0);
    track.uri = Some("mpd:fixture.wav".into());
    track.duration_ms = 0;
    {
        let mut p = engine.player.lock().await;
        p.queue.entries[0].track = track.clone();
        p.queue.current = Some(p.queue.entries[0].id);
        p.backend = Some(Backend::Mpd);
        p.request = Some(9);
        p.stopped = false;
        p.volume = 40;
    }
    engine
        .audio_event(Backend::Spotify, 0, AudioEvent::Volume(100))
        .await
        .unwrap();
    engine
        .audio_event(Backend::Spotify, 0, AudioEvent::Inactive)
        .await
        .unwrap();
    engine
        .audio_event(Backend::Mpd, 8, AudioEvent::Error("old request".into()))
        .await
        .unwrap();
    engine
        .audio_event(
            Backend::Mpd,
            9,
            AudioEvent::State {
                request: 9,
                position: 7000,
                playing: true,
            },
        )
        .await
        .unwrap();
    let state = engine.playback().await;
    assert_eq!(state.volume, 40);
    assert!(
        state.playing && state.position_ms >= 7000,
        "unknown stream duration must not clamp progress to zero"
    );
    track.duration_ms = 20000;
    engine
        .audio_event(Backend::Mpd, 9, AudioEvent::Metadata { request: 9, track })
        .await
        .unwrap();
    assert_eq!(
        engine.playback().await.entry.unwrap().track.duration_ms,
        20000
    );
    engine.player.lock().await.backend = Some(Backend::Spotify);
    let current = engine.player.lock().await.queue.current;
    engine
        .audio_event(Backend::Mpd, 9, AudioEvent::Ended { request: 9 })
        .await
        .unwrap();
    assert_eq!(engine.player.lock().await.queue.current, current);
}
#[tokio::test]
async fn mixed_queue_repeat_single_and_consume_keep_stable_entry_ids() {
    let (_dir, engine) = fixture().await;
    {
        let mut p = engine.player.lock().await;
        p.queue.entries[1].track.uri = Some("mpd:local.wav".into());
        p.repeat = true;
        p.single = 1;
    }
    engine.play(1).await.unwrap();
    let local_id = engine.player.lock().await.queue.current;
    engine.finished().await.unwrap();
    assert_eq!(engine.player.lock().await.queue.current, local_id);
    assert_eq!(engine.player.lock().await.backend, Some(Backend::Mpd));
    {
        let mut p = engine.player.lock().await;
        p.repeat = false;
        p.single = 2;
        p.queue.consume = true;
    }
    engine.finished().await.unwrap();
    assert!(engine.playback().await.stopped);
    let p = engine.player.lock().await;
    assert_eq!(p.single, 0);
    assert_eq!(
        p.queue.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        [1, 3]
    );
}
fn external(track: crate::model::TrackInfo, playing: bool) -> AudioEvent {
    AudioEvent::ExternalState {
        track,
        position: 5000,
        playing,
        stopped: false,
    }
}
#[tokio::test]
async fn initial_remote_snapshot_updates_real_mpd_status_and_preserves_the_queue() {
    let (dir, engine) = fixture().await;
    engine.soloist_epoch.store(1, Ordering::Relaxed);
    engine.player.lock().await.error = "old pairing error".into();
    let original = engine.player.lock().await.queue.clone();
    let mut track = super::super::catalog::demo_tracks().remove(0);
    track.uri = Some("spotify:track:phone".into());
    track.name = "Phone song".into();
    engine
        .soloist_event(1, AudioEvent::Volume(40))
        .await
        .unwrap();
    engine
        .soloist_event(1, external(track.clone(), true))
        .await
        .unwrap();
    let status = String::from_utf8(
        super::super::protocol::execute(&engine, &["status".into()])
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(status.contains("state: play\n"));
    assert!(status.contains("volume: 40\n"));
    assert!(!status.contains("error:"));
    let song = String::from_utf8(
        super::super::protocol::execute(&engine, &["currentsong".into()])
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(song.contains("Title: Phone song\n"));
    let state = engine.playback().await;
    assert!(state.playing && !state.stopped);
    assert!((5000..5500).contains(&state.position_ms));
    {
        let p = engine.player.lock().await;
        assert_eq!(p.queue.entries.len(), original.entries.len() + 1);
        for (entry, old) in p.queue.entries.iter().zip(&original.entries) {
            assert_eq!(entry.id, old.id);
            assert_eq!(entry.track, old.track);
        }
    }
    let id = state.entry.unwrap().id;
    let version = engine.player.lock().await.queue.version;
    engine
        .soloist_event(1, external(track, false))
        .await
        .unwrap();
    assert!(!engine.playback().await.playing);
    assert_eq!(engine.playback().await.entry.unwrap().id, id);
    assert_eq!(engine.player.lock().await.queue.version, version);
    engine.flush().await.unwrap();
    assert_eq!(
        Queue::load(&dir.path().join("q.json")).unwrap().current,
        Some(id)
    );
}
#[tokio::test]
async fn remote_sync_keeps_current_duplicate_id_and_local_play_takes_over() {
    let (_dir, engine) = fixture().await;
    let track = super::super::catalog::demo_tracks().remove(0);
    let id = engine
        .player
        .lock()
        .await
        .queue
        .add(vec![track.clone()], 3)
        .unwrap()[0];
    engine.play(3).await.unwrap();
    engine
        .soloist_event(0, external(track, true))
        .await
        .unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, id);
    assert_eq!(engine.player.lock().await.queue.entries.len(), 4);
    assert!(engine.player.lock().await.external);
    engine.play(1).await.unwrap();
    assert!(!engine.player.lock().await.external);
    assert_eq!(engine.player.lock().await.queue.cursor(), Some(1));
}
#[tokio::test]
async fn stale_player_and_pending_load_snapshots_cannot_replace_current_selection() {
    let (_dir, engine) = fixture().await;
    engine.soloist_epoch.store(2, Ordering::Relaxed);
    let track = super::super::catalog::demo_tracks().remove(0);
    engine
        .soloist_event(1, external(track.clone(), true))
        .await
        .unwrap();
    assert!(engine.playback().await.stopped);
    assert_eq!(engine.player.lock().await.queue.current, None);
    engine.player.lock().await.loading = Some(Instant::now());
    engine
        .soloist_event(2, external(track.clone(), true))
        .await
        .unwrap();
    engine.soloist_event(2, AudioEvent::Inactive).await.unwrap();
    assert!(engine.player.lock().await.loading.is_some());
    assert_eq!(engine.player.lock().await.queue.current, None);
    engine.player.lock().await.loading = None;
    engine
        .soloist_event(2, external(track, true))
        .await
        .unwrap();
    engine.soloist_event(1, AudioEvent::Inactive).await.unwrap();
    assert!(engine.playback().await.playing);
    engine.soloist_event(2, AudioEvent::Inactive).await.unwrap();
    assert!(engine.playback().await.stopped);
}
#[tokio::test]
async fn playback_failure_stops_without_skipping_or_rewriting_queue() {
    let (_dir, engine) = fixture().await;
    engine.play(0).await.unwrap();
    let before = engine.player.lock().await.queue.clone();
    engine
        .soloist_event(0, AudioEvent::Error("refused audio".into()))
        .await
        .unwrap();
    let after = engine.player.lock().await;
    assert!(after.stopped);
    assert_eq!(after.queue.current, before.current);
    assert_eq!(after.queue.entries.len(), before.entries.len());
    assert!(after.error.contains("refused audio"));
}
#[tokio::test]
async fn unpaired_play_keeps_queue_and_publishes_login_instructions() {
    use futures::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), socket.next())
                .await
                .is_err(),
            "unpaired player must not receive playback commands"
        );
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.json");
    let mut queue = Queue::default();
    queue.add(super::super::catalog::demo_tracks(), 0).unwrap();
    queue.save(&path).unwrap();
    let (tx, _) = mpsc::unbounded_channel();
    let engine = Engine::new(false, path, tx.clone()).await.unwrap();
    *engine.soloist.write().await = Some(Arc::new(Soloist::test_connection(socket, tx)));
    let error = engine.play(0).await.unwrap_err();
    assert!(error.to_string().contains("rspc login"));
    let state = engine.playback().await;
    assert!(state.stopped);
    assert!(state.error.contains("rspc login"));
    assert_eq!(engine.player.lock().await.queue.entries.len(), 3);
    assert_eq!(engine.player.lock().await.queue.current, None);
    server.await.unwrap();
}

async fn shuffle_option(engine: &Engine, name: &str, value: &str) {
    super::super::protocol::execute(engine, &[name.into(), value.into()])
        .await
        .unwrap();
}

#[tokio::test]
async fn advancing_notifies_playlist_listeners_only_when_a_track_is_consumed() {
    let (_dir, engine) = fixture().await;
    engine.play_id(1).await.unwrap();
    let mut events = engine.events.subscribe();
    let initial = events.borrow_and_update().clone();

    engine.next(false).await.unwrap();
    let advanced = events.borrow_and_update().clone();
    assert_ne!(initial.get("player"), advanced.get("player"));
    assert_eq!(initial.get("playlist"), advanced.get("playlist"));
    assert_eq!(initial.get("options"), advanced.get("options"));

    shuffle_option(&engine, "consume", "oneshot").await;
    let consuming = events.borrow_and_update().clone();
    engine.next(false).await.unwrap();
    let consumed = events.borrow_and_update().clone();
    assert_ne!(consuming.get("playlist"), consumed.get("playlist"));
    assert_ne!(consuming.get("options"), consumed.get("options"));
    assert_eq!(engine.player.lock().await.queue.entries.len(), 2);
}

#[tokio::test]
async fn status_predicts_the_next_shuffled_song_from_the_last_queue_position() {
    let (_dir, engine) = fixture().await;
    shuffle_option(&engine, "random", "1").await;
    engine.play_id(3).await.unwrap();
    let status = super::super::protocol::execute(&engine, &["status".into()])
        .await
        .unwrap();
    let next = String::from_utf8(status)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("nextsongid: "))
        .expect("an unplayed track remains")
        .parse::<u32>()
        .unwrap();
    engine.finished().await.unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, next);
}

async fn shuffle_pending(engine: &Engine) -> Vec<u32> {
    engine.player.lock().await.pending_ids.clone()
}

fn shuffle_ids(ids: &[u32]) -> std::collections::HashSet<u32> {
    ids.iter().copied().collect()
}

#[tokio::test]
async fn shuffle_finishes_every_entry_before_stopping() {
    for natural in [false, true] {
        let (_dir, engine) = fixture().await;
        shuffle_option(&engine, "random", "1").await;
        engine.play_id(1).await.unwrap();
        engine.player.lock().await.pending_ids = vec![3, 2];
        engine.next(false).await.unwrap();
        assert_eq!(engine.playback().await.entry.unwrap().id, 3);
        for _ in 0..3 {
            engine.sync_pending().await;
            assert_eq!(shuffle_pending(&engine).await, [2]);
        }
        if natural {
            engine.finished().await.unwrap();
        } else {
            engine.next(false).await.unwrap();
        }
        assert_eq!(engine.playback().await.entry.unwrap().id, 2);
        assert!(shuffle_pending(&engine).await.is_empty());
        if natural {
            engine.finished().await.unwrap();
        } else {
            engine.next(false).await.unwrap();
        }
        assert!(engine.playback().await.stopped);
        assert_eq!(engine.player.lock().await.queue.entries.len(), 3);
    }
}

#[tokio::test]
async fn shuffle_repeat_starts_a_full_cycle_after_exhaustion() {
    let (_dir, engine) = fixture().await;
    shuffle_option(&engine, "random", "1").await;
    shuffle_option(&engine, "repeat", "1").await;
    engine.play_id(1).await.unwrap();
    engine.player.lock().await.pending_ids = vec![3, 2];
    engine.finished().await.unwrap();
    assert_eq!(shuffle_pending(&engine).await, [2]);
    engine.finished().await.unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, 2);
    let preview = shuffle_pending(&engine).await;
    assert_eq!(preview.len(), 3);
    assert_eq!(shuffle_ids(&preview), shuffle_ids(&[1, 2, 3]));
    engine.sync_pending().await;
    assert_eq!(shuffle_pending(&engine).await, preview);
    for expected in preview {
        engine.finished().await.unwrap();
        assert_eq!(engine.playback().await.entry.unwrap().id, expected);
    }
    assert_eq!(shuffle_pending(&engine).await.len(), 3);
    shuffle_option(&engine, "repeat", "0").await;
    assert!(shuffle_pending(&engine).await.is_empty());
    engine.finished().await.unwrap();
    assert!(engine.playback().await.stopped);
}

#[tokio::test]
async fn shuffle_queue_edits_preserve_order_and_add_new_ids() {
    let (_dir, engine) = fixture().await;
    shuffle_option(&engine, "random", "1").await;
    engine.play_id(1).await.unwrap();
    engine.play_id(3).await.unwrap();
    let track = engine.playback().await.entry.unwrap().track;
    let added = engine
        .edit_queue(|queue| queue.add(vec![track.clone(), track], 0))
        .await
        .unwrap();
    let pending = shuffle_pending(&engine).await;
    assert_eq!(pending[0], 2);
    assert_eq!(shuffle_ids(&pending[1..]), shuffle_ids(&added));
    engine
        .edit_queue(|queue| queue.move_range(0, 2, 3))
        .await
        .unwrap();
    assert_eq!(shuffle_pending(&engine).await, pending);
    engine
        .edit_queue(|queue| {
            for id in [1, 2] {
                let index = queue.index(id)?;
                queue.delete(index, index + 1)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(shuffle_pending(&engine).await, pending[1..]);
    for expected in &pending[1..] {
        engine.queue_changed().await;
        engine.finished().await.unwrap();
        assert_eq!(engine.playback().await.entry.unwrap().id, *expected);
    }
    engine.finished().await.unwrap();
    assert!(engine.playback().await.stopped);
}

#[tokio::test]
async fn shuffle_deleting_current_preserves_unplayed_ids_when_paused_or_playing() {
    for paused in [false, true] {
        let (_dir, engine) = fixture().await;
        let track = engine.player.lock().await.queue.entries[0].track.clone();
        engine
            .edit_queue(|queue| queue.add(vec![track], 3))
            .await
            .unwrap();
        shuffle_option(&engine, "random", "1").await;
        engine.play_id(1).await.unwrap();
        engine.play_id(3).await.unwrap();
        if paused {
            engine.pause(Some(true)).await.unwrap();
        }
        engine.edit_queue(|queue| queue.delete(2, 3)).await.unwrap();
        assert_eq!(engine.playback().await.entry.unwrap().id, 4);
        assert_eq!(engine.playback().await.playing, !paused);
        assert_eq!(shuffle_pending(&engine).await, [2]);
        if paused {
            engine.pause(Some(false)).await.unwrap();
            assert_eq!(shuffle_pending(&engine).await, [2]);
        }
        engine.finished().await.unwrap();
        assert_eq!(engine.playback().await.entry.unwrap().id, 2);
        engine.finished().await.unwrap();
        assert!(engine.playback().await.stopped);
    }
}

#[tokio::test]
async fn shuffle_explicit_selections_and_seeks_preserve_other_unplayed_ids() {
    let (_dir, engine) = fixture().await;
    shuffle_option(&engine, "random", "1").await;
    engine.play(2).await.unwrap();
    assert_eq!(
        shuffle_ids(&shuffle_pending(&engine).await),
        shuffle_ids(&[1, 2])
    );
    engine.play_id(1).await.unwrap();
    assert_eq!(shuffle_pending(&engine).await, [2]);
    engine.play_id(3).await.unwrap();
    assert_eq!(shuffle_pending(&engine).await, [2]);
    engine.seek_id(2, 12.0).await.unwrap();
    assert!(shuffle_pending(&engine).await.is_empty());
    engine.seek_position(1, 15.0).await.unwrap();
    engine.pause(Some(true)).await.unwrap();
    engine.pause(Some(false)).await.unwrap();
    assert!(shuffle_pending(&engine).await.is_empty());
    engine.next(false).await.unwrap();
    assert!(engine.playback().await.stopped);
}

#[tokio::test]
async fn shuffle_mode_changes_reset_only_when_random_is_disabled_or_playback_stops() {
    let (_dir, engine) = fixture().await;
    shuffle_option(&engine, "random", "1").await;
    engine.play_id(1).await.unwrap();
    engine.play_id(3).await.unwrap();
    for (name, value) in [
        ("repeat", "1"),
        ("consume", "1"),
        ("consume", "0"),
        ("random", "1"),
    ] {
        shuffle_option(&engine, name, value).await;
        assert_eq!(shuffle_pending(&engine).await, [2]);
    }
    shuffle_option(&engine, "single", "1").await;
    assert_eq!(shuffle_pending(&engine).await, [3]);
    engine.finished().await.unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, 3);
    shuffle_option(&engine, "single", "0").await;
    assert_eq!(shuffle_pending(&engine).await, [2]);
    shuffle_option(&engine, "repeat", "0").await;
    shuffle_option(&engine, "random", "0").await;
    assert!(shuffle_pending(&engine).await.is_empty());
    shuffle_option(&engine, "random", "1").await;
    assert_eq!(
        shuffle_ids(&shuffle_pending(&engine).await),
        shuffle_ids(&[1, 2])
    );
    engine.play_id(1).await.unwrap();
    shuffle_option(&engine, "random", "0").await;
    assert_eq!(shuffle_pending(&engine).await, [2, 3]);
    engine.next(false).await.unwrap();
    assert_eq!(engine.playback().await.entry.unwrap().id, 2);
    shuffle_option(&engine, "random", "1").await;
    engine.stop().await;
    assert!(shuffle_pending(&engine).await.is_empty());
    engine.pause(Some(false)).await.unwrap();
    assert_eq!(
        shuffle_ids(&shuffle_pending(&engine).await),
        shuffle_ids(&[1, 3])
    );
}

#[tokio::test]
async fn shuffle_consume_and_oneshot_do_not_reintroduce_finished_ids() {
    for repeat in [false, true] {
        for consume in ["1", "oneshot"] {
            let (_dir, engine) = fixture().await;
            shuffle_option(&engine, "random", "1").await;
            shuffle_option(&engine, "repeat", if repeat { "1" } else { "0" }).await;
            shuffle_option(&engine, "consume", consume).await;
            engine.play_id(1).await.unwrap();
            engine.player.lock().await.pending_ids = vec![3, 2];
            engine.next(false).await.unwrap();
            assert_eq!(shuffle_pending(&engine).await, [2]);
            {
                let p = engine.player.lock().await;
                assert!(p.queue.index(1).is_err());
                assert_eq!(p.queue.consume, consume == "1");
                assert!(!p.queue.consume_oneshot);
            }
            engine.finished().await.unwrap();
            assert_eq!(engine.playback().await.entry.unwrap().id, 2);
            if repeat && consume == "oneshot" {
                let preview = shuffle_pending(&engine).await;
                assert_eq!(preview.len(), 2);
                assert_eq!(shuffle_ids(&preview), shuffle_ids(&[2, 3]));
                for expected in preview {
                    engine.finished().await.unwrap();
                    assert_eq!(engine.playback().await.entry.unwrap().id, expected);
                }
            } else {
                assert!(shuffle_pending(&engine).await.is_empty());
                engine.finished().await.unwrap();
                assert!(engine.playback().await.stopped);
                assert_eq!(
                    engine.player.lock().await.queue.entries.len(),
                    if consume == "1" { 0 } else { 2 }
                );
            }
        }
    }
}
