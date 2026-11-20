use std::{collections::HashSet, sync::Arc};

use anyhow::Result;

use super::{ListSection, create_save_modal, list_section::MenuItem};
use crate::{
    config::keys::actions::DuplicateStrategy,
    ctx::Ctx,
    mpd::{
        client::Client, commands::library::LibraryItem, errors::MpdError, mpd_client::MpdClient,
    },
    shared::{
        id::{self, Id},
        macros::{modal, status_info, status_warn},
    },
    ui::modals::confirm_modal::{Action, ConfirmModal},
};

type SongSelection = Arc<dyn Fn(&mut Client<'_>) -> Result<Vec<String>> + Send + Sync>;
pub(super) const QUERY_ID: &str = "library_menu";

#[derive(Debug, Clone, Copy, Default)]
enum LoadState {
    #[default]
    Pending,
    Loading(Id),
    Ready,
}

#[derive(Clone, Default, derive_more::Debug)]
pub(super) struct LibraryRequest {
    #[debug(skip)]
    songs: Option<SongSelection>,
    targets: Vec<(String, String)>,
    index: usize,
    len: usize,
    state: LoadState,
}

pub(super) struct LibraryData {
    pub(super) modal: Id,
    pub(super) section: usize,
    request: Id,
    liked: Option<Arc<HashSet<String>>>,
    add: Vec<String>,
    remove: Vec<String>,
    saved: Vec<LibraryItem>,
    errors: Vec<String>,
}

impl LibraryRequest {
    pub(super) fn reset(&mut self) {
        self.state = LoadState::Pending;
    }

    fn load(
        &self,
        client: &mut Client<'_>,
        cached: Option<Arc<HashSet<String>>>,
        liked_supported: bool,
        saved_supported: bool,
        mut data: LibraryData,
    ) -> Result<LibraryData> {
        if liked_supported && let Some(songs) = &self.songs {
            let result = (|| -> Result<_> {
                let mut paths = songs(client)?;
                paths.retain(|uri| uri.starts_with("spotify:track:"));
                let paths = liked_song_paths(paths)?;
                let liked = match cached {
                    Some(liked) => Some(liked),
                    None if paths.is_empty() => None,
                    None => Some(Arc::new(
                        client
                            .selection_songs("Liked Songs")?
                            .into_iter()
                            .map(|song| song.file)
                            .collect::<HashSet<_>>(),
                    )),
                };
                let (remove, add) = paths
                    .into_iter()
                    .partition(|uri| liked.as_ref().is_some_and(|liked| liked.contains(uri)));
                Ok((liked, remove, add))
            })();
            match result {
                Ok((liked, remove, add)) => {
                    data.liked = liked;
                    data.remove = remove;
                    data.add = add;
                }
                Err(error) => data.record_error("Liked Songs", error)?,
            }
        }
        if saved_supported {
            for (kind, value) in &self.targets {
                match client.library_items(kind, value) {
                    Ok(items) => data.saved.extend(items),
                    Err(error) => data.record_error("saved albums and artists", error.into())?,
                }
            }
            let mut seen = HashSet::new();
            data.saved.retain(|item| seen.insert(item.uri.clone()));
        }
        Ok(data)
    }
}

impl LibraryData {
    fn record_error(&mut self, collection: &str, error: anyhow::Error) -> Result<()> {
        if matches!(
            error.downcast_ref::<MpdError>(),
            Some(MpdError::TimedOut(_))
        ) {
            return Err(error);
        }
        self.errors
            .push(format!("Could not check {collection}: {error}"));
        Ok(())
    }
}

fn request(section: &mut ListSection) -> &mut LibraryRequest {
    let index = section.items.len();
    section.library.get_or_insert_with(|| LibraryRequest {
        index,
        ..LibraryRequest::default()
    })
}

pub fn add_saved_library_actions(section: &mut ListSection, ctx: &Ctx, kind: &str, value: String) {
    if ctx.supported_commands.contains("rspc_library_items")
        && ctx.supported_commands.contains("rspc_library_update")
    {
        request(section).targets.push((kind.to_owned(), value));
    }
}

pub fn add_song_collection_actions(
    section: &mut ListSection,
    ctx: &Ctx,
    songs: impl FnOnce(&mut Client<'_>) -> Result<Vec<String>> + Clone + Send + Sync + 'static,
) {
    let playlist_songs = songs.clone();
    section.add_item("Add to playlist", move |ctx| {
        let paths = ctx.query_sync(playlist_songs)?;
        if paths.is_empty() {
            status_warn!("No songs selected");
            return Ok(());
        }
        modal!(
            ctx,
            create_save_modal(paths, None, DuplicateStrategy::Ask, ctx)?
        );
        Ok(())
    });
    if ctx.supported_commands.contains("rspc_library_add")
        || ctx.supported_commands.contains("rspc_library_remove")
    {
        request(section).songs = Some(Arc::new(move |client| songs.clone()(client)));
    }
}

fn replace_items(section: &mut ListSection, items: Vec<MenuItem>) {
    let selected = section.state.get_selected();
    let label = selected
        .and_then(|index| section.items.get(index))
        .map(|item| item.label.clone());
    let request = section.library.as_mut().expect("library section");
    let count = items.len();
    section
        .items
        .splice(request.index..request.index + request.len, items);
    request.len = count;
    section.state.set_content_len(Some(section.items.len()));
    if let Some(selected) = selected {
        let index = label
            .and_then(|label| section.items.iter().position(|item| item.label == label))
            .or_else(|| (!section.items.is_empty()).then(|| selected.min(section.items.len() - 1)));
        section.state.select(index, 0);
    }
}

pub(super) fn start_loading(section: &mut ListSection, ctx: &Ctx, modal: Id, index: usize) -> bool {
    let Some(request) = section
        .library
        .as_ref()
        .filter(|request| matches!(request.state, LoadState::Pending))
        .cloned()
    else {
        return false;
    };
    let id = id::new();
    section.library.as_mut().expect("library section").state = LoadState::Loading(id);
    replace_items(
        section,
        vec![MenuItem {
            label: "Loading library…".into(),
            on_confirm: None,
        }],
    );
    let cached = ctx.liked_song_uris.borrow().clone();
    let liked_supported = ctx.supported_commands.contains("rspc_library_add")
        || ctx.supported_commands.contains("rspc_library_remove");
    let saved_supported = ctx.supported_commands.contains("rspc_library_items")
        && ctx.supported_commands.contains("rspc_library_update");
    ctx.query().id(QUERY_ID).query(move |client| {
        let data = LibraryData {
            modal,
            section: index,
            request: id,
            liked: None,
            add: Vec::new(),
            remove: Vec::new(),
            saved: Vec::new(),
            errors: Vec::new(),
        };
        Ok(crate::MpdQueryResult::Any(Box::new(request.load(
            client,
            cached,
            liked_supported,
            saved_supported,
            data,
        )?)))
    });
    true
}

pub(super) fn is_loading(section: &ListSection) -> bool {
    section
        .library
        .as_ref()
        .is_some_and(|request| !matches!(request.state, LoadState::Ready))
}

pub(super) fn apply(section: &mut ListSection, data: &mut LibraryData, ctx: &Ctx) -> bool {
    let Some(request) = section.library.as_mut() else {
        return false;
    };
    if !matches!(request.state, LoadState::Loading(id) if id == data.request) {
        return false;
    }
    request.state = LoadState::Ready;
    if let Some(liked) = data.liked.take() {
        ctx.liked_song_uris.replace(Some(liked));
    }
    let mut actions = ListSection::new(ctx.config.theme.current_item_style);
    add_liked_song_actions(
        &mut actions,
        ctx,
        std::mem::take(&mut data.add),
        std::mem::take(&mut data.remove),
    );
    append_saved_actions(&mut actions, std::mem::take(&mut data.saved));
    replace_items(section, actions.items);
    for error in data.errors.drain(..) {
        status_warn!("{error}");
    }
    true
}

fn append_saved_actions(section: &mut ListSection, items: Vec<LibraryItem>) {
    for item in items {
        let (label, done) = match (item.uri.starts_with("spotify:album:"), item.saved) {
            (true, false) => ("Save album", "Saved album"),
            (true, true) => ("Remove album", "Removed album"),
            (false, false) => ("Follow artist", "Following artist"),
            (false, true) => ("Unfollow artist", "Unfollowed artist"),
        };
        section.add_item(format!("{label}: {}", item.name), move |ctx| {
            let remove = item.saved;
            let message = format!("{label} \"{}\"?", item.name);
            let update = move |ctx: &Ctx| -> Result<()> {
                ctx.command(move |client| {
                    client.update_saved_item(&item.uri, item.saved)?;
                    status_info!("{done}: {}", item.name);
                    Ok(())
                });
                Ok(())
            };
            if remove {
                modal!(
                    ctx,
                    ConfirmModal::builder()
                        .ctx(ctx)
                        .message(vec![message])
                        .action(Action::Single {
                            confirm_label: Some(label),
                            cancel_label: None,
                            on_confirm: Box::new(update),
                        })
                        .build()
                );
            } else {
                update(ctx)?;
            }
            Ok(())
        });
    }
}

fn add_liked_song_actions(
    section: &mut ListSection,
    ctx: &Ctx,
    add: Vec<String>,
    remove: Vec<String>,
) {
    if !add.is_empty() && ctx.supported_commands.contains("rspc_library_add") {
        section.add_item("Add to Liked Songs", move |ctx| {
            add_to_liked_songs(ctx, move |_| Ok(add));
            Ok(())
        });
    }
    if !remove.is_empty() && ctx.supported_commands.contains("rspc_library_remove") {
        section.add_item("Remove from Liked Songs", move |ctx| {
            remove_from_liked_songs(ctx, remove)
        });
    }
}

pub fn add_to_liked_songs(
    ctx: &Ctx,
    songs: impl FnOnce(&mut Client<'_>) -> Result<Vec<String>> + Send + 'static,
) {
    update_liked_songs(ctx, songs, LikedSongsEdit::Add);
}

pub fn update_selected_liked_songs(
    ctx: &Ctx,
    songs: impl FnOnce(&mut Client<'_>) -> Result<Vec<String>> + Send + 'static,
    toggle: bool,
) {
    update_liked_songs(
        ctx,
        songs,
        if toggle {
            LikedSongsEdit::Toggle
        } else {
            LikedSongsEdit::Add
        },
    );
}

fn liked_song_paths(mut paths: Vec<String>) -> Result<Vec<String>> {
    anyhow::ensure!(
        paths.iter().all(|uri| uri.starts_with("spotify:track:")),
        "Liked Songs accepts Spotify tracks only"
    );
    let mut known = HashSet::new();
    paths.retain(|uri| known.insert(uri.clone()));
    Ok(paths)
}

pub fn remove_from_liked_songs(ctx: &Ctx, paths: Vec<String>) -> Result<()> {
    if !ctx.supported_commands.contains("rspc_library_remove") {
        status_warn!("The connected server does not support removing Spotify Liked Songs");
        return Ok(());
    }
    let paths = liked_song_paths(paths)?;
    if paths.is_empty() {
        status_warn!("No songs selected");
        return Ok(());
    }
    let suffix = if paths.len() == 1 { "" } else { "s" };
    modal!(
        ctx,
        ConfirmModal::builder()
            .ctx(ctx)
            .message(vec![format!(
                "Remove {} song{suffix} from Spotify Liked Songs?",
                paths.len()
            )])
            .action(Action::Single {
                confirm_label: Some("Remove"),
                cancel_label: None,
                on_confirm: Box::new(move |ctx| {
                    update_liked_songs(ctx, move |_| Ok(paths), LikedSongsEdit::Remove);
                    Ok(())
                }),
            })
            .size((60, 6))
            .build()
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum LikedSongsEdit {
    Add,
    Remove,
    Toggle,
}

fn update_liked_songs(
    ctx: &Ctx,
    songs: impl FnOnce(&mut Client<'_>) -> Result<Vec<String>> + Send + 'static,
    edit: LikedSongsEdit,
) {
    let command = match edit {
        LikedSongsEdit::Add => "rspc_library_add",
        LikedSongsEdit::Remove => "rspc_library_remove",
        LikedSongsEdit::Toggle => "rspc_library_toggle",
    };
    if !ctx.supported_commands.contains(command) {
        status_warn!(
            "Spotify Liked Songs is unavailable on this server. Open rspc without --local; if rspc is already running, run `rspc stop` and reopen it to update the daemon."
        );
        return;
    }
    ctx.liked_song_uris.borrow_mut().take();
    ctx.command(move |client| {
        let paths = liked_song_paths(songs(client)?)?;
        if paths.is_empty() {
            status_warn!("No songs selected");
            return Ok(());
        }
        let (added, removed) = match edit {
            LikedSongsEdit::Add => (client.add_liked_tracks(&paths)?, 0),
            LikedSongsEdit::Remove => (0, client.remove_liked_tracks(&paths)?),
            LikedSongsEdit::Toggle => client.toggle_liked_tracks(&paths)?,
        };
        match (added, removed) {
            (0, 0) if matches!(edit, LikedSongsEdit::Remove) => status_info!("Not in Liked Songs"),
            (0, 0) => status_info!("Already in Liked Songs"),
            (count, 0) => {
                let suffix = if count == 1 { "" } else { "s" };
                status_info!("Added {count} song{suffix} to Liked Songs");
            }
            (0, count) => {
                let suffix = if count == 1 { "" } else { "s" };
                status_info!("Removed {count} song{suffix} from Liked Songs");
            }
            _ => status_info!("Liked {added} and unliked {removed} in Liked Songs"),
        }
        Ok(())
    });
}

#[cfg(test)]
mod save_action_tests {
    use super::*;
    use crate::tests::fixtures::ctx;
    use itertools::Itertools;
    use rstest::rstest;

    fn enable_library(ctx: &mut Ctx) {
        ctx.supported_commands
            .extend(["rspc_library_add".into(), "rspc_library_remove".into()]);
    }

    fn reply(request: Id, uri: &str) -> LibraryData {
        LibraryData {
            modal: id::new(),
            section: 0,
            request,
            liked: Some(Arc::new(HashSet::from([uri.to_owned()]))),
            add: vec![uri.to_owned()],
            remove: Vec::new(),
            saved: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[rstest]
    fn menus_render_and_close_without_waiting_for_library_requests(mut ctx: Ctx) {
        use crate::{
            shared::{events::ClientRequest, keys::ActionEvent},
            ui::{
                UiAppEvent,
                modals::{Modal, menu::modal::MenuModal},
            },
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        enable_library(&mut ctx);
        let (sender, requests) = crossbeam::channel::unbounded();
        ctx.client_request_sender = sender;
        let (sender, events) = crossbeam::channel::unbounded();
        ctx.app_event_sender = sender;
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let mut menu = MenuModal::new(&ctx)
            .list_section(&ctx, |mut section| {
                add_song_collection_actions(&mut section, &ctx, move |_| {
                    observed.store(true, Ordering::Relaxed);
                    Ok(vec!["spotify:track:fixture".into()])
                });
                Some(section)
            })
            .build();
        assert!(!called.load(Ordering::Relaxed));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| menu.render(frame, &mut ctx).expect("render menu"))
            .expect("draw menu");
        assert!(!called.load(Ordering::Relaxed));
        assert!(
            matches!(requests.try_recv().expect("background library query"), ClientRequest::Query(query) if query.id == "library_menu")
        );
        assert!(requests.try_recv().is_err());
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!text.contains("Loading library"));
        assert!(!text.contains("Add to playlist"));
        for kind in [
            crate::shared::mouse_event::MouseEventKind::LeftClick,
            crate::shared::mouse_event::MouseEventKind::DoubleClick,
        ] {
            menu.handle_mouse_event(
                crate::shared::mouse_event::MouseEvent { x: 40, y: 10, kind },
                &mut ctx,
            )
            .expect("click disabled loading item");
        }
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, crate::AppEvent::UiEvent(UiAppEvent::PopModal(_))))
        );
        let mut close: ActionEvent =
            Arc::new(vec![crate::config::keys::CommonAction::Close.into()]).into();
        menu.handle_key(&mut close, &mut ctx)
            .expect("close pending menu");
        assert!(events.try_iter().any(|event| matches!(event, crate::AppEvent::UiEvent(UiAppEvent::PopModal(id)) if id == menu.id())));
        assert!(!called.load(Ordering::Relaxed));
    }

    #[rstest]
    fn reconnect_restarts_a_menu_query_that_never_completed(mut ctx: Ctx) {
        use crate::{
            shared::events::ClientRequest,
            ui::{
                UiEvent,
                modals::{Modal, menu::modal::MenuModal},
            },
        };

        enable_library(&mut ctx);
        let (sender, requests) = crossbeam::channel::unbounded();
        ctx.client_request_sender = sender;
        let mut menu = MenuModal::new(&ctx)
            .list_section(&ctx, |mut section| {
                add_song_collection_actions(&mut section, &ctx, |_| Ok(Vec::new()));
                Some(section)
            })
            .build();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| menu.render(frame, &mut ctx).expect("render waiting menu"))
            .expect("draw waiting menu");
        assert!(matches!(
            requests.try_recv().expect("first query"),
            ClientRequest::Query(query) if query.id == QUERY_ID
        ));
        assert!(requests.try_recv().is_err());
        menu.on_event(&mut UiEvent::Reconnected, &ctx)
            .expect("reset interrupted library load");
        terminal
            .draw(|frame| {
                menu.render(frame, &mut ctx)
                    .expect("render reconnecting menu");
            })
            .expect("draw reconnecting menu");
        assert!(matches!(
            requests.try_recv().expect("replacement query"),
            ClientRequest::Query(query) if query.id == QUERY_ID
        ));
        terminal
            .draw(|frame| menu.render(frame, &mut ctx).expect("render loading menu"))
            .expect("draw loading menu");
        assert!(requests.try_recv().is_err());
    }

    #[rstest]
    fn stale_library_replies_do_not_restore_invalidated_membership(mut ctx: Ctx) {
        enable_library(&mut ctx);
        let mut section = ListSection::default();
        add_song_collection_actions(&mut section, &ctx, |_| Ok(Vec::new()));
        let modal = id::new();
        assert!(start_loading(&mut section, &ctx, modal, 0));
        let LoadState::Loading(old) = section.library.as_ref().expect("library request").state
        else {
            panic!("request");
        };
        section.library.as_mut().expect("library request").reset();
        assert!(start_loading(&mut section, &ctx, modal, 0));
        let LoadState::Loading(current) = section.library.as_ref().expect("library request").state
        else {
            panic!("request");
        };
        assert!(!apply(
            &mut section,
            &mut reply(old, "spotify:track:stale"),
            &ctx
        ));
        assert!(ctx.liked_song_uris.borrow().is_none());
        assert!(apply(
            &mut section,
            &mut reply(current, "spotify:track:current"),
            &ctx
        ));
        assert!(
            ctx.liked_song_uris
                .borrow()
                .as_ref()
                .expect("loaded Liked Songs membership")
                .contains("spotify:track:current")
        );
        assert!(
            !ctx.liked_song_uris
                .borrow()
                .as_ref()
                .expect("loaded Liked Songs membership")
                .contains("spotify:track:stale")
        );
        assert!(!apply(
            &mut section,
            &mut reply(old, "spotify:track:stale"),
            &ctx
        ));
    }

    #[rstest]
    fn loading_library_actions_preserves_the_selected_static_item(mut ctx: Ctx) {
        enable_library(&mut ctx);
        let mut section = ListSection::default();
        add_song_collection_actions(&mut section, &ctx, |_| Ok(Vec::new()));
        section.add_item("Cancel", |_| Ok(()));
        section.state.set_content_len(Some(section.items.len()));
        section.state.select(Some(1), 0);
        start_loading(&mut section, &ctx, id::new(), 0);
        assert_eq!(
            section.items[section.state.get_selected().expect("selected menu item")].label,
            "Cancel"
        );
        let request = section.library.as_ref().expect("library request");
        let LoadState::Loading(id) = request.state else {
            panic!("request");
        };
        section.state.select(Some(request.index), 0);
        assert!(
            !super::super::Section::confirm(&mut section, &ctx).expect("disabled loading item")
        );
        section.state.select(Some(section.items.len() - 1), 0);
        apply(&mut section, &mut reply(id, "spotify:track:fixture"), &ctx);
        assert_eq!(
            section.items[section.state.get_selected().expect("selected menu item")].label,
            "Cancel"
        );
        assert_eq!(
            section
                .items
                .iter()
                .filter(|item| item.label == "Add to Liked Songs")
                .count(),
            1
        );
        assert!(
            super::super::Section::double_click(
                &mut section,
                ratatui::layout::Position::default(),
                &ctx
            )
            .expect("confirm selected action")
        );
    }

    #[rstest]
    fn filters_typed_while_loading_match_the_arriving_actions(mut ctx: Ctx) {
        use crate::ui::{
            input::{InputMode, InputResultEvent},
            modals::{Modal, menu::modal::MenuModal},
        };
        use ratatui::style::{Color, Style};

        enable_library(&mut ctx);
        let highlight = Color::Rgb(58, 58, 58);
        Arc::make_mut(&mut ctx.config).theme.current_item_style = Style::default().bg(highlight);
        let menu = MenuModal::new(&ctx);
        let mut section = ListSection::new(ctx.config.theme.current_item_style);
        add_song_collection_actions(&mut section, &ctx, |_| Ok(Vec::new()));
        start_loading(&mut section, &ctx, menu.id(), 0);
        let LoadState::Loading(request) = section.library.as_ref().expect("library request").state
        else {
            panic!("request");
        };
        let mut menu = menu.list_section(&ctx, |_| Some(section)).build();
        let mut search =
            Arc::new(vec![crate::config::keys::CommonAction::EnterSearch.into()]).into();
        menu.handle_key(&mut search, &mut ctx)
            .expect("enter menu filter");
        let InputMode::Insert(buffer) = ctx.input.mode() else {
            panic!("filter input");
        };
        ctx.input.set_buffer("LiKeD".into(), buffer);
        menu.handle_insert_mode(InputResultEvent::Push, &mut ctx)
            .expect("type menu filter");

        let mut stale =
            crate::MpdQueryResult::Any(Box::new(reply(request, "spotify:track:wrong-menu")));
        menu.on_query_finished(QUERY_ID, &mut stale, &ctx)
            .expect("ignore another menu reply");
        assert!(ctx.liked_song_uris.borrow().is_none());
        let mut data = reply(request, "spotify:track:current");
        data.modal = menu.id();
        let mut data = crate::MpdQueryResult::Any(Box::new(data));
        menu.on_query_finished(QUERY_ID, &mut data, &ctx)
            .expect("apply library actions");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| menu.render(frame, &mut ctx).expect("render menu"))
            .expect("draw menu");
        let cells = terminal.backend().buffer();
        let row = (0..20)
            .find(|y| {
                (0..80)
                    .map(|x| cells[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Add to Liked Songs")
            })
            .expect("liked-song action row");
        let column = (0..80)
            .find(|x| cells[(*x, row)].symbol() == "A")
            .expect("liked-song action");
        assert_eq!(cells[(column, row)].bg, highlight);
    }

    #[rstest]
    fn reconnect_updates_liked_songs_actions_and_sticker_support(mut ctx: Ctx) {
        for supported in [false, true, false] {
            ctx.liked_song_uris
                .replace(Some(HashSet::from(["spotify:track:cached".into()]).into()));
            let commands = if supported {
                [
                    "rspc_library_add".into(),
                    "rspc_library_remove".into(),
                    "sticker".into(),
                ]
                .into_iter()
                .collect()
            } else {
                std::collections::HashSet::new()
            };
            let version = crate::mpd::version::Version {
                major: 0,
                minor: 24,
                patch: 0,
            };
            ctx.update_server_capabilities(version, commands);
            assert!(ctx.liked_song_uris.borrow().is_none());
            assert_eq!(ctx.mpd_version, version);
            assert_eq!(
                matches!(
                    ctx.stickers_supported,
                    crate::ctx::StickersSupport::Supported
                ),
                supported
            );
            let mut section = ListSection::default();
            add_liked_song_actions(
                &mut section,
                &ctx,
                vec!["spotify:track:new".into()],
                vec!["spotify:track:old".into()],
            );
            let labels = section
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>();
            let expected = if supported {
                vec!["Add to Liked Songs", "Remove from Liked Songs"]
            } else {
                Vec::new()
            };
            assert_eq!(labels, expected);
        }
    }

    #[rstest]
    #[case(true, false, vec!["Add to Liked Songs"])]
    #[case(false, true, vec!["Remove from Liked Songs"])]
    #[case(true, true, vec!["Add to Liked Songs", "Remove from Liked Songs"])]
    #[case(false, false, Vec::<&str>::new())]
    fn liked_song_actions_require_an_eligible_selection(
        mut ctx: Ctx,
        #[case] can_add: bool,
        #[case] can_remove: bool,
        #[case] expected: Vec<&str>,
    ) {
        ctx.supported_commands
            .extend(["rspc_library_add".into(), "rspc_library_remove".into()]);
        let add = if can_add {
            vec!["spotify:track:new".into()]
        } else {
            Vec::new()
        };
        let remove = if can_remove {
            vec!["spotify:track:old".into()]
        } else {
            Vec::new()
        };
        let mut section = ListSection::default();
        add_liked_song_actions(&mut section, &ctx, add, remove);
        assert_eq!(
            section
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect_vec(),
            expected
        );
    }
}
