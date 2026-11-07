use anyhow::Result;
use either::Either;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use super::Pane;
use crate::{
    config::{
        album_art::VerticalAlign,
        theme::properties::{Property, PropertyKind},
    },
    ctx::Ctx,
    shared::keys::ActionEvent,
    ui::widgets::scrolling_line::ScrollingLine,
};

#[derive(Debug)]
pub struct PropertyPane<'content> {
    content: &'content Vec<Property<PropertyKind>>,
    align: Alignment,
    scroll_speed: u64,
    wrap: bool,
    vertical_align: VerticalAlign,
}

impl<'content> PropertyPane<'content> {
    pub fn new(
        content: &'content Vec<Property<PropertyKind>>,
        align: Alignment,
        scroll_speed: u64,
        wrap: bool,
        vertical_align: VerticalAlign,
    ) -> Self {
        Self {
            content,
            align,
            scroll_speed,
            wrap,
            vertical_align,
        }
    }
}

impl Pane for PropertyPane<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);

        let spans = self.content.iter().fold(Vec::new(), |mut acc, val| {
            match val.as_span(
                song,
                ctx,
                &ctx.config.theme.format_tag_separator,
                ctx.config.theme.multiple_tag_resolution_strategy,
            ) {
                Some(Either::Left(span)) => acc.push(span),
                Some(Either::Right(ref mut spans)) => acc.append(spans),
                None => {}
            }
            acc
        });

        if self.wrap {
            let paragraph = Paragraph::new(multiline_text(spans))
                .wrap(Wrap { trim: true })
                .alignment(if self.align == Alignment::Center {
                    Alignment::Left
                } else {
                    self.align
                });
            let height = paragraph
                .line_count(area.width)
                .min(usize::from(area.height)) as u16;
            let spare = area.height.saturating_sub(height);
            let offset = match self.vertical_align {
                VerticalAlign::Top => 0,
                VerticalAlign::Center => spare / 2,
                VerticalAlign::Bottom => spare,
            };
            let text_area = Rect::new(area.x, area.y + offset, area.width, height);
            frame.render_widget(paragraph, text_area);
            if self.align == Alignment::Center {
                center_rows(frame.buffer_mut(), text_area);
            }
            return Ok(());
        }

        let scrolling_line = ScrollingLine::builder()
            .scroll_speed(self.scroll_speed)
            .align(self.align)
            .line(Line::from(spans))
            .progress(ctx.status.elapsed)
            .build();
        frame.render_widget(scrolling_line, area);

        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

/// Match the image/layout convention: divide the remaining space, rather than
/// subtracting two independently rounded half-widths as Paragraph does.
fn center_rows(buffer: &mut Buffer, area: Rect) {
    if area.width == 0 {
        return;
    }
    for y in area.y..area.bottom() {
        let width = (area.x..area.right())
            .rev()
            .find(|x| buffer[(*x, y)].symbol() != " ")
            .map_or(0, |x| x - area.x + buffer[(x, y)].symbol().width() as u16);
        let offset = area.width.saturating_sub(width) / 2;
        let start = buffer.index_of(area.x, y);
        buffer.content[start..start + usize::from(area.width)].rotate_right(usize::from(offset));
    }
}

fn multiline_text(spans: Vec<Span<'_>>) -> Text<'_> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    for span in spans {
        for (index, part) in span.content.split('\n').enumerate() {
            if index > 0 {
                lines.push(Line::from(std::mem::take(&mut line)));
            }
            line.push(Span::styled(part.to_owned(), span.style));
        }
    }
    lines.push(Line::from(line));
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use rstest::rstest;

    use super::*;
    use crate::{
        config::tabs::{PaneType, PaneTypeFile},
        tests::fixtures::ctx,
    };

    #[test]
    fn centered_rows_match_cover_alignment_for_odd_even_and_wide_text() {
        use ratatui::widgets::Widget;
        for width in [9, 10, 23, 24] {
            let area = Rect::new(3, 2, width, 5);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 32, 10));
            Paragraph::new("S.N.C\nDARKSIDE\n界界界\nA longer album name")
                .wrap(Wrap { trim: true })
                .render(area, &mut buffer);
            center_rows(&mut buffer, area);
            for y in area.y..area.bottom() {
                let start = (area.x..area.right()).find(|&x| buffer[(x, y)].symbol() != " ");
                let Some(start) = start else {
                    continue;
                };
                let end = (area.x..area.right())
                    .rev()
                    .find(|&x| buffer[(x, y)].symbol() != " ")
                    .expect("row text");
                let end = end + buffer[(end, y)].symbol().width() as u16;
                let left = start - area.x;
                let right = area.right() - end;
                assert!(
                    left <= right && right - left <= 1,
                    "width {width}, row {y}: left {left}, right {right}"
                );
            }
        }
    }

    #[rstest]
    fn wrapped_properties_preserve_styles_and_center_the_complete_block(ctx: Ctx) {
        let file: PaneTypeFile = ron::from_str(
            r#"#![enable(implicit_some)]
        Property(
            content: [
                (kind: Text("TITLE\n"), style: (fg: "gray")),
                (kind: Text("A rather long song name"), style: (fg: "white")),
                (kind: Text("\n\nARTIST\nA band"), style: (fg: "gray")),
            ], align: Center, wrap: true, vertical_align: Center,
        )"#,
        )
        .expect("property config");
        let PaneType::Property {
            content,
            align,
            scroll_speed,
            wrap,
            vertical_align,
        } = file.try_into().expect("convert property")
        else {
            panic!("property pane")
        };
        let mut pane =
            PropertyPane::new(&content, align, scroll_speed.into(), wrap, vertical_align);
        let mut terminal = Terminal::new(TestBackend::new(16, 12)).expect("test terminal");
        terminal
            .draw(|frame| {
                pane.render(frame, frame.area(), &ctx)
                    .expect("render property");
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..16)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_owned()
        };
        assert_eq!(
            (3..9).map(row).collect::<Vec<_>>(),
            [
                "TITLE",
                "A rather long",
                "song name",
                "",
                "ARTIST",
                "A band"
            ]
        );
        assert_eq!(buffer[(8, 3)].fg, Color::Gray);
        assert_eq!(buffer[(4, 4)].fg, Color::White);
        assert_eq!(row(2), "");
        assert_eq!(row(9), "");
    }

    #[test]
    fn old_property_configs_keep_single_line_top_alignment() {
        let file: PaneTypeFile =
            ron::from_str("Property(content: [])").expect("old property config");
        let PaneType::Property {
            wrap,
            vertical_align,
            ..
        } = file.try_into().expect("convert")
        else {
            panic!("property pane")
        };
        assert!(!wrap);
        assert_eq!(vertical_align, VerticalAlign::Top);
    }
}
