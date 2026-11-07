use std::{collections::HashSet, time::Duration};

use anyhow::Result;
use bon::bon;
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    macros::constraint,
    style::Style,
    symbols::border,
    widgets::{Block, Borders, Cell, Clear, Row, Table, TableState},
};

use super::Modal;
use crate::{
    MpdQueryResult,
    config::keys::CommonAction,
    ctx::Ctx,
    mpd::{client::Client, commands::Song, errors::MpdError},
    shared::{
        ext::duration::DurationExt,
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::dirstack::DirState,
};

type InfoRequest = Box<dyn FnOnce(&mut Client<'_>) -> Result<KeyValues> + Send + Sync>;

#[derive(derive_more::Debug)]
pub struct InfoListModal {
    id: Id,
    scrolling_state: DirState<TableState>,
    table_area: Rect,
    items: KeyValues,
    column_widths: &'static [u16],
    title: &'static str,
    size: (u16, u16),
    row_items: Vec<usize>,
    wrapped_rows: Vec<[String; 2]>,
    wrap_widths: Option<(u16, u16)>,
    #[debug(skip)]
    pending: Option<InfoRequest>,
    loading: bool,
    min_rows: usize,
}

#[derive(Debug)]
pub struct KeyValues {
    entries: Vec<KeyValue>,
    song_file: Option<String>,
}
#[derive(Debug)]
struct KeyValue {
    key: String,
    value: String,
}

#[bon]
impl InfoListModal {
    pub fn for_song(song: &Song) -> Self {
        Self::builder()
            .items(song)
            .title("Song info")
            .column_widths(&[30, 70])
            .build()
    }

    #[builder]
    pub fn new(
        items: impl Into<KeyValues>,
        title: &'static str,
        column_widths: &'static [u16],
        size: Option<(u16, u16)>,
    ) -> Self {
        let mut scrolling_state = DirState::default();
        scrolling_state.select(Some(0), 0);
        Self {
            id: id::new(),
            scrolling_state,
            items: items.into(),
            table_area: Rect::default(),
            title,
            column_widths,
            size: size.unwrap_or((80, 80)),
            row_items: Vec::new(),
            wrapped_rows: Vec::new(),
            wrap_widths: None,
            pending: None,
            loading: false,
            min_rows: 0,
        }
    }

    fn navigate_selected(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(item) = self
            .scrolling_state
            .get_selected()
            .and_then(|row| self.row_items.get(row))
            .and_then(|idx| self.items.entries.get(*idx))
        else {
            return Ok(());
        };
        let pane = match item.key.to_ascii_lowercase().as_str() {
            "artist" => crate::config::tabs::PaneType::Artists,
            "albumartist" => crate::config::tabs::PaneType::AlbumArtists,
            "album" => crate::config::tabs::PaneType::Albums,
            _ => return Ok(()),
        };
        let value = item.value.clone();
        if let Some(id) = self
            .items
            .song_file
            .as_deref()
            .filter(|_| ctx.supported_commands.contains("rspc_identity"))
            .and_then(|file| file.strip_prefix("spotify:track:"))
        {
            let path = if matches!(pane, crate::config::tabs::PaneType::Albums) {
                format!("rspc://catalog/album/{id}")
            } else {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
                format!("rspc://catalog/artist/{id}/{encoded}")
            };
            // Keep a modal open throughout the transition so album art stays
            // hidden while the catalog replaces song information.
            ctx.app_event_sender
                .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(
                    Box::new(super::catalog::CatalogModal::new(path, value)),
                )))?;
            self.hide(ctx)?;
            return Ok(());
        }
        self.hide(ctx)?;
        ctx.app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::BrowseTag {
                pane,
                value,
            }))?;
        Ok(())
    }

    pub fn for_selection(items: impl Into<KeyValues>) -> Self {
        Self::builder()
            .items(items)
            .title("Selection info")
            .column_widths(&[30, 70])
            .build()
    }

    pub fn loading(
        request: impl FnOnce(&mut Client<'_>) -> Result<KeyValues> + Send + Sync + 'static,
    ) -> Self {
        let mut modal = Self::for_selection(vec![("Status".into(), "Loading selection…".into())]);
        modal.pending = Some(Box::new(request));
        modal.loading = true;
        // Album/folder summaries normally contain five rows. Reserve them
        // before the query finishes so the popup does not move under the user.
        modal.min_rows = 5;
        modal
    }

    fn wrap_rows(&mut self, widths: (u16, u16)) {
        if self.wrap_widths == Some(widths) {
            return;
        }
        self.row_items.clear();
        self.wrapped_rows.clear();
        for (idx, item) in self.items.entries.iter().enumerate() {
            let keys = textwrap::wrap(&item.key, usize::from(widths.0.max(1)));
            let values = textwrap::wrap(&item.value, usize::from(widths.1.max(1)));
            for row in keys.into_iter().zip_longest(values) {
                let (key, value) = match row {
                    itertools::EitherOrBoth::Both(key, value) => {
                        (key.into_owned(), value.into_owned())
                    }
                    itertools::EitherOrBoth::Left(key) => (key.into_owned(), String::new()),
                    itertools::EitherOrBoth::Right(value) => (String::new(), value.into_owned()),
                };
                self.wrapped_rows.push([key, value]);
                self.row_items.push(idx);
            }
        }
        self.wrap_widths = Some(widths);
    }
}

impl Modal for InfoListModal {
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        if let Some(request) = self.pending.take() {
            let modal = self.id;
            ctx.query().id("selection_info").query(move |client| {
                let items = match request(client) {
                    Ok(items) => items,
                    Err(error)
                        if matches!(
                            error.downcast_ref::<MpdError>(),
                            Some(MpdError::TimedOut(_))
                        ) =>
                    {
                        return Err(error);
                    }
                    Err(error) => vec![("Error".into(), error.to_string())].into(),
                };
                Ok(MpdQueryResult::SelectionInfo { modal, items })
            });
        }
        let (w, h) = self.size;
        let max_area = frame.area().centered(constraint!(==w%), constraint!(==h%));
        let column_constraints = self
            .column_widths
            .iter()
            .map(|w| Constraint::Percentage(*w))
            .collect_vec();
        let content = max_area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let columns = Layout::horizontal(&column_constraints)
            .spacing(2)
            .split(content);
        self.wrap_rows((columns[0].width, columns[1].width));
        let height = max_area.height.min(
            self.wrapped_rows
                .len()
                .max(self.min_rows)
                .saturating_add(4)
                .min(u16::MAX as usize) as u16,
        );
        let popup_area = frame.area().centered(
            Constraint::Length(max_area.width),
            Constraint::Length(height),
        );
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(
                Block::default().style(Style::default().bg(bg_color)),
                popup_area,
            );
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::PLAIN)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title);

        let margin = Margin {
            horizontal: 1,
            vertical: 0,
        };
        let [header_area, table_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Percentage(100)])
                .areas(block.inner(popup_area));
        let header_area = header_area.inner(margin);
        let table_area = table_area.inner(margin);

        self.scrolling_state
            .set_content_and_viewport_len(self.wrapped_rows.len(), table_area.height.into());
        let rows = self
            .wrapped_rows
            .iter()
            .map(|[key, value]| Row::new([key.as_str(), value.as_str()]));

        let header_table = Table::new(
            vec![Row::new([Cell::from("Tag"), Cell::from("Value")])],
            &column_constraints,
        )
        .column_spacing(2)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(ctx.config.as_border_style()),
        );
        let table = Table::new(rows, &column_constraints)
            .column_spacing(2)
            .style(ctx.config.as_text_style())
            .row_highlight_style(ctx.config.theme.current_item_style);

        self.table_area = table_area;

        frame.render_widget(block, popup_area);
        frame.render_widget(header_table, header_area);
        frame.render_stateful_widget(
            table,
            table_area,
            self.scrolling_state.as_render_state_ref(),
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame.render_stateful_widget(
                scrollbar,
                popup_area.inner(Margin {
                    horizontal: 0,
                    vertical: 1,
                }),
                self.scrolling_state.as_scrollbar_state_ref(),
            );
        }

        return Ok(());
    }

    fn on_query_finished(
        &mut self,
        _id: &'static str,
        data: &mut MpdQueryResult,
        ctx: &Ctx,
    ) -> Result<()> {
        if let MpdQueryResult::SelectionInfo { modal, items } = data
            && *modal == self.id
        {
            std::mem::swap(&mut self.items, items);
            self.loading = false;
            self.wrap_widths = None;
            self.scrolling_state = DirState::default();
            self.scrolling_state.select(Some(0), 0);
            ctx.render()?;
        }
        Ok(())
    }

    fn on_event(&mut self, event: &mut crate::ui::UiEvent, ctx: &Ctx) -> Result<()> {
        if self.loading && matches!(event, crate::ui::UiEvent::Reconnected) {
            self.items = vec![(
                "Status".into(),
                "Connection restored. Reopen selection info to retry.".into(),
            )]
            .into();
            self.loading = false;
            self.wrap_widths = None;
            ctx.render()?;
        }
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::DownHalf => {
                    self.scrolling_state
                        .next_half_viewport(ctx.config.scrolloff);

                    ctx.render()?;
                }
                CommonAction::UpHalf => {
                    self.scrolling_state
                        .prev_half_viewport(ctx.config.scrolloff);

                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.scrolling_state
                        .prev(ctx.config.scrolloff, ctx.config.wrap_navigation);

                    ctx.render()?;
                }
                CommonAction::Down => {
                    self.scrolling_state
                        .next(ctx.config.scrolloff, ctx.config.wrap_navigation);

                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    self.scrolling_state.last();

                    ctx.render()?;
                }
                CommonAction::Top => {
                    self.scrolling_state.first();

                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                }
                CommonAction::Confirm | CommonAction::Right => self.navigate_selected(ctx)?,
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.table_area.contains(event.into()) {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                let y: usize = event.y.saturating_sub(self.table_area.y).into();
                if let Some(idx) = self.scrolling_state.get_at_rendered_row(y) {
                    self.scrolling_state.select(Some(idx), ctx.config.scrolloff);
                    if matches!(event.kind, MouseEventKind::DoubleClick) {
                        self.navigate_selected(ctx)?;
                    }
                    ctx.render()?;
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollDown => {
                self.scrolling_state
                    .scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::ScrollUp => {
                self.scrolling_state
                    .scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::Drag {
                drag_start_position: _,
            } => {}
        }

        Ok(())
    }
}

impl KeyValues {
    pub fn from_songs<'a>(songs: impl IntoIterator<Item = &'a Song>) -> Self {
        let mut count = 0;
        let mut duration = Duration::ZERO;
        let mut artists = HashSet::new();
        let mut albums = HashSet::new();
        let mut genres = HashSet::new();
        for song in songs {
            count += 1;
            duration += song.duration.unwrap_or_default();
            for (tag, values) in [
                ("artist", &mut artists),
                ("album", &mut albums),
                ("genre", &mut genres),
            ] {
                if let Some(tags) = song.metadata.get(tag) {
                    values.extend(tags.iter());
                }
            }
        }
        vec![
            ("Songs".into(), count.to_string()),
            ("Total duration".into(), duration.to_string()),
            ("Artists".into(), artists.len().to_string()),
            ("Albums".into(), albums.len().to_string()),
            ("Genres".into(), genres.len().to_string()),
        ]
        .into()
    }
}

impl From<Vec<Song>> for KeyValues {
    fn from(songs: Vec<Song>) -> Self {
        Self::from_songs(&songs)
    }
}

impl From<Vec<(String, String)>> for KeyValues {
    fn from(items: Vec<(String, String)>) -> Self {
        Self {
            entries: items
                .into_iter()
                .map(|(key, value)| KeyValue { key, value })
                .collect(),
            song_file: None,
        }
    }
}

impl From<&Song> for KeyValues {
    fn from(song: &Song) -> Self {
        let mut result = Vec::new();
        result.push(KeyValue {
            key: "File".to_owned(),
            value: song.file.clone(),
        });
        let file_name = song.file_name().unwrap_or_default();
        if !file_name.is_empty() {
            result.push(KeyValue {
                key: "Filename".to_owned(),
                value: file_name.into_owned(),
            });
        }

        if let Some(title) = song.metadata.get("title") {
            result.extend(title.iter().map(|item| KeyValue {
                key: "Title".to_owned(),
                value: item.to_owned(),
            }));
        }

        if let Some(artist) = song.metadata.get("artist") {
            result.extend(artist.iter().map(|item| KeyValue {
                key: "Artist".to_owned(),
                value: item.to_owned(),
            }));
        }

        if let Some(album) = song.metadata.get("album") {
            result.extend(album.iter().map(|item| KeyValue {
                key: "Album".to_owned(),
                value: item.to_owned(),
            }));
        }

        let duration = song
            .duration
            .as_ref()
            .map(DurationExt::to_string)
            .unwrap_or_default();
        if !duration.is_empty() {
            result.push(KeyValue {
                key: "Duration".to_owned(),
                value: duration,
            });
        }

        result.extend(
            song.metadata
                .iter()
                .filter(|(key, _)| {
                    !["title", "album", "artist", "duration"].contains(&(*key).as_str())
                })
                .flat_map(|(k, v)| {
                    v.iter().map(|item| KeyValue {
                        key: k.to_owned(),
                        value: item.to_owned(),
                    })
                }),
        );

        KeyValues {
            entries: result,
            song_file: Some(song.file.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppEvent,
        config::tabs::PaneType,
        tests::fixtures::{app_event_channel, ctx},
    };
    use crossbeam::channel::{Receiver, Sender};
    use ratatui::{Terminal, backend::TestBackend};
    use rstest::rstest;

    #[rstest]
    fn a_wrapped_artist_row_opens_the_complete_artist_name(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        mut ctx: Ctx,
    ) {
        ctx.app_event_sender = app_event_channel.0.clone();
        let name = "A performer with a name long enough to wrap across several rows";
        let mut modal = InfoListModal::builder()
            .items(KeyValues {
                entries: vec![KeyValue {
                    key: "Artist".into(),
                    value: name.into(),
                }],
                song_file: None,
            })
            .title("Song information")
            .column_widths(&[30, 70])
            .build();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                modal
                    .render(frame, &mut ctx)
                    .expect("render song information");
            })
            .expect("draw terminal");
        assert!(modal.row_items.len() > 1);
        modal.scrolling_state.select(Some(1), 0);
        modal
            .navigate_selected(&mut ctx)
            .expect("navigate to artist");
        assert!(app_event_channel.1.try_iter().any(|event|matches!(event,
            AppEvent::UiEvent(crate::ui::UiAppEvent::BrowseTag {pane:PaneType::Artists,value}) if value==name
        )));
    }

    #[rstest]
    #[case::rspc(true)]
    #[case::mpd(false)]
    fn spotify_album_links_follow_the_server_capabilities(
        #[case] rspc: bool,
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        mut ctx: Ctx,
    ) {
        ctx.app_event_sender = app_event_channel.0.clone();
        if rspc {
            ctx.supported_commands.insert("rspc_identity".into());
        }
        let mut modal = InfoListModal::builder()
            .items(KeyValues {
                entries: vec![KeyValue {
                    key: "Album".into(),
                    value: "Full Album".into(),
                }],
                song_file: Some("spotify:track:fixture0".into()),
            })
            .title("Song information")
            .column_widths(&[30, 70])
            .build();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                modal
                    .render(frame, &mut ctx)
                    .expect("render song information");
            })
            .expect("draw terminal");
        modal
            .navigate_selected(&mut ctx)
            .expect("navigate to album");
        let events = app_event_channel.1.try_iter().collect_vec();
        if rspc {
            assert!(matches!(
                events.first(),
                Some(AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))
            ));
            assert!(
                matches!(events.get(1), Some(AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id))) if *id == modal.id)
            );
        }
        assert!(events.into_iter().any(|event| match event {
            AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(modal)) if rspc =>
                format!("{modal:?}").contains("rspc://catalog/album/fixture0"),
            AppEvent::UiEvent(crate::ui::UiAppEvent::BrowseTag {
                pane: PaneType::Albums,
                value,
            }) if !rspc => value == "Full Album",
            _ => false,
        }));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::tests::fixtures::ctx;
    use rstest::rstest;

    #[rstest]
    fn loading_album_summary_keeps_its_position_when_data_arrives(mut ctx: Ctx) {
        let mut modal = InfoListModal::loading(|_| unreachable!("fixture supplies its result"));
        modal.pending = None;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).expect("terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("loading"))
            .expect("draw");
        let area = modal.table_area;
        let mut result = MpdQueryResult::SelectionInfo {
            modal: modal.id,
            items: vec![
                ("Songs".into(), "29".into()),
                ("Total duration".into(), "1:27:31".into()),
                ("Artists".into(), "4".into()),
                ("Albums".into(), "1".into()),
                ("Genres".into(), "0".into()),
            ]
            .into(),
        };
        modal
            .on_query_finished("selection_info", &mut result, &ctx)
            .expect("result");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("complete"))
            .expect("draw");
        assert_eq!(modal.table_area, area);
        assert_eq!(modal.row_items, [0, 1, 2, 3, 4]);
        assert!(
            !terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol().contains('…'))
        );
    }

    #[rstest]
    fn short_summaries_fit_their_rows_and_keep_shared_gutters(mut ctx: Ctx) {
        let mut modal = InfoListModal::for_selection(vec![
            ("Songs".into(), "12".into()),
            ("Total duration".into(), "42:00".into()),
        ]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).expect("terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("summary"))
            .expect("draw");
        assert_eq!(modal.table_area.height, 2);
        assert_eq!(modal.row_items, [0, 1]);
        let cached = modal.wrapped_rows.as_ptr();
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("summary"))
            .expect("redraw");
        assert_eq!(modal.wrapped_rows.as_ptr(), cached);
    }

    #[rstest]
    fn async_results_only_update_the_modal_that_requested_them(mut ctx: Ctx) {
        let mut first = InfoListModal::loading(|_| Ok(Vec::<Song>::new().into()));
        let mut second = InfoListModal::loading(|_| Ok(Vec::<Song>::new().into()));
        let mut result = MpdQueryResult::SelectionInfo {
            modal: first.id,
            items: vec![("Songs".into(), "42".into())].into(),
        };
        second
            .on_query_finished("selection_info", &mut result, &ctx)
            .expect("ignore old result");
        assert!(second.loading);
        assert!(second.items.entries[0].value.contains("Loading"));
        first
            .on_query_finished("selection_info", &mut result, &ctx)
            .expect("accept own result");
        assert!(!first.loading);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).expect("terminal");
        // The query was never submitted in this test; prevent starting it during render.
        first.pending = None;
        terminal
            .draw(|frame| {
                first
                    .render(frame, &mut ctx)
                    .expect("render completed summary");
            })
            .expect("draw");
        assert_eq!(first.items.entries[0].value, "42");
    }
}
