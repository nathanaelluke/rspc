use std::io::Write;

use anyhow::Result;
use crossterm::{
    cursor::{RestorePosition, SavePosition},
    queue,
    style::{Colors, ResetColor, SetColors},
};
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
};

use crate::{
    config::{
        Size,
        album_art::{HorizontalAlign, VerticalAlign},
    },
    ctx::Ctx,
    shared::macros::csi_move,
};

pub mod block;
pub mod facade;
pub mod iterm2;
pub mod kitty;
pub mod sixel;
pub mod ueberzug;

/// Restore cells changed by an image protocol without clearing other panes.
pub fn invalidate_area(buffer: &mut Buffer, area: Rect) {
    for position in area.intersection(*buffer.area()).positions() {
        buffer[position].set_diff_option(CellDiffOption::AlwaysUpdate);
    }
}

#[allow(unused)]
pub trait Backend {
    type EncodedData;
    fn hide(
        &mut self,
        w: &mut impl Write,
        size: Rect,
        bg_color: Option<crossterm::style::Color>,
    ) -> Result<()>;
    fn cleanup(self: Box<Self>, rect: Rect) -> Result<()> {
        Ok(())
    }
    fn display(&mut self, w: &mut impl Write, data: &Self::EncodedData, ctx: &Ctx) -> Result<()>;
    fn create_data(
        image_data: &[u8],
        area: Rect,
        max_size: Size,
        halign: HorizontalAlign,
        valign: VerticalAlign,
    ) -> Result<Self::EncodedData>;
}

pub fn clear_area(
    w: &mut impl Write,
    bg_color: Option<crossterm::style::Color>,
    area: Rect,
) -> Result<()> {
    let colors = Colors {
        background: bg_color,
        foreground: None,
    };
    queue!(w, SetColors(colors))?;
    queue!(w, SavePosition)?;
    let capacity: usize = 2usize * area.width as usize * area.height as usize;
    let mut buf = Vec::with_capacity(capacity);
    for y in area.top()..area.bottom() {
        csi_move!(buf, area.x, y)?;
        for _ in 0..area.width {
            write!(buf, " ")?;
        }
    }

    w.write_all(&buf)?;
    w.flush()?;
    queue!(w, RestorePosition)?;
    queue!(w, ResetColor)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
        buffer::Cell,
        style::{Color, Style},
        widgets::Block,
    };

    use super::*;

    #[test]
    fn image_cleanup_restores_unchanged_modal_cells_without_clearing_other_panes() {
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).expect("terminal");
        let render = |frame: &mut ratatui::Frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(Color::DarkGray)),
                frame.area(),
            );
        };
        terminal.draw(render).expect("initial frame");

        // Simulate an image backend changing cells outside Ratatui's frame.
        let image = Rect::new(2, 1, 4, 2);
        let external = Cell::new("#");
        terminal
            .backend_mut()
            .draw(image.positions().map(|p| (p.x, p.y, &external)))
            .expect("image overlay");
        terminal
            .backend_mut()
            .draw(std::iter::once((0, 0, &external)))
            .expect("unrelated overlay");

        terminal
            .draw(|frame| {
                render(frame);
                invalidate_area(frame.buffer_mut(), image);
            })
            .expect("repaint image area");

        let buffer = terminal.backend().buffer();
        for position in image.positions() {
            assert_eq!(buffer[position].symbol(), " ");
            assert_eq!(buffer[position].bg, Color::DarkGray);
        }
        assert_eq!(buffer[(0, 0)].symbol(), "#");
    }

    #[test]
    fn image_cleanup_is_clipped_to_the_current_viewport() {
        let previous = Buffer::empty(Rect::new(0, 0, 12, 5));
        let mut next = previous.clone();
        invalidate_area(&mut next, Rect::new(10, 3, 20, 20));
        let cells = previous
            .diff(&next)
            .into_iter()
            .map(|(x, y, _)| (x, y))
            .collect::<Vec<_>>();
        assert_eq!(cells, [(10, 3), (11, 3), (10, 4), (11, 4)]);
    }
}
