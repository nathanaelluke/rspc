use std::io::Write;

use ansi_colours::ansi256_from_rgb;
use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
};
use image::{Rgba, RgbaImage};
use ratatui::{buffer::Buffer, layout::Rect, style::Color as RatatuiColor};

use super::Backend;
use crate::{
    config::{
        Size,
        album_art::{HorizontalAlign, VerticalAlign},
    },
    ctx::Ctx,
    shared::image::{AlignedArea, resize_image},
    ui::image::clear_area,
};

#[derive(Debug)]
pub struct Block;

#[derive(derive_more::Debug)]
pub struct Data {
    #[debug(skip)]
    cells: Vec<[Rgba<u8>; 2]>,
    aligned_area: AlignedArea,
}

impl Data {
    fn new(image: &RgbaImage, aligned_area: AlignedArea) -> Self {
        let area = aligned_area.area;
        // Terminals without pixel dimensions still need the image mapped to cells.
        let cell_width = if aligned_area.cell_width_px > 0.0 {
            aligned_area.cell_width_px
        } else {
            f64::from(image.width()) / f64::from(area.width.max(1))
        };
        let cell_height = if aligned_area.cell_height_px > 0.0 {
            aligned_area.cell_height_px / 2.0
        } else {
            f64::from(image.height()) / (2.0 * f64::from(area.height.max(1)))
        };
        let cells = area
            .positions()
            .map(|position| {
                let x = (f64::from(position.x - area.x) * cell_width) as u32;
                let y = f64::from(position.y - area.y) * 2.0 * cell_height;
                let sample = |y| {
                    average_region_pixel(
                        image,
                        x,
                        y,
                        (cell_width as u32).max(1),
                        (cell_height as u32).max(1),
                        image.width(),
                        image.height(),
                    )
                };
                [sample(y as u32), sample((y + cell_height) as u32)]
            })
            .collect();
        Self {
            cells,
            aligned_area,
        }
    }
}

impl Block {
    /// Render prepared cells into the frame so popups can cover individual cells.
    pub fn render(buffer: &mut Buffer, data: &Data) {
        let area = data.aligned_area.area;
        let truecolor = truecolor_available();
        for position in area.intersection(*buffer.area()).positions() {
            let index = usize::from(position.y - area.y) * usize::from(area.width)
                + usize::from(position.x - area.x);
            let [top, bottom] = data.cells[index];
            let color = |pixel: Rgba<u8>| {
                if truecolor {
                    RatatuiColor::Rgb(pixel[0], pixel[1], pixel[2])
                } else {
                    RatatuiColor::Indexed(ansi256_from_rgb((pixel[0], pixel[1], pixel[2])))
                }
            };
            buffer[position]
                .set_symbol(LOWER_HALF_BLOCK)
                .set_fg(color(bottom))
                .set_bg(color(top));
        }
    }
}

impl Backend for Block {
    type EncodedData = Data;

    fn hide(
        &mut self,
        w: &mut impl Write,
        size: Rect,
        bg_color: Option<crossterm::style::Color>,
    ) -> anyhow::Result<()> {
        clear_area(w, bg_color, size)
    }

    fn display(&mut self, w: &mut impl Write, data: &Self::EncodedData, _ctx: &Ctx) -> Result<()> {
        queue!(w, SavePosition)?;
        let area = data.aligned_area.area;
        let truecolor = truecolor_available();
        for term_y in 0..area.height {
            queue!(w, MoveTo(area.x, area.y + term_y))?;
            for term_x in 0..area.width {
                let index = usize::from(term_y) * usize::from(area.width) + usize::from(term_x);
                let [top, bottom] = data.cells[index];
                queue!(
                    w,
                    SetForegroundColor(color_from_pixel(bottom, truecolor)),
                    SetBackgroundColor(color_from_pixel(top, truecolor)),
                    Print(LOWER_HALF_BLOCK)
                )?;
            }
            queue!(w, ResetColor)?;
        }

        w.flush()?;
        queue!(w, RestorePosition)?;

        Ok(())
    }

    fn create_data(
        image_data: &[u8],
        area: Rect,
        max_size: Size,
        halign: HorizontalAlign,
        valign: VerticalAlign,
    ) -> Result<Self::EncodedData> {
        let (img, resized_area) = resize_image(image_data, area, max_size, halign, valign)?;
        Ok(Data::new(&img.into_rgba8(), resized_area))
    }
}

pub(crate) const LOWER_HALF_BLOCK: &str = "\u{2584}";

#[inline]
fn average_region_pixel(
    img: &image::RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    width: u32,
    height: u32,
) -> image::Rgba<u8> {
    let x_end = x.saturating_add(w).min(width);
    let y_end = y.saturating_add(h).min(height);

    if x_end <= x || y_end <= y {
        return image::Rgba([0, 0, 0, 255]);
    }

    let mut sr: u64 = 0;
    let mut sg: u64 = 0;
    let mut sb: u64 = 0;
    let mut sa: u64 = 0;
    let mut count: u64 = 0;

    for yy in y..y_end {
        for xx in x..x_end {
            let pixel = get_pixel_safe(img, xx, yy, width, height);
            sr += pixel[0] as u64;
            sg += pixel[1] as u64;
            sb += pixel[2] as u64;
            sa += pixel[3] as u64;
            count += 1;
        }
    }

    image::Rgba([
        sr.checked_div(count).unwrap_or(0) as u8,
        sg.checked_div(count).unwrap_or(0) as u8,
        sb.checked_div(count).unwrap_or(0) as u8,
        sa.checked_div(count).unwrap_or(255) as u8,
    ])
}

#[inline]
fn get_pixel_safe(
    img: &image::RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> image::Rgba<u8> {
    if x < width && y < height {
        *img.get_pixel(x, y)
    } else {
        image::Rgba([0, 0, 0, 255])
    }
}

#[inline]
fn truecolor_available() -> bool {
    if let Ok(value) = std::env::var("COLORTERM") {
        value.contains("truecolor") || value.contains("24bit")
    } else {
        false
    }
}

#[inline]
fn color_from_pixel(pixel: image::Rgba<u8>, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb {
            r: pixel[0],
            g: pixel[1],
            b: pixel[2],
        }
    } else {
        Color::AnsiValue(ansi256_from_rgb((pixel[0], pixel[1], pixel[2])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Clear};

    #[test]
    fn popup_covers_only_overlapping_art_and_closing_restores_it() {
        let data = Data::new(
            &image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 20, 255])),
            AlignedArea {
                area: Rect::new(0, 0, 4, 2),
                size_px: Size {
                    width: 4,
                    height: 4,
                },
                cell_width_px: 1.0,
                cell_height_px: 2.0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(4, 2)).expect("terminal");
        let render = |frame: &mut ratatui::Frame, popup| {
            Block::render(frame.buffer_mut(), &data);
            if popup {
                frame.render_widget(Clear, Rect::new(2, 0, 2, 2));
            }
        };
        terminal.draw(|frame| render(frame, false)).expect("cover");
        let original = terminal.backend().buffer().clone();
        terminal.draw(|frame| render(frame, true)).expect("popup");
        assert_eq!(terminal.backend().buffer()[(0, 0)], original[(0, 0)]);
        assert_eq!(terminal.backend().buffer()[(2, 0)].symbol(), " ");
        terminal.draw(|frame| render(frame, false)).expect("close");
        assert_eq!(terminal.backend().buffer(), &original);
    }

    #[test]
    fn renders_when_terminal_pixel_dimensions_are_unavailable() {
        let data = Data::new(
            &image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 255])),
            AlignedArea {
                area: Rect::new(0, 0, 4, 2),
                size_px: Size {
                    width: 4,
                    height: 4,
                },
                cell_width_px: 0.0,
                cell_height_px: 0.0,
            },
        );
        let mut buffer = Buffer::empty(data.aligned_area.area);
        Block::render(&mut buffer, &data);
        assert_ne!(buffer[(0, 0)].fg, RatatuiColor::Indexed(16));
        assert_eq!(buffer[(0, 0)].fg, buffer[(3, 1)].fg);
    }
}
