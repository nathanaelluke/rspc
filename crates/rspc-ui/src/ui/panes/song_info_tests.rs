use std::{collections::HashMap, sync::Arc};

use crossbeam::channel::{Receiver, Sender};
use ratatui::{Terminal, backend::TestBackend};
use rstest::rstest;

use crate::{
    AppEvent,
    config::keys::CommonAction,
    config::tabs::PaneType,
    ctx::Ctx,
    mpd::{commands::Song, mpd_client::Tag},
    shared::keys::ActionEvent,
    tests::fixtures::{app_event_channel, ctx},
    ui::{
        UiAppEvent,
        browser::BrowserPane,
        dir_or_song::DirOrSong,
        dirstack::Dir,
        modals::catalog::CatalogModal,
        panes::{
            albums::AlbumsPane, directories::DirectoriesPane, playlists::PlaylistsPane,
            tag_browser::TagBrowserPane,
        },
    },
};

fn assert_info(mut pane: impl BrowserPane<DirOrSong>, ctx: &mut Ctx, events: &Receiver<AppEvent>) {
    *pane.stack_mut().current_mut() = Dir::new(vec![DirOrSong::Song(Song {
        file: "outside-queue.flac".into(),
        metadata: HashMap::from([("title".into(), "Library song".into())]),
        ..Song::default()
    })]);
    let mut action: ActionEvent = Arc::new(vec![CommonAction::ShowInfo.into()]).into();
    pane.handle_common_action(&mut action, ctx)
        .expect("oi action");
    let mut modal = events
        .try_iter()
        .find_map(|event| match event {
            AppEvent::UiEvent(UiAppEvent::Modal(modal)) => Some(modal),
            _ => None,
        })
        .expect("song info must open without a server round trip");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| modal.render(frame, ctx).expect("song info"))
        .expect("render");
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("outside-queue.flac"));
    assert!(text.contains("Library song"));
}

#[rstest]
fn oi_works_for_songs_in_every_browser(
    mut ctx: Ctx,
    app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
) {
    ctx.app_event_sender = app_event_channel.0.clone();
    let events = &app_event_channel.1;
    assert_info(DirectoriesPane::new(&ctx), &mut ctx, events);
    assert_info(AlbumsPane::new(&ctx), &mut ctx, events);
    assert_info(
        TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx),
        &mut ctx,
        events,
    );
    assert_info(
        TagBrowserPane::new(Tag::AlbumArtist, PaneType::AlbumArtists, None, &ctx),
        &mut ctx,
        events,
    );
    assert_info(PlaylistsPane::new(&ctx), &mut ctx, events);
    assert_info(
        CatalogModal::new("rspc://catalog/release/fixture".into(), "Album".into()),
        &mut ctx,
        events,
    );
}

fn assert_empty_preview_is_cached(mut pane: impl BrowserPane<DirOrSong>, ctx: &mut Ctx) {
    let (sender, receiver) = crossbeam::channel::unbounded();
    ctx.client_request_sender = sender;
    *pane.stack_mut().current_mut() = Dir::new(vec![DirOrSong::name_only("Empty".into())]);
    pane.fetch_data_internal(ctx).expect("initial fetch");
    assert_eq!(receiver.try_iter().count(), 1);
    pane.stack_mut().insert("Empty".into(), Vec::new());
    for _ in 0..3 {
        pane.fetch_data_internal(ctx).expect("cached empty preview");
    }
    assert!(
        receiver.is_empty(),
        "a completed empty folder must not repeatedly query MPD"
    );
    pane.stack_mut()
        .get_mut(&"Empty".into())
        .expect("directory")
        .loaded = false;
    pane.fetch_data_internal(ctx).expect("invalidated preview");
    assert_eq!(receiver.try_iter().count(), 1);
}

#[rstest]
fn completed_empty_folders_do_not_refetch_on_every_render_or_info_action(mut ctx: Ctx) {
    assert_empty_preview_is_cached(DirectoriesPane::new(&ctx), &mut ctx);
    assert_empty_preview_is_cached(AlbumsPane::new(&ctx), &mut ctx);
    assert_empty_preview_is_cached(PlaylistsPane::new(&ctx), &mut ctx);
    assert_empty_preview_is_cached(
        TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx),
        &mut ctx,
    );
    assert_empty_preview_is_cached(
        TagBrowserPane::new(Tag::AlbumArtist, PaneType::AlbumArtists, None, &ctx),
        &mut ctx,
    );
    assert_empty_preview_is_cached(
        CatalogModal::new("rspc://catalog/release/fixture".into(), "Album".into()),
        &mut ctx,
    );
}

fn assert_folder_info(
    mut pane: impl BrowserPane<DirOrSong>,
    ctx: &mut Ctx,
    events: &Receiver<AppEvent>,
) {
    *pane.stack_mut().current_mut() = Dir::new(vec![DirOrSong::name_only("Folder".into())]);
    pane.stack_mut().insert(
        "Folder".into(),
        vec![DirOrSong::Song(Song {
            file: "cached.flac".into(),
            duration: Some(std::time::Duration::from_secs(123)),
            ..Song::default()
        })],
    );
    let mut action: ActionEvent = Arc::new(vec![CommonAction::ShowInfo.into()]).into();
    pane.handle_common_action(&mut action, ctx)
        .expect("folder info action");
    let mut modal = events
        .try_iter()
        .find_map(|event| match event {
            AppEvent::UiEvent(UiAppEvent::Modal(modal)) => Some(modal),
            _ => None,
        })
        .expect("cached folder summary");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| modal.render(frame, ctx).expect("render summary"))
        .expect("draw summary");
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("Selection info"));
    assert!(text.contains("Songs"));
    assert!(text.contains("2:03"), "{text}");
    assert!(!text.contains("Loading"));
}

#[rstest]
fn cached_folder_info_works_in_every_browser(
    mut ctx: Ctx,
    app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
) {
    ctx.app_event_sender = app_event_channel.0.clone();
    let events = &app_event_channel.1;
    assert_folder_info(DirectoriesPane::new(&ctx), &mut ctx, events);
    assert_folder_info(AlbumsPane::new(&ctx), &mut ctx, events);
    assert_folder_info(
        TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx),
        &mut ctx,
        events,
    );
    assert_folder_info(
        TagBrowserPane::new(Tag::AlbumArtist, PaneType::AlbumArtists, None, &ctx),
        &mut ctx,
        events,
    );
    assert_folder_info(PlaylistsPane::new(&ctx), &mut ctx, events);
    assert_folder_info(
        CatalogModal::new(
            "rspc://catalog/artist/fixture/Artist".into(),
            "Artist".into(),
        ),
        &mut ctx,
        events,
    );
}

#[rstest]
fn incomplete_folder_is_not_reported_as_empty(ctx: Ctx) {
    let mut pane = DirectoriesPane::new(&ctx);
    *pane.stack_mut().current_mut() = Dir::new(vec![DirOrSong::name_only("Loading".into())]);
    let item = pane.stack().current().items[0].clone();
    pane.stack_mut().enter();
    pane.stack_mut().leave();
    assert!(pane.cached_info(&item).is_none());
    pane.stack_mut().insert("Loading".into(), Vec::new());
    assert!(
        pane.cached_info(&item).is_some(),
        "a completed empty folder has a valid summary"
    );
}

#[rstest]
fn marked_songs_take_precedence_over_the_hovered_song(
    mut ctx: Ctx,
    app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
) {
    ctx.app_event_sender = app_event_channel.0.clone();
    let mut pane = DirectoriesPane::new(&ctx);
    *pane.stack_mut().current_mut() = Dir::new(
        (0..3)
            .map(|i| {
                DirOrSong::Song(Song {
                    file: format!("song-{i}"),
                    duration: Some(std::time::Duration::from_secs(60)),
                    ..Song::default()
                })
            })
            .collect(),
    );
    pane.stack_mut().current_mut().state.mark(1);
    pane.stack_mut().current_mut().state.mark(2);
    let mut action: ActionEvent = Arc::new(vec![CommonAction::ShowInfo.into()]).into();
    pane.handle_common_action(&mut action, &ctx)
        .expect("marked info");
    let mut modal = app_event_channel
        .1
        .try_iter()
        .find_map(|event| match event {
            AppEvent::UiEvent(UiAppEvent::Modal(modal)) => Some(modal),
            _ => None,
        })
        .expect("marked summary");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| modal.render(frame, &mut ctx).expect("summary"))
        .expect("draw");
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("2:00"));
    assert!(!text.contains("song-0"));
}

#[rstest]
fn empty_curator_profiles_keep_their_playlist_summary(mut ctx: Ctx) {
    ctx.supported_commands.insert("rspc_identity".into());
    let mut pane = DirectoriesPane::new(&ctx);
    let item = DirOrSong::Dir {
        name: "Curator".into(),
        full_path: "People/profile/id/Curator".into(),
        last_modified: chrono::Utc::now(),
        playlist: false,
    };
    *pane.stack_mut().current_mut() = Dir::new(vec![item.clone()]);
    pane.stack_mut().insert("Curator".into(), Vec::new());
    assert!(pane.cached_info(&item).is_none());
    assert_eq!(
        pane.info_target(&item),
        Some(("path", "People/profile/id/Curator".into()))
    );
}
