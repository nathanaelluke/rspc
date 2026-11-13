#![allow(clippy::unwrap_used)]

use std::{
    collections::HashMap,
    sync::{
        LazyLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use rstest::{fixture, rstest};

use crate::{
    ctx::Ctx,
    mpd::commands::Song,
    tests::fixtures::ctx,
    ui::{
        browser::BrowserPane,
        dir_or_song::DirOrSong,
        panes::{Pane, playlists::PlaylistsPane},
    },
};

mod sorting {
    use super::*;
    use crate::{
        config::sort_mode::{SongSort, SongSortKey},
        shared::{
            events::{AppEvent, ClientRequest, WorkRequest},
            mpd_client_ext::MpdDelete,
        },
        tests::fixtures::{app_event_channel, client_request_channel, work_request_channel},
        ui::{
            browser::MoveDirection,
            dirstack::{DirStack, DirStackItem, Path},
        },
    };
    use crossbeam::channel::{Receiver, Sender};

    fn row(name: &str, added: i64) -> DirOrSong {
        let mut song = song(name);
        song.metadata.insert("title".into(), name.into());
        song.added = chrono::DateTime::from_timestamp(added, 0);
        DirOrSong::Song(song)
    }

    #[rstest]
    fn sorting_is_per_playlist_and_keeps_backend_positions_without_sending_mutations(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let ctx = ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel.clone(),
        );
        let mut screen = PlaylistsPane::new(&ctx);
        screen.stack = DirStack::new(vec![dir("First"), dir("Second")]);
        screen.stack.insert(
            Path::new().join("First"),
            vec![row("Zulu", 10), row("Alpha", 20)],
        );
        screen.stack.insert(
            Path::new().join("Second"),
            vec![row("Old", 100), row("New", 200)],
        );
        screen.stack.enter();
        screen.change_sort(Some(SongSortKey::Title), &ctx).unwrap();
        assert_eq!(screen.stack.current().items[0].as_path(), "Alpha");
        screen.stack.current_mut().select_idx(0, 0);
        let delete = screen.delete_items(false);
        match delete.as_slice() {
            [MpdDelete::SongInPlaylist { playlist, range }] => {
                assert_eq!(playlist.as_ref(), "First");
                assert_eq!(range.start, 1);
                assert_eq!(range.end, None);
            }
            other => panic!("expected one playlist deletion, got {other:?}"),
        }
        screen.move_selected(MoveDirection::Down, &ctx).unwrap();
        screen.stack.leave();
        screen.stack.current_mut().select_idx(1, 0);
        screen.stack.enter();
        screen.change_sort(Some(SongSortKey::Recent), &ctx).unwrap();
        assert_eq!(screen.stack.current().items[0].as_path(), "New");
        assert_eq!(
            ctx.collection_sorts.borrow().get("playlist:First"),
            SongSort::new(SongSortKey::Title)
        );
        assert_eq!(
            ctx.collection_sorts.borrow().get("playlist:Second"),
            SongSort::new(SongSortKey::Recent)
        );
        screen.stack.leave();
        screen.stack.current_mut().select_idx(0, 0);
        screen.restore_sort(&ctx);
        assert_eq!(screen.stack.next().unwrap().items[0].as_path(), "Alpha");
        screen.stack.enter();
        screen.change_sort(None, &ctx).unwrap();
        assert_eq!(screen.stack.current().items[0].as_path(), "Zulu");
        screen
            .change_sort(Some(SongSortKey::Original), &ctx)
            .unwrap();
        assert_eq!(screen.stack.current().items[0].as_path(), "Zulu");
        assert!(!screen.stack.current().has_sort());
        assert!(
            client_request_channel.1.try_recv().is_err(),
            "view sorting must not send playlist changes"
        );
    }
}

mod on_idle_event {
    use super::*;
    use crate::{
        ctx::Ctx,
        shared::mpd_query::MpdQueryResult,
        ui::panes::playlists::{INIT, REINIT},
    };

    mod browsing_playlists {

        use super::*;

        #[rstest]
        fn selects_the_same_playlist_by_name(mut screen: PlaylistsPane, ctx: Ctx) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            let current = screen.stack.current_mut();
            current.select_idx(1, 0);
            assert_eq!(current.selected(), Some(dir("pl2")).as_ref());

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl2"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected(), Some(dir("pl2")).as_ref());
        }

        #[rstest]
        fn selects_the_same_index_when_playlist_not_found_after_refresh(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 2);
        }

        #[rstest]
        fn selects_the_last_playlist_when_last_was_selected_and_removed(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(3, 0);

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 1);
        }

        #[rstest]
        fn selects_the_first_playlist_when_first_was_selected_and_removed(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(0, 0);
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 0);
        }
    }

    mod browsing_songs {
        use crossbeam::channel::{Receiver, Sender};

        use super::*;
        use crate::{
            shared::events::{AppEvent, ClientRequest, WorkRequest},
            tests::fixtures::{app_event_channel, client_request_channel, work_request_channel},
            ui::panes::playlists::FETCH_DATA,
        };

        #[rstest]
        fn selects_the_same_playlist_and_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(
                app_event_channel,
                work_request_channel,
                client_request_channel,
            );
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            // init playlists
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            // select third playlist ind init its songs
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            // select third song - s3
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );

            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s2"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            // send in new songs without s1
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx
                        .send(MpdQueryResult::Any(Box::new(new_songs2)))
                        .unwrap();
                }
            });
            // trigger reinit of playlists without pl1
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(
                screen.stack.previous().and_then(|p| p.selected()),
                Some(&dir("pl3"))
            );
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_last_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(
                app_event_channel,
                work_request_channel,
                client_request_channel,
            );
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s2")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx
                        .send(MpdQueryResult::Any(Box::new(new_songs2)))
                        .unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(
                screen.stack.previous().and_then(|p| p.selected()),
                Some(&dir("pl3"))
            );

            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_first_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(
                app_event_channel,
                work_request_channel,
                client_request_channel,
            );
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx
                        .send(MpdQueryResult::Any(Box::new(new_songs2)))
                        .unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(
                screen.stack.previous().and_then(|p| p.selected()),
                Some(&dir("pl3"))
            );
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[0].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_song_idx(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(
                app_event_channel,
                work_request_channel,
                client_request_channel,
            );
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(1, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[1].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx
                        .send(MpdQueryResult::Any(Box::new(new_songs2)))
                        .unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(
                screen.stack.previous().and_then(|p| p.selected()),
                Some(&dir("pl3"))
            );
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_idx_and_last_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(
                app_event_channel,
                work_request_channel,
                client_request_channel,
            );
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            let initial_playlists = vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: initial_playlists,
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(1, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[1].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx
                        .send(MpdQueryResult::Any(Box::new(new_songs2)))
                        .unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(
                screen.stack.previous().and_then(|p| p.selected()),
                Some(&dir("pl4"))
            );
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }
    }
}

static LAST_ID: AtomicU32 = AtomicU32::new(1);
static NOW: LazyLock<chrono::DateTime<chrono::Utc>> = LazyLock::new(chrono::Utc::now);

pub fn new_id() -> u32 {
    LAST_ID.fetch_add(1, Ordering::Relaxed)
}
fn song(name: &str) -> Song {
    Song {
        id: new_id(),
        file: name.to_string(),
        duration: Some(Duration::from_secs(1)),
        metadata: HashMap::new(),
        last_modified: *NOW,
        added: None,
    }
}

fn dir(name: &str) -> DirOrSong {
    DirOrSong::Dir {
        name: name.to_string(),
        full_path: name.to_string(),
        last_modified: *NOW,
        playlist: false,
    }
}

#[fixture]
fn screen(ctx: Ctx) -> PlaylistsPane {
    let mut screen = PlaylistsPane::new(&ctx);
    screen.before_show(&ctx).unwrap();
    screen
}
