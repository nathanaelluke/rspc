use anyhow::Result;
use enum_map::EnumMap;
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::Style,
    widgets::{Block, Borders, Clear, ListState},
};

use super::Modal;
use crate::{
    MpdQueryResult,
    config::keys::CommonAction,
    ctx::Ctx,
    mpd::{
        client::Client,
        commands::{Song, lsinfo::LsInfoEntry},
        mpd_client::MpdClient,
    },
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::MouseEvent,
    },
    ui::{
        browser::BrowserPane,
        dir_or_song::DirOrSong,
        dirstack::{DirStack, Path},
        panes::Pane,
        widgets::browser::{Browser, BrowserArea},
    },
};

const FETCH: &str = "catalog_browser";

#[derive(Debug)]
pub struct CatalogModal {
    id: Id,
    title: String,
    stack: DirStack<DirOrSong, ListState>,
    browser: Browser<DirOrSong>,
    initialized: bool,
    response_key: String,
}

impl CatalogModal {
    pub fn new(path: String, title: String) -> Self {
        let root = DirOrSong::Dir {
            name: title.clone(),
            full_path: path,
            last_modified: chrono::Utc::now(),
            playlist: false,
        };
        let id = id::new();
        Self {
            id,
            title,
            stack: DirStack::new(vec![root]),
            browser: Browser::new(),
            initialized: false,
            response_key: format!("catalog:{id:?}"),
        }
    }
}

impl Modal for CatalogModal {
    fn id(&self) -> Id {
        self.id
    }
    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        if !self.initialized {
            self.fetch_data_internal(ctx)?;
            self.stack.enter();
            self.initialized = true;
        }
        let area = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        frame.render_widget(Clear, area);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(format!(" {} · Spotify catalog ", self.title));
        if let Some(bg) = ctx.config.theme.modal_background_color {
            block = block.style(Style::default().bg(bg));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.browser
            .render(inner, frame.buffer_mut(), &mut self.stack, ctx);
        Ok(())
    }
    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            if matches!(action, CommonAction::Close)
                || matches!(action, CommonAction::Left) && self.stack.path().as_slice().len() <= 1
            {
                return self.hide(ctx);
            }
            key.abandon();
        }
        self.handle_common_action(key, ctx)?;
        self.handle_global_action(key, ctx)
    }
    fn handle_insert_mode(
        &mut self,
        kind: crate::ui::input::InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        BrowserPane::handle_insert_mode(self, kind, ctx)
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        self.handle_mouse_action(event, ctx)
    }
    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: &mut MpdQueryResult,
        ctx: &Ctx,
    ) -> Result<()> {
        if id == FETCH
            && let MpdQueryResult::DirOrSong {
                data,
                path: Some(path),
            } = data
        {
            if path.current_dir() != Some(self.response_key.as_str()) {
                return Ok(());
            }
            path.pop();
            self.stack.insert(path.clone(), std::mem::take(data));
            self.fetch_data_internal(ctx)?;
            ctx.render()?;
        }
        Ok(())
    }
}

impl Pane for CatalogModal {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.browser
            .render(area, frame.buffer_mut(), &mut self.stack, ctx);
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        Modal::handle_key(self, event, ctx)
    }
}

impl BrowserPane<DirOrSong> for CatalogModal {
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
            DirOrSong::Dir { full_path, .. } => Some(("path", full_path.clone())),
            DirOrSong::Song(_) => None,
        }
    }

    fn list_songs_in_item(
        &self,
        item: DirOrSong,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Clone + 'static {
        move |client| match item {
            DirOrSong::Song(song) => Ok(vec![song]),
            DirOrSong::Dir { full_path, .. } => Ok(client
                .lsinfo(Some(&full_path))?
                .0
                .into_iter()
                .filter_map(|entry| match entry {
                    LsInfoEntry::File(song) => Some(song),
                    _ => None,
                })
                .collect()),
        }
    }
    fn fetch_data(&self, selected: &DirOrSong, ctx: &Ctx) -> Result<()> {
        let DirOrSong::Dir { full_path, .. } = selected else {
            return Ok(());
        };
        let full_path = full_path.clone();
        let path: Option<Path> = self.stack.next_path().map(|mut path| {
            path.push(self.response_key.clone());
            path
        });
        let mode = ctx.config.show_playlists_in_browser;
        // Nested catalog modals must not replace each other's queued requests.
        ctx.query().id(FETCH).query(move |client| {
            let data = client
                .lsinfo(Some(&full_path))?
                .0
                .into_iter()
                .filter_map(|entry| entry.into_dir_or_song(mode))
                .collect();
            Ok(MpdQueryResult::DirOrSong { data, path })
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::fixtures::ctx;
    use rstest::rstest;

    #[rstest]
    fn nested_catalogs_receive_only_their_own_responses(ctx: Ctx) {
        let mut first = CatalogModal::new("rspc://catalog/album/first".into(), "First".into());
        let mut second = CatalogModal::new("rspc://catalog/album/second".into(), "Second".into());
        first.stack.enter();
        second.stack.enter();
        let mut data = MpdQueryResult::DirOrSong {
            data: vec![DirOrSong::Song(Song {
                file: "spotify:track:fixture".into(),
                ..Default::default()
            })],
            path: Some(vec!["Second".into(), second.response_key.clone()].into()),
        };
        Modal::on_query_finished(&mut first, FETCH, &mut data, &ctx)
            .expect("ignore another modal's response");
        assert_eq!(first.stack.current().items.len(), 0);
        Modal::on_query_finished(&mut second, FETCH, &mut data, &ctx)
            .expect("accept this modal's response");
        assert_eq!(second.stack.current().items.len(), 1);
    }
}
