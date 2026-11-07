use super::*;

#[test]
fn duplicate_names_preserve_every_item_and_accept_unicode_ids() {
    let mut names = BTreeMap::new();
    insert_named(&mut names, "Curator".into(), "abcdefgh一".into());
    insert_named(&mut names, "Curator".into(), "abcdefg二".into());
    insert_named(&mut names, "Curator".into(), "abcdefgh三".into());
    insert_named(&mut names, "Curator [abcdefg二]".into(), "fourth".into());
    assert_eq!(names.len(), 4);
    let ids: std::collections::HashSet<_> = names.values().map(String::as_str).collect();
    assert_eq!(
        ids,
        ["abcdefgh一", "abcdefg二", "abcdefgh三", "fourth"]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn completed_requests_do_not_leave_unbounded_cache_gates() {
    let catalog = Catalog::new(true);
    let active = catalog.request_gate("active").await;
    for index in 0..CACHE_CAPACITY * 2 {
        drop(catalog.request_gate(&index.to_string()).await);
    }
    assert!(Arc::ptr_eq(&active, &catalog.request_gate("active").await));
    assert!(catalog.gates.lock().await.len() <= CACHE_CAPACITY);
}

#[tokio::test]
async fn invalidation_clears_derived_metadata_and_rejects_queued_cache_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(true);
    catalog.disk_root = dir.path().join("catalog");
    let catalog = Arc::new(catalog);
    let track = demo_tracks().remove(0);
    catalog.remember(vec![track.clone()]).await;
    catalog
        .albums
        .lock()
        .await
        .insert("old".into(), json!({"id":"old"}));
    let blocked = catalog.disk_writes.lock().await;
    let writer = Arc::clone(&catalog);
    let pending = tokio::spawn(async move {
        writer
            .persist_entry(
                "stale",
                0,
                Arc::new(Cached {
                    used: Default::default(),
                    fetched: now(),
                    value: json!({"old":true}),
                }),
            )
            .await
    });
    let invalidated = Arc::clone(&catalog);
    let invalidation = tokio::spawn(async move { invalidated.invalidate().await });
    while catalog.generation.load(Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    drop(blocked);
    pending.await.unwrap().unwrap();
    invalidation.await.unwrap();
    assert!(catalog.tracks.lock().await.is_empty());
    assert!(catalog.albums.lock().await.is_empty());
    assert!(catalog.artist_views.lock().await.is_empty());
    assert!(
        !catalog.disk("stale").exists(),
        "an old cache write survived invalidation"
    );
}
#[tokio::test]
async fn people_browsing_keeps_curators_separate_and_opens_public_playlist_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    catalog
        .playlist_settings
        .lock()
        .unwrap()
        .rules
        .exclude_playlists = vec!["Hidden".into()];
    cache_page(
        &catalog,
        "me/playlists",
        0,
        vec![json!({"id":"saved","name":"Workout","owner":{"id":"bob","display_name":"bob"}})],
        1,
    );
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"artist","name":"Saved Musician"})],
        None,
    );
    for (path, value) in [
        (
            "me/following/users",
            json!({"items":[{"uri":"spotify:user:alice","name":"Alice"}]}),
        ),
        (
            "users/alice/profile",
            json!({"name":"Alice","public_playlists":[{"uri":"spotify:playlist:public1","name":"Public Mix"},{"uri":"spotify:playlist:hidden1","name":"Hidden"}]}),
        ),
        (
            "users/bob/profile",
            json!({"name":"Bob","uri":"spotify:user:bob"}),
        ),
    ] {
        let entry = Cached {
            used: Default::default(),
            fetched: now(),
            value,
        };
        crate::paths::write_private_file_atomic(
            &catalog.disk(&catalog.cache_key(path, &[])),
            &serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
    }
    cache_page(
        &catalog,
        "playlists/public1/tracks",
        0,
        vec![json!({"item":playlist_track(0)})],
        1,
    );
    let catalog = Arc::new(catalog);
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    let (people, _) = catalog.browse_people("People").await.unwrap();
    assert_eq!(people.len(), 2);
    let alice = people.iter().find(|path| path.ends_with("/Alice")).unwrap();
    assert!(
        people
            .iter()
            .any(|path| path.contains("Curator of Workout"))
    );
    let (playlists, _) = catalog.browse_people(alice).await.unwrap();
    assert_eq!(playlists, ["People/playlist/public1/Public Mix"]);
    let bob = people.iter().find(|path| path.contains("/bob/")).unwrap();
    assert!(catalog.browse_people(bob).await.unwrap().0.is_empty());
    assert_eq!(
        catalog.browse_people(&playlists[0]).await.unwrap().1.len(),
        1
    );
    assert_eq!(
        catalog.spotify_tags("Artist", "").await.unwrap(),
        ["Saved Musician"]
    );
}
#[tokio::test]
async fn changing_exclusions_invalidates_playlist_caches_without_discarding_album_cache_keys() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.ron");
    std::fs::write(&config, "(spotify:(exclude_folders:[\"archive\"]),)").unwrap();
    std::fs::File::open(&config)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(5))
        .unwrap();
    let catalog = Catalog::new(false);
    catalog
        .playlist_settings
        .lock()
        .unwrap()
        .configure(Some(config.clone()))
        .unwrap();
    let before = catalog.cache_key("me/playlists", &[]);
    let albums = catalog.cache_key("me/albums", &[]);
    *catalog.playlist_fetched.lock().await = Some(std::time::Instant::now());
    std::fs::write(&config, "(spotify:(exclude_playlists:[\"Old Mix\"]),)").unwrap();
    std::fs::File::open(&config)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    assert!(catalog.reload_playlist_settings().await.unwrap());
    assert_ne!(catalog.cache_key("me/playlists", &[]), before);
    assert_eq!(catalog.cache_key("me/albums", &[]), albums);
    assert!(catalog.playlist_fetched.lock().await.is_none());
}
#[tokio::test]
async fn song_info_catalog_navigation_fetches_full_releases_without_changing_library_roots() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    cache_page(&catalog, "me/albums", 0, vec![], 0);
    cache_following(&catalog, None, vec![], None);
    let mut first = playlist_track(0);
    first["album"]["id"] = json!("fullalbum1");
    let mut second = playlist_track(1);
    second["album"]["id"] = json!("fullalbum1");
    let mut album = first["album"].clone();
    album["rspc_track_uris"] = json!([first["uri"], second["uri"]]);
    cache_page(
        &catalog,
        "artists/artist1/albums",
        0,
        vec![album.clone()],
        1,
    );
    let entry = Cached {
        used: Default::default(),
        fetched: now(),
        value: album,
    };
    crate::paths::write_private_file_atomic(
        &catalog.disk(&catalog.cache_key("albums/fullalbum1", &[])),
        &serde_json::to_vec(&entry).unwrap(),
    )
    .unwrap();
    let catalog = Arc::new(catalog);
    catalog
        .remember(vec![
            track_from_json(&first).unwrap(),
            track_from_json(&second).unwrap(),
        ])
        .await;
    catalog.albums.lock().await.insert(
        "fullalbum1".into(),
        json!({"id":"fullalbum1","rspc_partial":true,"rspc_track_uris":[first["uri"]]}),
    );
    let (_, tracks) = catalog
        .browse_catalog("rspc://catalog/album/fixture0")
        .await
        .unwrap();
    assert_eq!(
        tracks.len(),
        2,
        "opening song info's album must load more than the liked subset"
    );
    let (directories, tracks) = catalog
        .browse_catalog("rspc://catalog/artist/fixture0/Playlist%20Artist")
        .await
        .unwrap();
    assert!(tracks.is_empty());
    assert_eq!(directories.len(), 1);
    assert_eq!(
        catalog
            .browse_catalog(&directories[0])
            .await
            .unwrap()
            .1
            .len(),
        2
    );
    for tag in ["Album", "AlbumArtist", "Artist"] {
        assert!(
            catalog.spotify_tags(tag, "").await.unwrap().is_empty(),
            "{tag} must stay restricted to saved items"
        );
    }
}
#[tokio::test]
async fn library_browsers_distinguish_album_artists_and_guest_performers() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    let mut track = playlist_track(0);
    track["artists"] = json!([{"id":"guest","name":"Guest Performer"}]);
    track["album"]["artists"] = json!([{"id":"various","name":"Various Artists"}]);
    let metadata = track_from_json(&track).unwrap();
    let mut album = track["album"].clone();
    album["rspc_track_uris"] = json!([track["uri"], track["uri"]]);
    cache_page(&catalog, "artists/guest/albums", 0, vec![album.clone()], 1);
    cache_page(&catalog, "me/albums", 0, vec![json!({"album":album})], 1);
    cache_page(&catalog, "me/tracks", 0, vec![json!({"track":track})], 1);
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"guest","name":"Guest Performer"})],
        None,
    );
    let catalog = Arc::new(catalog);
    catalog.remember(vec![metadata]).await;
    // No uncached request can succeed: both the library and the full artist
    // catalog have been seeded separately.
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    assert_eq!(
        catalog.tags("AlbumArtist", "").await.unwrap(),
        ["Various Artists"]
    );
    assert_eq!(
        catalog.tags("Artist", "").await.unwrap(),
        ["Guest Performer"]
    );
    assert_eq!(
        catalog
            .tags("Album", "(Artist == 'Guest Performer')")
            .await
            .unwrap(),
        ["Playlist Album"]
    );
    let tracks = catalog
        .spotify_find("(AlbumArtist == 'Various Artists')", false)
        .await
        .unwrap();
    assert_eq!(
        tracks.len(),
        2,
        "album order and duplicate tracks must survive metadata reuse"
    );
    assert!(
        catalog
            .spotify_find("(AlbumArtist == 'Guest Performer')", false)
            .await
            .unwrap()
            .is_empty()
    );
    let wire = super::super::protocol::song(
        &super::super::queue::Entry {
            id: 1,
            track: tracks[0].clone(),
        },
        None,
    );
    assert!(wire.contains("Artist: Guest Performer\n"));
    assert!(wire.contains("AlbumArtist: Various Artists\n"));
}
#[tokio::test]
async fn album_and_album_artist_roots_only_use_saved_albums() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    let mut saved = playlist_track(0);
    saved["album"]["artists"] = json!([{"name":"Saved Album Artist"}]);
    let mut album = saved["album"].clone();
    album["rspc_track_uris"] = json!([saved["uri"]]);
    cache_page(&catalog, "me/albums", 0, vec![json!({"album":album})], 1);
    let mut liked = playlist_track(1);
    liked["album"] = json!({"id":"not-saved","name":"Liked-only Album","artists":[{"name":"Liked-only Artist"}]});
    let mut same_name = playlist_track(2);
    same_name["album"]["id"] = json!("unsaved-same-title");
    cache_page(
        &catalog,
        "me/tracks",
        0,
        vec![
            json!({"track":saved}),
            json!({"track":liked}),
            json!({"track":same_name}),
        ],
        3,
    );
    let catalog = Arc::new(catalog);
    catalog.liked().await.unwrap();
    assert_eq!(
        catalog.spotify_tags("AlbumArtist", "").await.unwrap(),
        ["Saved Album Artist"]
    );
    assert_eq!(
        catalog.spotify_tags("Album", "").await.unwrap(),
        ["Playlist Album"]
    );
    assert_eq!(
        catalog
            .spotify_find("(AlbumArtist == 'Saved Album Artist')", false)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        catalog
            .spotify_find("(Album == 'Liked-only Album')", false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        catalog
            .spotify_find("(Album == 'Playlist Album')", false)
            .await
            .unwrap()
            .len(),
        1,
        "an unsaved album with the same title must not leak into the saved album"
    );
    assert!(
        catalog
            .spotify_find("(AlbumArtist == 'Liked-only Artist')", false)
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn artists_use_full_catalog_even_with_liked_tracks_without_changing_saved_album_roots() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    cache_page(&catalog, "me/albums", 0, vec![], 0);
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"backyardigans","name":"The Backyardigans"})],
        None,
    );
    let mut track = playlist_track(0);
    track["artists"] = json!([{"id":"backyardigans","name":"The Backyardigans"}]);
    cache_page(
        &catalog,
        "me/tracks",
        0,
        vec![json!({"track":track.clone()})],
        1,
    );
    let mut unsaved = playlist_track(1);
    unsaved["artists"] = track["artists"].clone();
    let mut album = track["album"].clone();
    album["artists"] = track["artists"].clone();
    album["rspc_track_uris"] = json!([track["uri"], unsaved["uri"]]);
    cache_page(&catalog, "artists/backyardigans/albums", 0, vec![album], 1);
    let catalog = Arc::new(catalog);
    catalog
        .remember(vec![
            track_from_json(&track).unwrap(),
            track_from_json(&unsaved).unwrap(),
        ])
        .await;
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    let tracks = catalog
        .spotify_find("(Artist == 'The Backyardigans')", false)
        .await
        .unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(
        catalog
            .spotify_tags("Album", "(Artist == 'The Backyardigans')")
            .await
            .unwrap(),
        ["Playlist Album"]
    );
    for tag in ["Album", "AlbumArtist"] {
        assert!(catalog.spotify_tags(tag, "").await.unwrap().is_empty());
    }
    assert!(
        catalog
            .spotify_find("(AlbumArtist == 'The Backyardigans')", false)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        catalog
            .spotify_find("(Artist == 'Unfollowed')", false)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn prepared_artist_reads_share_work_and_invalidation_discards_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"artist1","name":"Playlist Artist"})],
        None,
    );
    let first_track = playlist_track(0);
    let mut album = first_track["album"].clone();
    album["rspc_track_uris"] = json!([first_track["uri"]]);
    cache_page(&catalog, "artists/artist1/albums", 0, vec![album], 1);
    let catalog = Arc::new(catalog);
    catalog
        .remember(vec![track_from_json(&first_track).unwrap()])
        .await;
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    let views =
        futures::future::join_all((0..12).map(|_| catalog.artist_tracks("Playlist Artist"))).await;
    let first = views[0].as_ref().unwrap();
    assert_eq!(first.len(), 1);
    assert!(
        views
            .iter()
            .all(|view| Arc::ptr_eq(first, view.as_ref().unwrap()))
    );
    catalog.cache.lock().await.clear();
    std::fs::remove_dir_all(&catalog.disk_root).unwrap();
    assert!(Arc::ptr_eq(
        first,
        &catalog.artist_tracks("Playlist Artist").await.unwrap()
    ));
    catalog.invalidate().await;
    assert!(catalog.artist_views.lock().await.is_empty());
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"artist1","name":"Playlist Artist"})],
        None,
    );
    let second_track = playlist_track(1);
    let mut album = second_track["album"].clone();
    album["rspc_track_uris"] = json!([second_track["uri"]]);
    cache_page(&catalog, "artists/artist1/albums", 0, vec![album], 1);
    catalog
        .remember(vec![track_from_json(&second_track).unwrap()])
        .await;
    let refreshed = catalog.artist_tracks("Playlist Artist").await.unwrap();
    assert!(!Arc::ptr_eq(first, &refreshed));
    assert_ne!(first[0].uri, refreshed[0].uri);
}

#[tokio::test]
async fn artists_only_lists_saved_artists_across_cursor_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"saved1","name":"First Saved Artist"})],
        Some("saved1"),
    );
    cache_following(
        &catalog,
        Some("saved1"),
        vec![json!({"id":"saved2","name":"Second Saved Artist"})],
        None,
    );
    let catalog = Arc::new(catalog);
    catalog
        .remember(vec![track_from_json(&playlist_track(0)).unwrap()])
        .await;
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    assert_eq!(
        catalog.spotify_tags("Artist", "").await.unwrap(),
        ["First Saved Artist", "Second Saved Artist"]
    );
}
fn cache_following(catalog: &Catalog, after: Option<&str>, items: Vec<Value>, next: Option<&str>) {
    let mut query = vec![("limit", "100".to_owned())];
    if let Some(after) = after {
        query.push(("after", after.to_owned()));
    }
    let entry = Cached {
        used: Default::default(),
        fetched: now(),
        value: json!({"artists":{"items":items,"next":next.map(|_|"internal://following"),"cursors":{"after":next}}}),
    };
    crate::paths::write_private_file_atomic(
        &catalog.disk(&format!("me/following|{query:?}")),
        &serde_json::to_vec(&entry).unwrap(),
    )
    .unwrap();
}
#[tokio::test]
async fn playlist_edits_preserve_duplicates_and_do_not_change_the_queue() {
    let catalog = Arc::new(Catalog::new(true));
    catalog.playlist_save("Copy", demo_tracks()).await.unwrap();
    catalog
        .playlist_add("Copy", "spotify:track:demo1", Some(1))
        .await
        .unwrap();
    catalog.playlist_remove("Copy", 0, 1).await.unwrap();
    catalog.playlist_move("Copy", 0, 1, 2).await.unwrap();
    let tracks = catalog.playlist_tracks("Copy").await.unwrap();
    assert_eq!(
        tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["Second Window", "After Hours", "Morning Light"]
    );
    assert_eq!(catalog.playlist_tracks("Demo Mix").await.unwrap().len(), 3);
    assert!(
        catalog
            .playlist_add("Copy", "mpd:file.flac", None)
            .await
            .is_err()
    );
    assert!(catalog.playlist_remove("Copy", 3, 4).await.is_err());
    assert!(catalog.playlist_save("Copy", vec![]).await.is_err());
    catalog.playlist_rename("Copy", "Renamed").await.unwrap();
    catalog.playlist_clear("Renamed").await.unwrap();
    assert!(catalog.playlist_tracks("Renamed").await.unwrap().is_empty());
    catalog.playlist_delete("Renamed").await.unwrap();
    assert!(catalog.playlist_tracks("Renamed").await.is_err());
}
pub(super) fn cache_page(
    catalog: &Catalog,
    path: &str,
    offset: usize,
    items: Vec<Value>,
    total: usize,
) {
    let query = [("limit", "100".to_owned()), ("offset", offset.to_string())];
    let key = catalog.cache_key(path, &query);
    let entry = Cached {
        used: Default::default(),
        fetched: now(),
        value: json!({"items":items,"total":total,"next":null}),
    };
    crate::paths::write_private_file_atomic(
        &catalog.disk(&key),
        &serde_json::to_vec(&entry).unwrap(),
    )
    .unwrap();
}
pub(super) fn playlist_track(index: usize) -> Value {
    json!({"uri":format!("spotify:track:fixture{index}"),"id":format!("fixture{index}"),
        "name":format!("Playlist song {index}"),"duration_ms":180000,"track_number":index+1,
        "artists":[{"id":"artist1","name":"Playlist Artist"}],
        "album":{"id":"album1","name":"Playlist Album"}})
}
#[tokio::test]
async fn cached_playlist_items_populate_the_mpd_browser_across_pages() {
    // Exercise the real, non-demo catalog reader. Every page is on disk,
    // so no account/session/network or user cache is involved.
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    cache_page(
        &catalog,
        "me/playlists",
        0,
        vec![json!({"id":"fixture-playlist","name":"Regression Mix"})],
        1,
    );
    cache_page(&catalog, "playlists/fixture-playlist/tracks", 0,
        (0..100).map(|i| json!({"added_at":"2026-01-01T00:00:00Z","item":playlist_track(i),"is_local":false})).collect(), 103);
    cache_page(
        &catalog,
        "playlists/fixture-playlist/tracks",
        100,
        vec![
            json!({"item":playlist_track(100)}),
            json!({"item":null}),
            json!({"item":playlist_track(0)}),
        ],
        103,
    );
    let catalog = Arc::new(catalog);
    let tracks = catalog.playlist_tracks("Regression Mix").await.unwrap();
    assert_eq!(tracks.len(), 102);
    assert_eq!(tracks[0].name, "Playlist song 0");
    assert_eq!(tracks[100].name, "Playlist song 100");
    assert_eq!(
        tracks[101].uri, tracks[0].uri,
        "playlist duplicates must be retained"
    );
    assert_eq!(tracks[0].artists, ["Playlist Artist"]);
    assert_eq!(tracks[0].album, "Playlist Album");
    let positions = catalog.playlist_positions("Regression Mix").await.unwrap();
    assert_eq!(
        positions.last().unwrap().0,
        102,
        "visible playlist rows must map back to original Spotify positions"
    );

    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let mut engine = super::super::engine::Engine::new(true, dir.path().join("queue.json"), tx)
        .await
        .unwrap();
    Arc::get_mut(&mut engine).unwrap().catalog = catalog;
    for args in [
        vec!["listplaylistinfo", "Regression Mix"],
        vec!["lsinfo", "Playlists/Regression Mix"],
    ] {
        let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
        let reply = String::from_utf8(
            super::super::protocol::execute(&engine, &args)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            reply.lines().filter(|l| l.starts_with("file: ")).count(),
            102
        );
        assert!(reply.contains("Title: Playlist song 100\n"));
    }
    let range = vec![
        "listplaylistinfo".into(),
        "Regression Mix".into(),
        "99:102".into(),
    ];
    let reply = String::from_utf8(
        super::super::protocol::execute(&engine, &range)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply.lines().filter(|l| l.starts_with("file: ")).count(), 3);
    assert!(reply.contains("Title: Playlist song 99\n"));
    assert!(reply.contains("Title: Playlist song 100\n"));
    super::super::protocol::execute(&engine, &["load".into(), "Regression Mix".into()])
        .await
        .unwrap();
    assert_eq!(engine.player.lock().await.queue.entries.len(), 105);
}
#[tokio::test]
async fn cached_legacy_track_envelopes_remain_supported() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().to_owned();
    cache_page(
        &catalog,
        "me/playlists",
        0,
        vec![json!({"id":"legacy","name":"Legacy Mix"})],
        1,
    );
    cache_page(
        &catalog,
        "playlists/legacy/tracks",
        0,
        vec![
            json!({"track":playlist_track(0)}),
            json!({"item":null,"track":playlist_track(1)}),
            json!({"item":null,"track":null}),
        ],
        3,
    );
    let tracks = Arc::new(catalog)
        .playlist_tracks("Legacy Mix")
        .await
        .unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].name, "Playlist song 0");
    assert_eq!(tracks[1].name, "Playlist song 1");
}
#[tokio::test]
async fn disk_cache_serves_concurrent_reads_without_a_spotify_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().to_owned();
    let catalog = Arc::new(catalog);
    let query = [("offset", "0".to_owned())];
    let key = format!("me/albums|{query:?}");
    let entry = Cached {
        used: Default::default(),
        fetched: now(),
        value: json!({"items":[{"album":{"name":"Cached album"}}],"total":1}),
    };
    crate::paths::write_private_file_atomic(
        &catalog.disk(&key),
        &serde_json::to_vec(&entry).unwrap(),
    )
    .unwrap();
    let requests = (0..20).map(|_| {
        let c = Arc::clone(&catalog);
        async move { c.get("me/albums", &[("offset", "0".into())]).await.unwrap() }
    });
    for value in futures::future::join_all(requests).await {
        assert_eq!(value["total"], 1);
    }
    catalog.invalidate().await;
    assert!(!catalog.disk(&key).exists());
    assert!(catalog.get("me/albums", &query).await.is_err());
}

#[tokio::test]
async fn artist_index_reads_one_page_without_resolving_any_track_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new(false);
    catalog.disk_root = dir.path().join("catalog");
    catalog.versions.get_mut().unwrap().clear();
    cache_following(
        &catalog,
        None,
        vec![json!({"id":"composer", "name":"Composer"})],
        None,
    );
    let albums: Vec<_> = (0..100).map(|index| json!({
        "id":format!("album{index}"), "name":format!("Release {index}"), "release_date":"2000",
        "rspc_track_uris":[format!("spotify:track:uncached{index}")]
    })).collect();
    let query = [("limit", "100".to_owned()), ("offset", "0".to_owned())];
    let key = catalog.cache_key("artists/composer/albums", &query);
    let cached = Cached {
        used: Default::default(),
        fetched: now(),
        value: json!({"items":albums,"next":"internal://more","total":700}),
    };
    crate::paths::write_private_file_atomic(
        &catalog.disk(&key),
        &serde_json::to_vec(&cached).unwrap(),
    )
    .unwrap();
    let catalog = Arc::new(catalog);
    // Missing pages and tracks fail immediately: only the first index page exists.
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    let page = catalog.artist_album_page("Composer", 0).await.unwrap();
    assert_eq!(page.albums.len(), 100);
    assert_eq!(page.next, Some(100));
    assert!(catalog.tracks.lock().await.is_empty());
    assert_eq!(catalog.albums.lock().await.len(), 100);
    catalog.cache.lock().await.clear();
    std::fs::remove_dir_all(&catalog.disk_root).unwrap();
    assert_eq!(
        catalog
            .artist_album_page("Composer", 0)
            .await
            .unwrap()
            .albums
            .len(),
        100
    );
    assert!(catalog.artist_album_page("Composer", 1).await.is_err());
    catalog
        .invalidate_artist_view("artists/composer/albums")
        .await;
    assert!(catalog.artist_pages.lock().await.is_empty());
}

#[tokio::test]
async fn artist_scoped_album_excludes_other_performers_without_refetching_the_album() {
    let catalog = Arc::new(Catalog::new(false));
    let mut a = demo_tracks().remove(0);
    a.uri = Some("spotify:track:a".into());
    a.artists = vec!["Composer".into()];
    let mut b = a.clone();
    b.uri = Some("spotify:track:b".into());
    b.artists = vec!["Other performer".into()];
    catalog.remember(vec![a.clone(), b]).await;
    catalog.albums.lock().await.insert(
        "compilation".into(),
        json!({"rspc_track_uris":["spotify:track:a","spotify:track:b"]}),
    );
    *catalog.cooldown.lock().await = Some(std::time::Instant::now() + Duration::from_secs(60));
    let tracks = catalog
        .resolve("rspc://artist-album/spotify/compilation?artist=Composer")
        .await
        .unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].uri, a.uri);
}

#[test]
fn catalog_eviction_keeps_hot_pages_instead_of_emptying_the_cache() {
    let mut cache = HashMap::new();
    for index in 0..CACHE_CAPACITY {
        cache_insert(
            &mut cache,
            index.to_string(),
            Arc::new(Cached {
                fetched: now(),
                value: json!(index),
                used: Default::default(),
            }),
        );
    }
    let hot = Arc::clone(&cache["0"]);
    cache_insert(&mut cache, "0".into(), hot);
    cache_insert(
        &mut cache,
        "new".into(),
        Arc::new(Cached {
            fetched: now(),
            value: json!("new"),
            used: Default::default(),
        }),
    );
    assert!(cache.len() > CACHE_CAPACITY / 2);
    assert!(cache.len() <= CACHE_CAPACITY);
    assert!(cache.contains_key("0"));
    assert!(cache.contains_key("new"));
    assert!(!cache.contains_key("1"));
}
