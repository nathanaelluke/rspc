use anyhow::Result;
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{Frame, prelude::Rect, widgets::ListState};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::tabs::PaneType,
    ctx::Ctx,
    mpd::{
        client::Client,
        commands::Song,
        mpd_client::{Filter, FilterKind, MpdClient, Tag},
    },
    shared::{keys::ActionEvent, mouse_event::MouseEvent, mpd_client_ext::Enqueue},
    ui::{
        UiEvent,
        browser::BrowserPane,
        dir_or_song::DirOrSong,
        dirstack::{DirStack, DirStackItem},
        input::InputResultEvent,
        widgets::browser::{Browser, BrowserArea},
    },
};

#[derive(Debug)]
pub struct DirectoriesPane {
    stack: DirStack<DirOrSong, ListState>,
    browser: Browser<DirOrSong>,
    initialized: bool,
    rspc: bool,
    root: Option<&'static str>,
    target: PaneType,
}

const INIT: &str = "init";
const FETCH_DATA: &str = "fetch_data";

impl DirectoriesPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            stack: DirStack::default(),
            browser: Browser::new(),
            initialized: false,
            rspc: ctx.supported_commands.contains("rspc_identity"),
            root: None,
            target: PaneType::Directories,
        }
    }

    pub fn liked_songs(ctx: &Ctx) -> Self {
        Self {
            root: Some("Liked Songs"),
            target: PaneType::LikedSongs,
            ..Self::new(ctx)
        }
    }

    fn request_root(&self, ctx: &Ctx) {
        let sort = ctx.config.directories_sort.clone();
        let playlist_display_mode = ctx.config.show_playlists_in_browser;
        let root = self.root;
        ctx.query()
            .id(INIT)
            .replace_id(INIT)
            .target(self.target.clone())
            .query(move |client| {
                let mut data = client
                    .lsinfo(root)?
                    .into_iter()
                    .filter_map(|value| value.into_dir_or_song(playlist_display_mode))
                    .collect::<Vec<_>>();
                if root.is_none() {
                    data.sort_by(|a, b| a.with_custom_sort(&sort).cmp(&b.with_custom_sort(&sort)));
                }
                Ok(MpdQueryResult::DirOrSong { data, path: None })
            });
    }

    fn request_path(&self, full_path: &str, next_path: &str) -> String {
        if self.rspc && !full_path.is_empty() {
            full_path.to_owned()
        } else {
            next_path.to_owned()
        }
    }
}

impl Pane for DirectoriesPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        self.restore_sort(ctx);
        self.browser
            .render(area, frame.buffer_mut(), &mut self.stack, ctx);

        Ok(())
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized {
            self.rspc = ctx.supported_commands.contains("rspc_identity");
            self.request_root(ctx);
            self.initialized = true;
        }

        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::Database => {
                self.request_root(ctx);
            }
            UiEvent::Reconnected => {
                self.initialized = false;
                self.before_show(ctx)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        self.handle_mouse_action(event, ctx)
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        BrowserPane::handle_insert_mode(self, kind, ctx)?;
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        self.handle_common_action(event, ctx)?;
        self.handle_global_action(event, ctx)?;
        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, data) {
            (FETCH_DATA, MpdQueryResult::DirOrSong { data, path }) => {
                let Some(path) = path else {
                    log::error!(path:?, current_path:? = self.stack().path(); "Cannot insert data because path is not provided");
                    return Ok(());
                };

                self.stack_mut().insert(path, data);
                self.fetch_data_internal(ctx)?;
                ctx.render()?;
            }
            (INIT, MpdQueryResult::DirOrSong { data, path: _ }) => {
                self.stack = DirStack::new(data);
                self.fetch_data_internal(ctx)?;
                ctx.render()?;
            }
            _ => {}
        }
        self.restore_sort(ctx);
        Ok(())
    }
}

impl BrowserPane<DirOrSong> for DirectoriesPane {
    fn sort_context(&self) -> Option<String> {
        if self.root.is_some() {
            return Some("library:liked".into());
        }
        match self.stack.path().as_slice() {
            [root, playlist] if root == "Playlists" => Some(format!("playlist:{playlist}")),
            [root] if root == "Liked Songs" => Some("library:liked".into()),
            _ => None,
        }
    }
    fn preview_sort_context(&self) -> Option<String> {
        let selected = self.stack.current().selected()?;
        match self.stack.path().as_slice() {
            [root] if root == "Playlists" => Some(format!("playlist:{}", selected.as_path())),
            [] if selected.as_path() == "Liked Songs" => Some("library:liked".into()),
            _ => None,
        }
    }
    fn stack(&self) -> &DirStack<DirOrSong, ListState> {
        &self.stack
    }

    fn stack_mut(&mut self) -> &mut DirStack<DirOrSong, ListState> {
        &mut self.stack
    }

    fn browser_areas(&self) -> EnumMap<BrowserArea, Rect> {
        self.browser.areas
    }

    fn cached_info(
        &self,
        item: &DirOrSong,
    ) -> Option<crate::ui::modals::info_list_modal::KeyValues> {
        if self.rspc
            && let DirOrSong::Dir { full_path, .. } = item
            && (matches!(
                full_path.as_str(),
                "Albums" | "Artists" | "Playlists" | "People" | "Local Playlists"
            ) || full_path.starts_with("People/profile/"))
        {
            return None;
        }
        self.cached_song_info(item)
    }

    fn info_target(&self, item: &DirOrSong) -> Option<(&'static str, String)> {
        match item {
            DirOrSong::Dir {
                name,
                full_path,
                playlist,
                ..
            } => Some((
                if *playlist { "playlist" } else { "path" },
                if full_path.is_empty() {
                    name.clone()
                } else {
                    full_path.clone()
                },
            )),
            DirOrSong::Song(_) => None,
        }
    }

    fn list_songs_in_item(
        &self,
        item: DirOrSong,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Clone + 'static {
        let rspc = self.rspc;
        move |client| {
            Ok(match item {
                DirOrSong::Dir {
                    full_path,
                    playlist: false,
                    ..
                } if rspc => client.selection_songs(&full_path)?,
                DirOrSong::Dir {
                    full_path,
                    playlist: false,
                    ..
                } => client.find(&[Filter::new_with_kind(
                    Tag::File,
                    &full_path,
                    FilterKind::StartsWith,
                )])?,
                DirOrSong::Dir {
                    name,
                    playlist: true,
                    ..
                } => client.list_playlist_info(&name, None)?,
                DirOrSong::Song(song) => vec![song.clone()],
            })
        }
    }

    fn fetch_data(&self, selected: &DirOrSong, ctx: &Ctx) -> Result<()> {
        match selected {
            DirOrSong::Dir {
                playlist: is_playlist,
                full_path,
                ..
            } => {
                let Some(next_path) = self.stack.next_path() else {
                    log::error!("Failed to move deeper inside dir. Next path is None");
                    return Ok(());
                };

                let is_playlist = *is_playlist;
                let request_path = self.request_path(full_path, &next_path.to_string());
                let playlist_display_mode = ctx.config.show_playlists_in_browser;

                let sort = ctx.config.directories_sort.clone();
                ctx.query()
                    .id(FETCH_DATA)
                    .replace_id("directories_data")
                    .target(self.target.clone())
                    .query(move |client| {
                        let data: Vec<_> = if is_playlist {
                            client
                                .list_playlist_info(&next_path.to_string(), None)?
                                .into_iter()
                                .map(DirOrSong::Song)
                                .collect()
                        } else {
                            match client.lsinfo(Some(&request_path)) {
                                Ok(val) => val,
                                Err(err) => {
                                    log::error!(error:? = err; "Failed to get lsinfo for dir");
                                    return Ok(MpdQueryResult::DirOrSong {
                                        data: Vec::new(),
                                        path: None,
                                    });
                                }
                            }
                            .0
                            .into_iter()
                            .filter_map(|v| v.into_dir_or_song(playlist_display_mode))
                            .sorted_by(|a, b| {
                                a.with_custom_sort(&sort).cmp(&b.with_custom_sort(&sort))
                            })
                            .collect()
                        };

                        Ok(MpdQueryResult::DirOrSong {
                            data,
                            path: Some(next_path),
                        })
                    });
            }
            DirOrSong::Song(_) => {}
        }
        Ok(())
    }

    fn enqueue<'a>(
        &self,
        items: impl Iterator<Item = &'a DirOrSong>,
    ) -> (Vec<Enqueue>, Option<usize>) {
        let mut dir_or_playlist_found = false;
        let items = items
            .map(|item| match item {
                DirOrSong::Dir {
                    full_path,
                    playlist: true,
                    ..
                } => {
                    dir_or_playlist_found = true;
                    Enqueue::Playlist {
                        name: full_path.to_owned(),
                    }
                }
                DirOrSong::Dir {
                    full_path,
                    playlist: false,
                    ..
                } => {
                    dir_or_playlist_found = true;
                    Enqueue::File {
                        path: full_path.to_owned(),
                    }
                }
                DirOrSong::Song(song) => Enqueue::File {
                    path: song.file.clone(),
                },
            })
            .collect_vec();

        let hovered_idx = if dir_or_playlist_found {
            None
        } else {
            // We are not adding any playlists or directories so autoplay on hovered item
            // can work
            if let Some(curr) = self.stack().current().selected() {
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, item)| {
                        if let Enqueue::File { path } = item {
                            Some((idx, path))
                        } else {
                            None
                        }
                    })
                    .find(|(_, path)| path == &&curr.dir_name_or_file())
                    .map(|(idx, _)| idx)
            } else {
                None
            }
        };

        (items, hovered_idx)
    }
}

#[cfg(test)]
mod rspc_tests {
    use super::*;
    use crate::tests::fixtures::ctx;
    use rstest::rstest;

    #[rstest]
    fn people_paths_are_special_only_on_rspc_connections(mut ctx: Ctx) {
        let opaque = "People/profile/user/Alice";
        let display = "People/Alice";
        assert_eq!(
            DirectoriesPane::new(&ctx).request_path(opaque, display),
            display
        );
        ctx.supported_commands.insert("rspc_identity".into());
        assert_eq!(
            DirectoriesPane::new(&ctx).request_path(opaque, display),
            opaque
        );
    }
}
