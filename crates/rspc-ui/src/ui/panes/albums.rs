use anyhow::{Context, Result};
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
        mpd_client::{Filter, MpdClient, Tag},
    },
    shared::{cmp::StringCompare, keys::ActionEvent, mouse_event::MouseEvent},
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
pub struct AlbumsPane {
    stack: DirStack<DirOrSong, ListState>,
    browser: Browser<DirOrSong>,
    initialized: bool,
    navigation: Option<String>,
}

const INIT: &str = "init";
const FETCH_DATA: &str = "fetch_data";

impl AlbumsPane {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            stack: DirStack::default(),
            browser: Browser::new(),
            initialized: false,
            navigation: None,
        }
    }

    pub fn navigate_to(&mut self, name: String, ctx: &Ctx) -> Result<()> {
        self.navigation = Some(name);
        self.initialized = false;
        self.before_show(ctx)
    }
}

impl Pane for AlbumsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.restore_sort(ctx);
        self.browser
            .render(area, frame.buffer_mut(), &mut self.stack, ctx);

        Ok(())
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized {
            ctx.query()
                .id(INIT)
                .replace_id(INIT)
                .target(PaneType::Albums)
                .query(move |client| {
                    let result = client
                        .list_tag(Tag::Album, None)
                        .context("Cannot list tags")?;
                    Ok(MpdQueryResult::LsInfo {
                        data: result.0,
                        path: None,
                    })
                });
            self.initialized = true;
        }

        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::Database => {
                self.navigation = self.stack.path().as_slice().first().cloned();
                ctx.query()
                    .id(INIT)
                    .replace_id(INIT)
                    .target(PaneType::Albums)
                    .query(move |client| {
                        let result = client
                            .list_tag(Tag::Album, None)
                            .context("Cannot list tags")?;
                        Ok(MpdQueryResult::LsInfo {
                            data: result.0,
                            path: None,
                        })
                    });
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
            (INIT, MpdQueryResult::LsInfo { data, path: _ }) => {
                let root = data
                    .into_iter()
                    .sorted_by(|a, b| {
                        StringCompare::from(ctx.config.browser_song_sort.as_ref()).compare(a, b)
                    })
                    .map(DirOrSong::name_only)
                    .collect_vec();
                self.stack = DirStack::new(root);
                if let Some(name) = self.navigation.take() {
                    if let Some(idx) = self
                        .stack
                        .current()
                        .items
                        .iter()
                        .position(|item| item.as_path() == name)
                    {
                        self.stack
                            .current_mut()
                            .select_idx(idx, ctx.config.scrolloff);
                        if let Some(item) = self.stack.current().selected() {
                            self.fetch_data(item, ctx)?;
                        }
                        self.stack.enter();
                    } else {
                        crate::shared::macros::status_warn!("Album is not in your library: {name}");
                    }
                }
                self.fetch_data_internal(ctx)?;
                ctx.render()?;
            }
            _ => {}
        }
        self.restore_sort(ctx);
        Ok(())
    }
}

impl BrowserPane<DirOrSong> for AlbumsPane {
    fn sort_context(&self) -> Option<String> {
        match self.stack.path().as_slice() {
            [album] => Some(format!("album:{album}")),
            _ => None,
        }
    }

    fn preview_sort_context(&self) -> Option<String> {
        self.stack
            .path()
            .is_empty()
            .then(|| self.stack.current().selected())
            .flatten()
            .map(|item| format!("album:{}", item.as_path()))
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

    fn info_target(&self, item: &DirOrSong) -> Option<(&'static str, String)> {
        match item {
            DirOrSong::Dir { name, .. } => Some(("album", name.clone())),
            DirOrSong::Song(_) => None,
        }
    }

    fn list_songs_in_item(
        &self,
        item: DirOrSong,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Clone + 'static {
        move |client| match item {
            DirOrSong::Dir { name, .. } => Ok(client.find(&[Filter::new(Tag::Album, &name)])?),
            DirOrSong::Song(song) => Ok(vec![song.clone()]),
        }
    }

    fn fetch_data(&self, selected: &DirOrSong, ctx: &Ctx) -> Result<()> {
        let current = selected.as_path().to_owned();
        let Some(path) = self.stack().next_path() else {
            log::error!(stack:? = self.stack; "Cannot fetch data because next path is not available");
            return Ok(());
        };

        match self.stack.path().as_slice() {
            [_album] => {}
            [] => {
                let sort_order = ctx.config.browser_song_sort.clone();
                ctx.query()
                    .id(FETCH_DATA)
                    .replace_id("albums_data")
                    .target(PaneType::Albums)
                    .query(move |client| {
                        let data = client
                            .find(&[Filter::new(Tag::Album, current)])?
                            .into_iter()
                            .sorted_by(|a, b| {
                                a.with_custom_sort(&sort_order)
                                    .cmp(&b.with_custom_sort(&sort_order))
                            })
                            .map(DirOrSong::Song)
                            .collect();
                        Ok(MpdQueryResult::DirOrSong {
                            data,
                            path: Some(path),
                        })
                    });
            }

            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::sort_mode::{SongSort, SongSortKey},
        tests::fixtures::ctx,
        ui::dirstack::Path,
    };
    use rstest::rstest;

    #[rstest]
    fn album_song_sort_is_remembered_without_sorting_album_names(ctx: Ctx) {
        let mut pane = AlbumsPane::new(&ctx);
        let mut song = Song {
            file: "new".into(),
            added: chrono::DateTime::from_timestamp(20, 0),
            ..Default::default()
        };
        song.metadata.insert("title".into(), "New".into());
        let mut old = song.clone();
        old.file = "old".into();
        old.added = chrono::DateTime::from_timestamp(10, 0);
        pane.stack = DirStack::new(vec![
            DirOrSong::name_only("Mix".into()),
            DirOrSong::name_only("Other".into()),
        ]);
        pane.stack.insert(
            Path::new().join("Mix"),
            vec![DirOrSong::Song(old), DirOrSong::Song(song)],
        );
        assert!(
            pane.sort_context().is_none(),
            "album list must keep its own ordering"
        );
        pane.stack.enter();
        pane.change_sort(Some(SongSortKey::Recent), &ctx)
            .expect("sort album songs");
        assert_eq!(pane.stack.current().items[0].as_path(), "new");
        assert_eq!(
            ctx.collection_sorts.borrow().get("album:Mix"),
            SongSort::new(SongSortKey::Recent)
        );
        assert_eq!(
            ctx.collection_sorts.borrow().get("playlist:Mix"),
            SongSort::default()
        );
        pane.stack.leave();
        pane.restore_sort(&ctx);
        assert_eq!(pane.stack.current().items[0].as_path(), "Mix");
        assert_eq!(
            pane.stack.next().expect("preview").items[0].as_path(),
            "new"
        );
    }
}
