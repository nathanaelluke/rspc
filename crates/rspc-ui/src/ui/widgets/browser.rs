use enum_map::{Enum, EnumMap};
use itertools::Itertools;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Padding},
};
use std::{collections::HashMap, sync::Arc};

use crate::{
    config::Config,
    ctx::Ctx,
    mpd::commands::Song,
    ui::dirstack::{Dir, DirStack, DirStackItem},
};

#[derive(Copy, Clone, Debug, Enum, Eq, PartialEq, Hash)]
pub enum BrowserArea {
    Previous = 0,
    Current = 1,
    Preview = 2,
    Scrollbar = 3,
}

#[derive(Debug)]
pub struct Browser<T: std::fmt::Debug + DirStackItem + Clone + Send> {
    state_type_marker: std::marker::PhantomData<T>,
    pub areas: EnumMap<BrowserArea, Rect>,
    file_preview: Option<FilePreview>,
}

impl<T: std::fmt::Debug + DirStackItem + Clone + Send> Browser<T> {
    pub fn new() -> Self {
        Self {
            state_type_marker: std::marker::PhantomData,
            areas: EnumMap::default(),
            file_preview: None,
        }
    }
}
#[derive(Debug)]
struct FilePreview {
    song: Song,
    config: Arc<Config>,
    stickers: Option<HashMap<String, String>>,
    list: List<'static>,
}

fn file_preview<T: DirStackItem>(item: &T, ctx: &Ctx) -> List<'static> {
    let mut rows = Vec::new();
    for group in item.to_file_preview(ctx) {
        if let Some(name) = group.name {
            let mut row = ListItem::new(name);
            if let Some(style) = group.header_style {
                row = row.style(style);
            }
            rows.push(row);
        }
        rows.extend(group.items);
        rows.push(ListItem::new(""));
    }
    List::new(rows).style(ctx.config.as_text_style())
}

const MIDDLE_COLUMN_SYMBOLS: symbols::border::Set = symbols::border::Set {
    top_right: symbols::line::NORMAL.horizontal_down,
    bottom_right: symbols::line::NORMAL.horizontal_up,
    ..symbols::border::PLAIN
};

const LEFT_COLUMN_SYMBOLS: symbols::border::Set = symbols::border::Set {
    bottom_right: symbols::line::NORMAL.horizontal_up,
    top_right: symbols::line::NORMAL.horizontal_down,
    ..symbols::border::PLAIN
};

impl<T> Browser<T>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
{
    pub fn render(
        &mut self,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
        state: &mut DirStack<T, ListState>,
        ctx: &Ctx,
    ) {
        let config = &ctx.config;
        let scrollbar_margin = match config.theme.scrollbar.as_ref() {
            Some(scrollbar) if config.theme.draw_borders => {
                let scrollbar_track = &scrollbar.symbols[0];
                Margin {
                    vertical: 0,
                    horizontal: scrollbar_track.is_empty().into(),
                }
            }
            Some(_) | None => Margin {
                vertical: 0,
                horizontal: 0,
            },
        };
        let column_right_padding = 1;

        let [previous_area, current_area, preview_area] = *Layout::horizontal([
            Constraint::Percentage(config.theme.column_widths[0]),
            Constraint::Percentage(config.theme.column_widths[1]),
            Constraint::Percentage(config.theme.column_widths[2]),
        ])
        .split(area) else {
            return;
        };

        let preview_area = preview_area.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        self.areas[BrowserArea::Preview] = preview_area;
        if config.theme.column_widths[2] > 0 {
            if let Some(current) = state.current().selected()
                && current.is_file()
            {
                if let Some(song) = current.as_song() {
                    let stickers = ctx.song_stickers_if_supported(&song.file);
                    let needs_update = self.file_preview.as_ref().is_none_or(|preview| {
                        preview.song != *song
                            || !Arc::ptr_eq(&preview.config, &ctx.config)
                            || preview.stickers.as_ref() != stickers
                    });
                    if needs_update {
                        self.file_preview = Some(FilePreview {
                            song: song.clone(),
                            config: Arc::clone(&ctx.config),
                            stickers: stickers.cloned(),
                            list: file_preview(current, ctx),
                        });
                    }
                    if let Some(preview) = &self.file_preview {
                        Widget::render(&preview.list, preview_area, buf);
                    }
                } else {
                    Widget::render(file_preview(current, ctx), preview_area, buf);
                }
            } else {
                let items = state.next_dir_items().map_or(Vec::new(), |items| {
                    items
                        .iter()
                        .take(usize::from(preview_area.height))
                        .map(|item| item.to_list_item_simple(ctx))
                        .collect_vec()
                });
                if let Some(next) = state.next_mut() {
                    next.state
                        .set_content_and_viewport_len(next.items.len(), preview_area.height.into());
                }
                Widget::render(
                    List::new(items).style(config.as_text_style()),
                    preview_area,
                    buf,
                );
            }
        }

        if let Some(previous) = state.previous_mut()
            && config.theme.column_widths[0] > 0
        {
            let title = previous.filter_text(previous_area.width.saturating_sub(2), ctx);
            let mut block = if config.theme.draw_borders {
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(config.as_border_style())
                    .padding(Padding::new(1, column_right_padding, 0, 0))
                    .border_set(LEFT_COLUMN_SYMBOLS)
            } else {
                Block::default().padding(Padding::new(1, column_right_padding, 0, 0))
            };
            if let Some(title) = title {
                block = block.title(title);
            }

            let inner_block = block.inner(previous_area);
            self.areas[BrowserArea::Previous] = inner_block;
            render_directory(previous, inner_block, buf, ctx);
            ratatui::widgets::Widget::render(block, previous_area, buf);
            if let Some(scrollbar) = config.as_styled_scrollbar()
                && previous.state.content_len().is_some_and(|l| l > 0)
            {
                ratatui::widgets::StatefulWidget::render(
                    scrollbar,
                    previous_area.inner(scrollbar_margin),
                    buf,
                    previous.state.as_scrollbar_state_ref(),
                );
            }
        }
        if config.theme.column_widths[1] > 0 {
            let title = state
                .current()
                .filter_text(current_area.width.saturating_sub(2), ctx);

            let block = {
                let mut b = Block::default();
                if config.theme.draw_borders {
                    b = b
                        .borders(Borders::RIGHT)
                        .border_style(config.as_border_style())
                        .border_set(MIDDLE_COLUMN_SYMBOLS);
                }
                if let Some(title) = title {
                    b = b.title(title);
                }
                b.padding(Padding::new(1, column_right_padding, 0, 0))
            };
            let inner_block = block.inner(current_area);
            render_directory(state.current_mut(), inner_block, buf, ctx);
            self.areas[BrowserArea::Current] = inner_block;
            let scrollbar_area = current_area.inner(scrollbar_margin);
            self.areas[BrowserArea::Scrollbar] = scrollbar_area;
            ratatui::widgets::Widget::render(block, current_area, buf);
            if let Some(scrollbar) = config.as_styled_scrollbar() {
                ratatui::widgets::StatefulWidget::render(
                    scrollbar,
                    scrollbar_area,
                    buf,
                    state.current_mut().state.as_scrollbar_state_ref(),
                );
            }
        }
    }
}

/// Browser rows are single lines. Only format the visible slice, while keeping
/// directory selection and mouse offsets in coordinates of the full list.
pub(crate) fn render_directory<T>(
    directory: &mut Dir<T, ListState>,
    area: Rect,
    buffer: &mut Buffer,
    ctx: &Ctx,
) where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
{
    directory
        .state
        .set_content_and_viewport_len(directory.items.len(), usize::from(area.height));
    directory
        .state
        .select(directory.state.get_selected(), ctx.config.scrolloff);
    let selected = directory.state.get_selected();
    let offset = directory.state.offset();
    let items = directory.to_list_items_range(
        offset..offset.saturating_add(usize::from(area.height)),
        ctx.config.theme.browser_song_format.0.as_slice(),
        ctx,
    );
    let mut visible =
        ListState::default().with_selected(selected.map(|index| index.saturating_sub(offset)));
    StatefulWidget::render(
        List::new(items)
            .style(ctx.config.as_text_style())
            .highlight_style(ctx.config.theme.current_item_style),
        area,
        buffer,
        &mut visible,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::theme::properties::{Property, SongProperty},
        tests::fixtures::ctx,
    };
    use rstest::rstest;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug, Clone)]
    struct Item {
        name: String,
        formatted: Arc<AtomicUsize>,
    }

    impl DirStackItem for Item {
        fn as_path(&self) -> &str {
            &self.name
        }
        fn is_file(&self) -> bool {
            false
        }
        fn to_file_preview(&self, _: &Ctx) -> Vec<crate::shared::mpd_query::PreviewGroup> {
            Vec::new()
        }
        fn matches(&self, _: &[Property<SongProperty>], _: &Ctx, filter: &str) -> bool {
            self.name.contains(filter)
        }
        fn to_list_item<'a>(
            &self,
            _: &Ctx,
            marked: bool,
            _: bool,
            suffix: Option<String>,
        ) -> ListItem<'a> {
            self.formatted.fetch_add(1, Ordering::Relaxed);
            ListItem::new(format!(
                "{}{}{}",
                if marked { "*" } else { " " },
                self.name,
                suffix.unwrap_or_default()
            ))
        }
    }

    #[rstest]
    #[case(0, false)]
    #[case(505, false)]
    #[case(9999, false)]
    #[case(505, true)]
    fn visible_rows_match_the_full_list_without_formatting_offscreen_items(
        #[case] selected: usize,
        #[case] filtered: bool,
        ctx: Ctx,
    ) {
        let formatted = Arc::new(AtomicUsize::new(0));
        let mut dir = Dir::new(
            (0..10_000)
                .map(|index| Item {
                    name: format!("Item {index:05}"),
                    formatted: Arc::clone(&formatted),
                })
                .collect(),
        );
        let area = Rect::new(2, 1, 50, 16);
        dir.state
            .set_content_and_viewport_len(dir.len(), area.height.into());
        dir.state.select(Some(selected), ctx.config.scrolloff);
        dir.state.marked.insert(selected);
        if filtered {
            ctx.input.create_buffer(dir.filter_buffer_id, Some("5"));
            dir.set_filter_active(true);
            dir.recalculate_matched_items(ctx.config.theme.browser_song_format.0.as_slice(), &ctx);
        }
        let mut actual = Buffer::empty(Rect::new(0, 0, 54, 20));
        render_directory(&mut dir, area, &mut actual, &ctx);
        assert_eq!(formatted.load(Ordering::Relaxed), usize::from(area.height));
        let mut expected = Buffer::empty(*actual.area());
        let mut state = dir.state.inner;
        StatefulWidget::render(
            List::new(dir.to_list_items(ctx.config.theme.browser_song_format.0.as_slice(), &ctx))
                .style(ctx.config.as_text_style())
                .highlight_style(ctx.config.theme.current_item_style),
            area,
            &mut expected,
            &mut state,
        );
        assert_eq!(actual, expected);
        assert_eq!(dir.state.offset(), state.offset());
        assert_eq!(dir.state.get_selected(), state.selected());
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::{tests::fixtures::ctx, ui::dir_or_song::DirOrSong};
    use rstest::rstest;

    #[rstest]
    fn unchanged_preview_reuses_its_widget_and_changes_invalidate_it(mut ctx: Ctx) {
        let area = Rect::new(0, 0, 120, 24);
        let mut buffer = Buffer::empty(area);
        let mut stack = DirStack::new(vec![DirOrSong::Song(Song {
            file: "track.flac".into(),
            metadata: HashMap::from([("title".into(), "First title".into())]),
            ..Song::default()
        })]);
        let mut browser = Browser::new();
        browser.render(area, &mut buffer, &mut stack, &ctx);
        let preview = browser
            .file_preview
            .as_ref()
            .expect("prepared preview")
            .list
            .clone();
        let song_allocation = browser
            .file_preview
            .as_ref()
            .expect("preview")
            .song
            .file
            .as_ptr();
        let first = buffer.clone();
        buffer.reset();
        browser.render(area, &mut buffer, &mut stack, &ctx);
        assert_eq!(buffer, first, "caching must preserve every rendered cell");
        assert_eq!(
            browser
                .file_preview
                .as_ref()
                .expect("preview")
                .song
                .file
                .as_ptr(),
            song_allocation
        );
        assert_eq!(
            browser.file_preview.as_ref().expect("preview").list,
            preview
        );
        if let DirOrSong::Song(song) = &mut stack.current_mut().items[0] {
            song.metadata.insert("title".into(), "Changed title".into());
        }
        browser.render(area, &mut buffer, &mut stack, &ctx);
        assert_ne!(
            browser.file_preview.as_ref().expect("updated preview").list,
            preview
        );
        let previous = Arc::clone(&ctx.config);
        Arc::make_mut(&mut ctx.config).theme.preview_label_style = Style::default().fg(Color::Red);
        browser.render(area, &mut buffer, &mut stack, &ctx);
        assert!(!Arc::ptr_eq(
            &browser.file_preview.as_ref().expect("theme updated").config,
            &previous
        ));
    }

    #[rstest]
    fn preview_keeps_the_full_directory_length(ctx: Ctx) {
        let area = Rect::new(0, 0, 80, 10);
        let mut buffer = Buffer::empty(area);
        let mut stack = DirStack::new(vec![DirOrSong::name_only("Album".into())]);
        stack.insert(
            "Album".into(),
            (0..1000)
                .map(|i| {
                    DirOrSong::Song(Song {
                        file: i.to_string(),
                        ..Song::default()
                    })
                })
                .collect(),
        );
        Browser::new().render(area, &mut buffer, &mut stack, &ctx);
        assert_eq!(stack.next().expect("album").state.content_len(), Some(1000));
    }
}
