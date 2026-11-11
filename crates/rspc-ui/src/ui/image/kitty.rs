use std::{io::Write, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use base64::Engine;
use crossterm::{
    cursor::{RestorePosition, SavePosition},
    queue,
};
use flate2::Compression;
use itertools::Itertools;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use super::{Backend, csi_move};
use crate::{
    config::{
        Size,
        album_art::{HorizontalAlign, VerticalAlign},
    },
    ctx::Ctx,
    shared::{
        image::{create_aligned_area, get_gif_frames, resize_image},
        terminal::{Emulator, TERMINAL},
        tmux::tmux_write,
    },
};

#[derive(Debug)]
pub struct Kitty {
    uploaded: Option<Arc<Data>>,
    unicode_placeholders: bool,
}

impl Default for Kitty {
    fn default() -> Self {
        Self {
            uploaded: None,
            unicode_placeholders: crate::shared::tmux::is_inside_tmux()
                || TERMINAL.emulator() == Emulator::Kitty,
        }
    }
}

impl Kitty {
    pub fn uses_placeholders(&self) -> bool {
        self.unicode_placeholders
    }

    pub fn invalidate(&mut self) {
        self.uploaded = None;
    }

    /// Put image cells in Ratatui's frame so popups can cover and restore them.
    pub fn render(&self, buffer: &mut Buffer, data: &Data, background: Option<Color>) {
        let area = match data {
            Data::ImageData(image) => image.aligned_area,
            Data::AnimationData(animation) => animation.aligned_area,
        };
        let mut symbol = String::with_capacity(16);
        for position in area.intersection(*buffer.area()).positions() {
            let Some(row) = GRID.get(usize::from(position.y - area.y)) else {
                continue;
            };
            let Some(col) = GRID.get(usize::from(position.x - area.x)) else {
                continue;
            };
            symbol.clear();
            symbol.push(PLACEHOLDER);
            symbol.push_str(row);
            symbol.push_str(col);
            let cell = &mut buffer[position];
            cell.reset();
            cell.set_symbol(&symbol).set_fg(Color::Indexed(1));
            if let Some(background) = background {
                cell.set_bg(background);
            }
        }
    }
}

impl Backend for Kitty {
    type EncodedData = Arc<Data>;

    fn hide(
        &mut self,
        w: &mut impl Write,
        area: Rect,
        bg_color: Option<crossterm::style::Color>,
    ) -> Result<()> {
        if !self.unicode_placeholders {
            tmux_write!(w, "\x1b_Ga=d,d=i,i=1,p=1,q=2\x1b\\")?;
        }
        // Soft-delete the placement while retaining the decoded image. Unicode
        // placements are hidden by clearing their text instead.
        super::clear_area(w, bg_color, area)?;
        Ok(())
    }

    fn display(&mut self, w: &mut impl Write, data: &Self::EncodedData, _ctx: &Ctx) -> Result<()> {
        let cached = self
            .uploaded
            .as_ref()
            .is_some_and(|image| Arc::ptr_eq(image, data));
        let area = match data.as_ref() {
            Data::ImageData(image) => {
                if !cached {
                    transfer_image_data(
                        w,
                        &image.content,
                        image.img_width,
                        image.img_height,
                        image.aligned_area,
                        self.unicode_placeholders,
                    )?;
                }
                image.aligned_area
            }
            Data::AnimationData(animation) => {
                if !cached {
                    transfer_animation_data(w, animation, self.unicode_placeholders)?;
                }
                animation.aligned_area
            }
        };
        if !self.unicode_placeholders {
            queue!(w, SavePosition)?;
            csi_move!(w, area.x, area.y)?;
            tmux_write!(
                w,
                "\x1b_Ga=p,i=1,p=1,c={cols},r={rows},C=1,q=2\x1b\\",
                cols = area.width,
                rows = area.height
            )?;
            queue!(w, RestorePosition)?;
            w.flush()?;
        }
        self.uploaded = Some(Arc::clone(data));
        Ok(())
    }

    fn cleanup(self: Box<Self>, _area: Rect) -> Result<()> {
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut writer = writer.lock();
        // Delete only our image, including the retained virtual placement.
        tmux_write!(writer, "\x1b_Ga=d,d=I,i=1,q=2\x1b\\")?;
        writer.flush()?;
        Ok(())
    }

    fn create_data(
        image_data: &[u8],
        area: Rect,
        max_size: Size,
        halign: HorizontalAlign,
        valign: VerticalAlign,
    ) -> Result<Self::EncodedData> {
        let start_time = Instant::now();
        log::debug!(bytes = image_data.len(); "Compressing image data");

        if let Some(data) = get_gif_frames(image_data)? {
            let frames = data.frames;
            let (width, height) = data.dimensions;

            let frames: Vec<AnimationFrame> = frames
                .map_ok(|frame| {
                    let delay = frame.delay().numer_denom_ms();

                    AnimationFrame {
                        delay: delay.0 / delay.1,
                        content: base64::engine::general_purpose::STANDARD
                            .encode(frame.buffer().as_raw()),
                    }
                })
                .try_collect()?;
            let aligned_area =
                create_aligned_area(area, (width, height), max_size, halign, valign).area;

            Ok(Arc::new(Data::AnimationData(AnimationData {
                frames,
                is_compressed: false,
                img_width: width,
                img_height: height,
                aligned_area,
            })))
        } else {
            let (image, aligned_area) = resize_image(image_data, area, max_size, halign, valign)?;

            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), Compression::new(6));
            e.write_all(image.to_rgba8().as_raw())
                .context("Error occurred when writing image bytes to zlib encoder")?;

            let content = base64::engine::general_purpose::STANDARD.encode(
                e.finish()
                    .context("Error occurred when flushing image bytes to zlib encoder")?,
            );

            log::debug!(input_bytes = image_data.len(), compressed_bytes = content.len(), duration:? = start_time.elapsed(); "Image data compression finished");
            Ok(Arc::new(Data::ImageData(ImageData {
                content,
                aligned_area: aligned_area.area,
                img_width: image.width(),
                img_height: image.height(),
            })))
        }
    }
}

fn transfer_animation_data(
    w: &mut impl Write,
    data: &AnimationData,
    unicode_placeholders: bool,
) -> Result<()> {
    let start_time = Instant::now();
    let AnimationData {
        frames,
        is_compressed,
        img_width,
        img_height,
        aligned_area,
    } = data;

    log::debug!(frames = frames.len(), img_width, img_height, aligned_area:?; "Transferring animation data");

    if frames.len() < 2 {
        log::warn!("Less than two frames, invalid animation data");
        return Ok(());
    }

    let mut first_frame_iter = frames[0].content.chars().peekable();
    let chunk: String = first_frame_iter.by_ref().take(4096).collect();

    let m = i32::from(first_frame_iter.peek().is_some());
    let delay = frames[0].delay;

    // Create image and transfer first frame
    let action = if unicode_placeholders { "T" } else { "t" };
    let placeholder = u8::from(unicode_placeholders);
    tmux_write!(
        w,
        "\x1b_Gi=1,f=32,U={placeholder},a={action},t=d,m={m},z={delay},q=2,s={img_width},v={img_height},c={cols},r={rows}{compression};{chunk}\x1b\\",
        compression = if *is_compressed { ",o=z" } else { "" },
        cols = aligned_area.width,
        rows = aligned_area.height
    )?;

    // Transfer the rest of the first frame if any
    while first_frame_iter.peek().is_some() {
        let chunk: String = first_frame_iter.by_ref().take(4096).collect();
        let m = i32::from(first_frame_iter.peek().is_some());
        tmux_write!(w, "\x1b_Gi=1,m={m};{chunk}\x1b\\")?;
    }

    // Transfer rest of the frames, skip first because it was already
    // transferred
    for AnimationFrame { delay, content, .. } in frames.iter().skip(1) {
        let mut frame_iter = content.chars().peekable();
        let chunk: String = frame_iter.by_ref().take(4096).collect();

        tmux_write!(
            w,
            "\x1b_Gi=1,a=f,t=d,m={m},z={delay},q=2,s={img_width},v={img_height}{compression};{chunk}\x1b\\",
            compression = if *is_compressed { ",o=z" } else { "" }
        )?;

        while frame_iter.peek().is_some() {
            let chunk: String = frame_iter.by_ref().take(4096).collect();
            let m = i32::from(frame_iter.peek().is_some());
            tmux_write!(w, "\x1b_Ga=f,i=1,m={m};{chunk}\x1b\\")?;
        }
    }

    // Run the animation
    tmux_write!(w, "\x1b_Ga=a,i=1,s=3\x1b\\")?;
    log::debug!(duration:? = start_time.elapsed(); "Transfer finished");

    Ok(())
}

fn transfer_image_data(
    w: &mut impl Write,
    content: &str,
    img_width: u32,
    img_height: u32,
    area: Rect,
    unicode_placeholders: bool,
) -> Result<()> {
    let start_time = Instant::now();
    log::debug!(bytes = content.len(), img_width, img_height; "Transferring compressed image data");
    let mut iter = content.chars().peekable();

    let first: String = iter.by_ref().take(4096).collect();
    let more = i32::from(iter.peek().is_some());
    let action = if unicode_placeholders { "T" } else { "t" };
    let placeholder = u8::from(unicode_placeholders);
    tmux_write!(
        w,
        "\x1b_Gi=1,f=32,U={placeholder},t=d,a={action},m={more},q=2,o=z,s={img_width},v={img_height},c={cols},r={rows};{first}\x1b\\",
        cols = area.width,
        rows = area.height
    )?;

    while iter.peek().is_some() {
        let chunk: String = iter.by_ref().take(4096).collect();
        let m = i32::from(iter.peek().is_some());
        tmux_write!(w, "\x1b_Gm={m};{chunk}\x1b\\")?;
    }
    log::debug!(duration:? = start_time.elapsed(); "Transfer finished");

    Ok(())
}

#[derive(Debug)]
pub enum Data {
    ImageData(ImageData),
    AnimationData(AnimationData),
}

#[derive(derive_more::Debug)]
pub struct ImageData {
    #[debug(skip)]
    content: String,
    img_width: u32,
    img_height: u32,
    aligned_area: Rect,
}

#[derive(derive_more::Debug)]
pub struct AnimationFrame {
    #[debug(skip)]
    content: String,
    delay: u32,
}

#[derive(derive_more::Debug)]
pub struct AnimationData {
    #[debug(skip)]
    frames: Vec<AnimationFrame>,
    is_compressed: bool,
    img_width: u32,
    img_height: u32,
    aligned_area: Rect,
}

pub const PLACEHOLDER: char = '\u{10EEEE}';

const GRID: &[&str] = &[
    "\u{0305}",
    "\u{030D}",
    "\u{030E}",
    "\u{0310}",
    "\u{0312}",
    "\u{033D}",
    "\u{033E}",
    "\u{033F}",
    "\u{0346}",
    "\u{034A}",
    "\u{034B}",
    "\u{034C}",
    "\u{0350}",
    "\u{0351}",
    "\u{0352}",
    "\u{0357}",
    "\u{035B}",
    "\u{0363}",
    "\u{0364}",
    "\u{0365}",
    "\u{0366}",
    "\u{0367}",
    "\u{0368}",
    "\u{0369}",
    "\u{036A}",
    "\u{036B}",
    "\u{036C}",
    "\u{036D}",
    "\u{036E}",
    "\u{036F}",
    "\u{0483}",
    "\u{0484}",
    "\u{0485}",
    "\u{0486}",
    "\u{0487}",
    "\u{0592}",
    "\u{0593}",
    "\u{0594}",
    "\u{0595}",
    "\u{0597}",
    "\u{0598}",
    "\u{0599}",
    "\u{059C}",
    "\u{059D}",
    "\u{059E}",
    "\u{059F}",
    "\u{05A0}",
    "\u{05A1}",
    "\u{05A8}",
    "\u{05A9}",
    "\u{05AB}",
    "\u{05AC}",
    "\u{05AF}",
    "\u{05C4}",
    "\u{0610}",
    "\u{0611}",
    "\u{0612}",
    "\u{0613}",
    "\u{0614}",
    "\u{0615}",
    "\u{0616}",
    "\u{0617}",
    "\u{0657}",
    "\u{0658}",
    "\u{0659}",
    "\u{065A}",
    "\u{065B}",
    "\u{065D}",
    "\u{065E}",
    "\u{06D6}",
    "\u{06D7}",
    "\u{06D8}",
    "\u{06D9}",
    "\u{06DA}",
    "\u{06DB}",
    "\u{06DC}",
    "\u{06DF}",
    "\u{06E0}",
    "\u{06E1}",
    "\u{06E2}",
    "\u{06E4}",
    "\u{06E7}",
    "\u{06E8}",
    "\u{06EB}",
    "\u{06EC}",
    "\u{0730}",
    "\u{0732}",
    "\u{0733}",
    "\u{0735}",
    "\u{0736}",
    "\u{073A}",
    "\u{073D}",
    "\u{073F}",
    "\u{0740}",
    "\u{0741}",
    "\u{0743}",
    "\u{0745}",
    "\u{0747}",
    "\u{0749}",
    "\u{074A}",
    "\u{07EB}",
    "\u{07EC}",
    "\u{07ED}",
    "\u{07EE}",
    "\u{07EF}",
    "\u{07F0}",
    "\u{07F1}",
    "\u{07F3}",
    "\u{0816}",
    "\u{0817}",
    "\u{0818}",
    "\u{0819}",
    "\u{081B}",
    "\u{081C}",
    "\u{081D}",
    "\u{081E}",
    "\u{081F}",
    "\u{0820}",
    "\u{0821}",
    "\u{0822}",
    "\u{0823}",
    "\u{0825}",
    "\u{0826}",
    "\u{0827}",
    "\u{0829}",
    "\u{082A}",
    "\u{082B}",
    "\u{082C}",
    "\u{082D}",
    "\u{0951}",
    "\u{0953}",
    "\u{0954}",
    "\u{0F82}",
    "\u{0F83}",
    "\u{0F86}",
    "\u{0F87}",
    "\u{135D}",
    "\u{135E}",
    "\u{135F}",
    "\u{17DD}",
    "\u{193A}",
    "\u{1A17}",
    "\u{1A75}",
    "\u{1A76}",
    "\u{1A77}",
    "\u{1A78}",
    "\u{1A79}",
    "\u{1A7A}",
    "\u{1A7B}",
    "\u{1A7C}",
    "\u{1B6B}",
    "\u{1B6D}",
    "\u{1B6E}",
    "\u{1B6F}",
    "\u{1B70}",
    "\u{1B71}",
    "\u{1B72}",
    "\u{1B73}",
    "\u{1CD0}",
    "\u{1CD1}",
    "\u{1CD2}",
    "\u{1CDA}",
    "\u{1CDB}",
    "\u{1CE0}",
    "\u{1DC0}",
    "\u{1DC1}",
    "\u{1DC3}",
    "\u{1DC4}",
    "\u{1DC5}",
    "\u{1DC6}",
    "\u{1DC7}",
    "\u{1DC8}",
    "\u{1DC9}",
    "\u{1DCB}",
    "\u{1DCC}",
    "\u{1DD1}",
    "\u{1DD2}",
    "\u{1DD3}",
    "\u{1DD4}",
    "\u{1DD5}",
    "\u{1DD6}",
    "\u{1DD7}",
    "\u{1DD8}",
    "\u{1DD9}",
    "\u{1DDA}",
    "\u{1DDB}",
    "\u{1DDC}",
    "\u{1DDD}",
    "\u{1DDE}",
    "\u{1DDF}",
    "\u{1DE0}",
    "\u{1DE1}",
    "\u{1DE2}",
    "\u{1DE3}",
    "\u{1DE4}",
    "\u{1DE5}",
    "\u{1DE6}",
    "\u{1DFE}",
    "\u{20D0}",
    "\u{20D1}",
    "\u{20D4}",
    "\u{20D5}",
    "\u{20D6}",
    "\u{20D7}",
    "\u{20DB}",
    "\u{20DC}",
    "\u{20E1}",
    "\u{20E7}",
    "\u{20E9}",
    "\u{20F0}",
    "\u{2CEF}",
    "\u{2CF0}",
    "\u{2CF1}",
    "\u{2DE0}",
    "\u{2DE1}",
    "\u{2DE2}",
    "\u{2DE3}",
    "\u{2DE4}",
    "\u{2DE5}",
    "\u{2DE6}",
    "\u{2DE7}",
    "\u{2DE8}",
    "\u{2DE9}",
    "\u{2DEA}",
    "\u{2DEB}",
    "\u{2DEC}",
    "\u{2DED}",
    "\u{2DEE}",
    "\u{2DEF}",
    "\u{2DF0}",
    "\u{2DF1}",
    "\u{2DF2}",
    "\u{2DF3}",
    "\u{2DF4}",
    "\u{2DF5}",
    "\u{2DF6}",
    "\u{2DF7}",
    "\u{2DF8}",
    "\u{2DF9}",
    "\u{2DFA}",
    "\u{2DFB}",
    "\u{2DFC}",
    "\u{2DFD}",
    "\u{2DFE}",
    "\u{2DFF}",
    "\u{A66F}",
    "\u{A67C}",
    "\u{A67D}",
    "\u{A6F0}",
    "\u{A6F1}",
    "\u{A8E0}",
    "\u{A8E1}",
    "\u{A8E2}",
    "\u{A8E3}",
    "\u{A8E4}",
    "\u{A8E5}",
    "\u{A8E6}",
    "\u{A8E7}",
    "\u{A8E8}",
    "\u{A8E9}",
    "\u{A8EA}",
    "\u{A8EB}",
    "\u{A8EC}",
    "\u{A8ED}",
    "\u{A8EE}",
    "\u{A8EF}",
    "\u{A8F0}",
    "\u{A8F1}",
    "\u{AAB0}",
    "\u{AAB2}",
    "\u{AAB3}",
    "\u{AAB7}",
    "\u{AAB8}",
    "\u{AABE}",
    "\u{AABF}",
    "\u{AAC1}",
    "\u{FE20}",
    "\u{FE21}",
    "\u{FE22}",
    "\u{FE23}",
    "\u{FE24}",
    "\u{FE25}",
    "\u{FE26}",
    "\u{10A0F}",
    "\u{10A38}",
    "\u{1D185}",
    "\u{1D186}",
    "\u{1D187}",
    "\u{1D188}",
    "\u{1D189}",
    "\u{1D1AA}",
    "\u{1D1AB}",
    "\u{1D1AC}",
    "\u{1D1AD}",
    "\u{1D242}",
    "\u{1D243}",
    "\u{1D244}",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::fixtures::ctx;
    use rstest::rstest;

    fn image() -> Arc<Data> {
        Arc::new(Data::ImageData(ImageData {
            content: "encoded pixels".into(),
            img_width: 2,
            img_height: 2,
            aligned_area: Rect::new(1, 1, 2, 2),
        }))
    }

    #[rstest]
    #[case::native(false)]
    #[case::multiplexer(true)]
    fn hidden_covers_reuse_terminal_pixels_instead_of_uploading_again(
        #[case] unicode_placeholders: bool,
        ctx: Ctx,
    ) {
        let mut backend = Kitty {
            uploaded: None,
            unicode_placeholders,
        };
        let image = image();
        let mut output = Vec::new();
        backend
            .display(&mut output, &image, &ctx)
            .expect("initial image");
        assert!(String::from_utf8_lossy(&output).contains("f=32"));
        output.clear();
        backend
            .hide(&mut output, Rect::new(1, 1, 2, 2), None)
            .expect("hide");
        backend
            .display(&mut output, &image, &ctx)
            .expect("restore image");
        let restored = String::from_utf8_lossy(&output);
        assert!(!restored.contains("f=32"));
        assert!(
            !restored.contains("d=I"),
            "hiding must retain the image data"
        );
        if unicode_placeholders {
            assert!(!restored.contains("a=p"));
        } else {
            assert!(restored.contains("a=p"));
            assert!(restored.contains("d=i"));
        }

        backend.invalidate();
        output.clear();
        backend
            .display(&mut output, &image, &ctx)
            .expect("new terminal");
        assert!(String::from_utf8_lossy(&output).contains("f=32"));
    }

    #[test]
    fn a_small_image_finishes_in_its_first_transfer_chunk() {
        let mut output = Vec::new();
        transfer_image_data(&mut output, "small", 2, 2, Rect::new(0, 0, 2, 2), false)
            .expect("small transfer");
        assert!(String::from_utf8_lossy(&output).contains("m=0"));
        assert!(!String::from_utf8_lossy(&output).contains("m=1"));
    }

    #[test]
    fn popups_cover_only_the_image_cells_they_overlap() {
        use ratatui::{Terminal, backend::TestBackend, widgets::Clear};

        let backend = Kitty {
            uploaded: None,
            unicode_placeholders: true,
        };
        let image = image();
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).expect("terminal");
        let popup = Rect::new(2, 1, 2, 2);
        let render = |frame: &mut ratatui::Frame, modal| {
            backend.render(frame.buffer_mut(), &image, Some(Color::Black));
            if modal {
                frame.render_widget(Clear, popup);
            }
        };
        terminal.draw(|frame| render(frame, false)).expect("cover");
        let original = terminal.backend().buffer().clone();
        terminal.draw(|frame| render(frame, true)).expect("popup");
        let overlaid = terminal.backend().buffer();
        assert_eq!(overlaid[(1, 1)], original[(1, 1)]);
        assert!(overlaid[(1, 1)].symbol().starts_with(PLACEHOLDER));
        assert_eq!(overlaid[(2, 1)].symbol(), " ");
        terminal
            .draw(|frame| render(frame, false))
            .expect("close popup");
        assert_eq!(terminal.backend().buffer(), &original);
    }

    #[test]
    fn image_cells_are_clipped_without_changing_their_source_coordinates() {
        let backend = Kitty {
            uploaded: None,
            unicode_placeholders: true,
        };
        let mut buffer = Buffer::empty(Rect::new(2, 2, 2, 2));
        backend.render(&mut buffer, &image(), None);
        assert_eq!(
            buffer[(2, 2)].symbol(),
            format!("{PLACEHOLDER}{}{}", GRID[1], GRID[1])
        );
        assert_eq!(buffer[(2, 2)].fg, Color::Indexed(1));
        assert_eq!(buffer[(3, 3)].symbol(), " ");
    }
}
