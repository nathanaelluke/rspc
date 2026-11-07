use super::*;

#[test]
fn an_oversized_position_returns_an_error_instead_of_wrapping() {
    assert!(range(&usize::MAX.to_string(), 3).is_err());
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
