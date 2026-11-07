use super::*;
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
    let mut wire = Wire {
        stream: BufStream::new(Box::new(client)),
    };
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
    let mut wire = Wire {
        stream: BufStream::new(Box::new(client)),
    };
    assert!(
        wire.response()
            .await
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
}
