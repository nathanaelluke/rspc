// This is a "fork" of ratatui's Tabs widget

// The MIT License (MIT)
//
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    prelude::Alignment,
    style::{Style, Styled},
    symbols,
    text::{Line, Span},
    widgets::{Block, Widget},
};

use super::get_line_offset;

/// A widget to display available tabs in a multiple panels context.
///
/// # Examples
///
/// ```
/// # use ratatui::widgets::{Block, Borders, Tabs};
/// # use ratatui::style::{Style, Color};
/// # use ratatui::text::{Line};
/// # use ratatui::symbols::{DOT};
/// let titles = ["Tab1", "Tab2", "Tab3", "Tab4"].iter().cloned().map(Line::from).collect();
/// Tabs::new(titles)
///     .block(Block::default().title("Tabs").borders(Borders::ALL))
///     .style(Style::default().fg(Color::White))
///     .highlight_style(Style::default().fg(Color::Yellow))
///     .divider(DOT);
/// ```
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Tabs<'a> {
    /// A block to wrap this widget in if necessary
    pub block: Option<Block<'a>>,
    /// One title for each tab
    pub titles: Vec<Line<'a>>,
    /// The index of the selected tabs
    selected: usize,
    /// The style used to draw the text
    pub style: Style,
    /// Style to apply to the selected item
    pub highlight_style: Style,
    /// Tab divider
    pub divider: Span<'a>,
    /// Alignment of the tabs
    pub alignment: Alignment,
    /// Distribute unused space across tab cells, including their backgrounds.
    pub fill: bool,
    first_visible: usize,
    /// Vec of areas that tabs were last rendered in
    pub areas: Vec<Rect>,
}

#[allow(unused)]
impl<'a> Tabs<'a> {
    pub fn new<T>(titles: Vec<T>) -> Tabs<'a>
    where
        T: Into<Line<'a>>,
    {
        let titles: Vec<_> = titles.into_iter().map(Into::into).collect();
        Tabs {
            block: None,
            selected: 0,
            style: Style::default(),
            highlight_style: Style::default(),
            divider: Span::raw(symbols::line::VERTICAL),
            alignment: Alignment::Left,
            fill: false,
            first_visible: 0,
            areas: vec![Rect::default(); titles.len()],
            titles,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Tabs<'a> {
        self.block = Some(block);
        self
    }

    pub fn select(&mut self, selected: usize) -> &mut Self {
        self.selected = selected;
        self
    }

    pub fn style(mut self, style: Style) -> Tabs<'a> {
        self.style = style;
        self
    }

    pub fn titles(&mut self, titles: Vec<impl Into<Line<'a>>>) -> &mut Tabs<'a> {
        let titles: Vec<_> = titles.into_iter().map(Into::into).collect();
        self.titles = titles;
        self
    }

    pub fn highlight_style(mut self, style: Style) -> Tabs<'a> {
        self.highlight_style = style;
        self
    }

    pub fn divider<T>(mut self, divider: T) -> Tabs<'a>
    where
        T: Into<Span<'a>>,
    {
        self.divider = divider.into();
        self
    }

    pub fn alignment(mut self, alignment: Alignment) -> Tabs<'a> {
        self.alignment = alignment;
        self
    }

    pub fn fill(mut self, fill: bool) -> Self {
        self.fill = fill;
        self
    }

    fn render_filled(&mut self, first: usize, area: Rect, buf: &mut Buffer) {
        let available = usize::from(area.width);
        let mut used = 0;
        let mut end = first;
        for title in &self.titles[first..] {
            let width = title.width();
            if end != first && used + width > available {
                break;
            }
            used += width.min(available);
            end += 1;
        }
        let count = end - first;
        if count == 0 {
            return;
        }
        let extra = available.saturating_sub(used);
        let mut widths: Vec<_> = (first..end)
            .enumerate()
            .map(|(offset, index)| {
                self.titles[index].width().min(available)
                    + extra / count
                    + usize::from(offset < extra % count)
            })
            .collect();
        if count > 2 {
            // Give both ends equal, symmetric padding. Interior tabs absorb
            // the odd columns that cannot be split across two sides.
            let edge_extra = extra / count / 2 * 2;
            let mut spare = 0;
            for offset in [0, count - 1] {
                let width = self.titles[first + offset].width() + edge_extra;
                spare += widths[offset] - width;
                widths[offset] = width;
            }
            for offset in 0..spare {
                widths[1 + offset % (count - 2)] += 1;
            }
        }
        let mut x = area.x;
        for (offset, index) in (first..end).enumerate() {
            let title = &self.titles[index];
            let width = widths[offset] as u16;
            let cell = Rect::new(x, area.y, width, 1);
            let offset = width.saturating_sub(title.width() as u16) / 2;
            buf.set_line(x + offset, area.y, title, width);
            if index == self.selected {
                buf.set_style(cell, self.highlight_style);
            }
            self.areas[index] = cell;
            x += width;
        }
    }
}

impl<'a> Styled for Tabs<'a> {
    type Item = Tabs<'a>;

    fn style(&self) -> Style {
        self.style
    }

    fn set_style<S: Into<Style>>(self, style: S) -> Self::Item {
        self.style(style.into())
    }
}

impl Widget for &mut Tabs<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, self.style);
        let tabs_area = match &self.block {
            Some(b) => {
                let inner_area = b.inner(area);
                b.render(area, buf);
                inner_area
            }
            None => area,
        };

        if tabs_area.height < 1 {
            return;
        }

        self.areas.fill(Rect::default());
        let gap = if self.fill {
            0
        } else {
            self.divider.width() + 1
        };
        let total_width = self.titles.iter().map(Line::width).sum::<usize>()
            + gap * self.titles.len().saturating_sub(1);
        let mut first = 0;
        if total_width > usize::from(tabs_area.width) {
            let selected = self.selected.min(self.titles.len().saturating_sub(1));
            first = self.first_visible.min(selected);
            let mut width = self.titles[first..=selected]
                .iter()
                .map(Line::width)
                .sum::<usize>()
                + gap * (selected - first);
            while first < selected && width > usize::from(tabs_area.width) {
                width -= self.titles[first].width() + gap;
                first += 1;
            }
        }
        self.first_visible = first;

        if self.fill {
            self.render_filled(first, tabs_area, buf);
            return;
        }

        let mut x = get_line_offset(
            self.titles
                .iter()
                .skip(first)
                .map(|t| t.width() as u16)
                .sum(),
            tabs_area.width,
            self.alignment,
        ) + area.x;

        let titles_length = self.titles.len();
        for (i, title) in self.titles.iter().enumerate().skip(first) {
            let last_title = titles_length.saturating_sub(1) == i;
            let remaining_width = tabs_area.right().saturating_sub(x);
            if remaining_width == 0 {
                // make the rest of the areas empty since we ran out of space
                self.areas[i..]
                    .iter_mut()
                    .for_each(|a| *a = Rect::default());
                break;
            }
            let pos = buf.set_line(x, tabs_area.top(), title, remaining_width);
            self.areas[i] = Rect {
                x,
                y: tabs_area.top(),
                width: pos.0.saturating_sub(x),
                height: 1,
            };

            if i == self.selected {
                buf.set_style(
                    Rect {
                        x,
                        y: tabs_area.top(),
                        width: pos.0.saturating_sub(x),
                        height: 1,
                    },
                    self.highlight_style,
                );
            }
            x = pos.0.saturating_add(1);
            let remaining_width = tabs_area.right().saturating_sub(x);
            if remaining_width == 0 || last_title {
                if i < self.areas.len().saturating_sub(2) {
                    // make the rest of the areas empty since we ran out of
                    // space
                    self.areas[i + 1..]
                        .iter_mut()
                        .for_each(|a| *a = Rect::default());
                }
                break;
            }
            let pos = buf.set_span(
                x.saturating_sub(1),
                tabs_area.top(),
                &self.divider,
                self.divider.width() as u16,
            );
            x = pos.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Stylize};

    use super::*;

    #[test]
    fn selected_tab_stays_visible_when_the_bar_is_narrow() {
        let mut tabs = Tabs::new(vec!["Queue", "Directories", "Artists"]).divider("");
        let area = Rect::new(0, 0, 10, 1);
        let mut buffer = Buffer::empty(area);
        tabs.render(area, &mut buffer);
        tabs.select(2);
        tabs.render(area, &mut buffer);
        assert_eq!(tabs.areas[0], Rect::default());
        assert_eq!(tabs.areas[1], Rect::default());
        assert_eq!(tabs.areas[2].width, 7);
        let text = (0..7).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        assert_eq!(text, "Artists");
    }

    #[test]
    fn filled_tabs_cover_the_strip_and_have_matching_mouse_areas() {
        let area = Rect::new(2, 1, 33, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 3));
        let mut tabs = Tabs::new(vec![" Queue ", " Search "])
            .divider("")
            .fill(true)
            .style(Style::default().bg(Color::DarkGray))
            .highlight_style(Style::default().bg(Color::Gray));
        tabs.select(1).render(area, &mut buffer);
        assert_eq!(tabs.areas[0].left(), area.left());
        assert_eq!(tabs.areas[0].right(), tabs.areas[1].left());
        assert_eq!(tabs.areas[1].right(), area.right());
        for x in tabs.areas[1].left()..area.right() {
            assert_eq!(buffer[(x, area.y)].bg, Color::Gray);
        }
    }

    #[test]
    fn filled_tabs_keep_the_selected_tab_visible_at_small_widths() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buffer = Buffer::empty(area);
        let mut tabs = Tabs::new(vec![" Queue ", " Directories ", " Search "])
            .divider("")
            .fill(true);
        tabs.select(2).render(area, &mut buffer);
        assert_eq!(tabs.areas[..2], [Rect::default(); 2]);
        assert_eq!(tabs.areas[2], area);
    }

    #[test]
    fn end_tabs_have_equal_padding_on_both_sides() {
        let area = Rect::new(0, 0, 58, 1);
        let mut buffer = Buffer::empty(area);
        let mut tabs = Tabs::new(vec![
            "  Queue  ",
            "  Directories  ",
            "  Artists  ",
            "  Search  ",
        ])
        .divider("")
        .fill(true);
        tabs.render(area, &mut buffer);
        let padding = |index: usize| {
            let cell = tabs.areas[index];
            let text = (cell.left()..cell.right())
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>();
            let left = text.len() - text.trim_start().len();
            let right = text.len() - text.trim_end().len();
            assert_eq!(left, right, "{text:?}");
            left
        };
        assert_eq!(padding(0), padding(3));
        assert_eq!(tabs.areas[0].left(), area.left());
        assert_eq!(tabs.areas[3].right(), area.right());
        for pair in tabs.areas.windows(2) {
            assert_eq!(pair[0].right(), pair[1].left());
        }
    }

    #[test]
    fn selected_labels_are_centered_at_every_filled_width() {
        let names = [
            "Queue",
            "Directories",
            "Artists",
            "Album Artists",
            "Albums",
            "Playlists",
            "Search",
        ];
        for width in 100..190 {
            for selected in 0..names.len() {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                let mut tabs = Tabs::new(names.iter().map(|name| format!("  {name}  ")).collect())
                    .divider("")
                    .fill(true);
                tabs.select(selected).render(area, &mut buffer);
                let cell = tabs.areas[selected];
                let text = (cell.left()..cell.right())
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>();
                assert_eq!(text.trim(), names[selected]);
                let left = text.len() - text.trim_start().len();
                let right = text.len() - text.trim_end().len();
                assert!(left.abs_diff(right) <= 1, "width {width}, {text:?}");
                let visible = tabs
                    .areas
                    .iter()
                    .filter(|area| area.width > 0)
                    .collect::<Vec<_>>();
                assert_eq!(visible[0].left(), area.left());
                assert_eq!(visible.last().expect("visible tabs").right(), area.right());
                assert!(
                    visible
                        .windows(2)
                        .all(|pair| pair[0].right() == pair[1].left())
                );
            }
        }
    }

    #[test]
    fn selecting_tabs_only_changes_the_highlight_when_they_fit() {
        let names = [
            "Queue",
            "Directories",
            "Artists",
            "Album Artists",
            "Albums",
            "Playlists",
            "Search",
        ];
        for width in 100..190 {
            let area = Rect::new(0, 0, width, 1);
            let mut tabs = Tabs::new(names.iter().map(|name| format!("  {name}  ")).collect())
                .divider("")
                .fill(true);
            let mut buffer = Buffer::empty(area);
            tabs.render(area, &mut buffer);
            let positions = tabs.areas.clone();
            let text: Vec<_> = buffer
                .content
                .iter()
                .map(|cell| cell.symbol().to_owned())
                .collect();
            for selected in 0..names.len() {
                let mut next = Buffer::empty(area);
                tabs.select(selected).render(area, &mut next);
                assert_eq!(tabs.areas, positions, "width {width}, selected {selected}");
                assert_eq!(
                    next.content
                        .iter()
                        .map(|cell| cell.symbol().to_owned())
                        .collect::<Vec<_>>(),
                    text
                );
            }
        }
    }

    #[test]
    fn narrow_bars_scroll_only_when_the_selection_leaves_the_visible_tabs() {
        let area = Rect::new(0, 0, 23, 1);
        let mut tabs = Tabs::new(vec![" Queue ", " Directories ", " Artists ", " Albums "])
            .divider("")
            .fill(true);
        let mut buffer = Buffer::empty(area);
        tabs.select(2).render(area, &mut buffer);
        let positions = tabs.areas.clone();
        tabs.select(1).render(area, &mut buffer);
        assert_eq!(tabs.areas, positions);
        tabs.select(0).render(area, &mut buffer);
        assert!(tabs.areas[0].width > 0);
    }

    #[test]
    fn can_be_stylized() {
        assert_eq!(
            Tabs::new(vec![""])
                .black()
                .on_white()
                .bold()
                .not_italic()
                .style,
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
                .remove_modifier(Modifier::ITALIC)
        );
    }
}
