use std::{io::Write, sync::Arc};

use anyhow::Result;
use crossbeam::channel::Sender;
use ratatui::{layout::Rect, prelude::IntoCrossterm};

use super::{
    Backend,
    block::Block,
    iterm2::Iterm2,
    kitty::Kitty,
    sixel::Sixel,
    ueberzug::{Layer, Ueberzug},
};
use crate::{
    config::album_art::ImageMethod,
    ctx::Ctx,
    shared::{events::WorkRequest, macros::status_error, terminal::TERMINAL},
};

#[derive(Debug)]
pub struct AlbumArtFacade {
    image_backend: ImageBackend,
    current_album_art: Option<Arc<Vec<u8>>>,
    default_album_art: Arc<Vec<u8>>,
    last_size: Rect,
    work_tx: Sender<WorkRequest>,
    is_showing: bool,
    in_flight: Option<ImageRequest>,
    desired: Option<ImageRequest>,
    prepared: Option<(ImageRequest, EncodeData)>,
    displayed_area: Option<Rect>,
    hide_pending: bool,
    display_pending: bool,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageRequest {
    data: Arc<Vec<u8>>,
    area: Rect,
    max_size: crate::config::Size,
    horizontal_align: crate::config::album_art::HorizontalAlign,
    vertical_align: crate::config::album_art::VerticalAlign,
    revision: u64,
}

impl ImageRequest {
    fn encode(&self, method: ImageMethod) -> Result<EncodeData> {
        let Self {
            data,
            area,
            max_size,
            horizontal_align,
            vertical_align,
            ..
        } = self;
        Ok(match method {
            ImageMethod::Kitty => EncodeData::Kitty(Kitty::create_data(
                data,
                *area,
                *max_size,
                *horizontal_align,
                *vertical_align,
            )?),
            ImageMethod::Iterm2 => EncodeData::Iterm2(Iterm2::create_data(
                data,
                *area,
                *max_size,
                *horizontal_align,
                *vertical_align,
            )?),
            ImageMethod::Sixel => EncodeData::Sixel(Sixel::create_data(
                data,
                *area,
                *max_size,
                *horizontal_align,
                *vertical_align,
            )?),
            ImageMethod::Block => EncodeData::Block(Block::create_data(
                data,
                *area,
                *max_size,
                *horizontal_align,
                *vertical_align,
            )?),
            ImageMethod::UeberzugWayland | ImageMethod::UeberzugX11 => EncodeData::Ueberzug(
                Ueberzug::create_data(data, *area, *max_size, *horizontal_align, *vertical_align)?,
            ),
            ImageMethod::None => EncodeData::Empty,
        })
    }
}

#[derive(Debug, Default)]
enum ImageBackend {
    Kitty(Kitty),
    Ueberzug(Ueberzug),
    Iterm2(Iterm2),
    Sixel(Sixel),
    Block(Block),
    #[default]
    None,
}

#[derive(Debug, Default)]
pub enum EncodeData {
    Kitty(<Kitty as Backend>::EncodedData),
    Ueberzug(<Ueberzug as Backend>::EncodedData),
    Iterm2(<Iterm2 as Backend>::EncodedData),
    Sixel(<Sixel as Backend>::EncodedData),
    Block(<Block as Backend>::EncodedData),
    #[default]
    Empty,
}

impl AlbumArtFacade {
    pub fn new(ctx: &Ctx) -> Self {
        let config = ctx.config.as_ref();
        let image_backend = match config.album_art.method {
            ImageMethod::Kitty => ImageBackend::Kitty(Kitty::default()),
            ImageMethod::UeberzugWayland => ImageBackend::Ueberzug(Ueberzug::new(Layer::Wayland)),
            ImageMethod::UeberzugX11 => ImageBackend::Ueberzug(Ueberzug::new(Layer::X11)),
            ImageMethod::Iterm2 => ImageBackend::Iterm2(Iterm2),
            ImageMethod::Sixel => ImageBackend::Sixel(Sixel),
            ImageMethod::Block => ImageBackend::Block(Block),
            ImageMethod::None => ImageBackend::None,
        };

        Self {
            image_backend,
            current_album_art: None,
            last_size: Rect::default(),
            default_album_art: Arc::new(config.theme.default_album_art.to_vec()),
            work_tx: ctx.work_sender.clone(),
            is_showing: false,
            in_flight: None,
            desired: None,
            prepared: None,
            displayed_area: None,
            hide_pending: false,
            display_pending: false,
            revision: 0,
        }
    }

    pub fn show_default(&mut self, ctx: &Ctx) -> Result<()> {
        self.current_album_art = Some(Arc::clone(&self.default_album_art));
        self.show_current(ctx)
    }

    pub fn show_current(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(current_album_art) = self.current_album_art.as_ref().map(Arc::clone) else {
            log::warn!("Tried to display current album art but none was present");
            return Ok(());
        };

        self.show(current_album_art, ctx)?;
        return Ok(());
    }

    pub fn has_current(&self) -> bool {
        self.current_album_art.is_some()
    }

    pub fn supports_overlay(&self) -> bool {
        matches!(&self.image_backend, ImageBackend::Kitty(backend) if backend.uses_placeholders())
    }

    pub fn show(&mut self, data: impl Into<Arc<Vec<u8>>>, ctx: &Ctx) -> Result<()> {
        let data = data.into();
        self.current_album_art = Some(Arc::clone(&data));
        if matches!(self.image_backend, ImageBackend::None) {
            return Ok(());
        }
        self.is_showing = true;
        let request = ImageRequest {
            data,
            area: self.last_size,
            max_size: ctx.config.album_art.max_size_px,
            horizontal_align: ctx.config.album_art.horizontal_align,
            vertical_align: ctx.config.album_art.vertical_align,
            revision: self.revision,
        };
        self.desired = Some(request.clone());
        self.display_pending = self
            .prepared
            .as_ref()
            .is_some_and(|(key, _)| key == &request);
        if self.display_pending {
            ctx.render()?;
        } else if self.in_flight.is_none() {
            self.encode(request)?;
        }
        Ok(())
    }

    fn encode(&mut self, request: ImageRequest) -> Result<()> {
        let method = match self.image_backend {
            ImageBackend::Kitty(_) => ImageMethod::Kitty,
            ImageBackend::Iterm2(_) => ImageMethod::Iterm2,
            ImageBackend::Sixel(_) => ImageMethod::Sixel,
            ImageBackend::Block(_) => ImageMethod::Block,
            ImageBackend::Ueberzug(_) => ImageMethod::UeberzugWayland,
            ImageBackend::None => return Ok(()),
        };
        let work = request.clone();
        self.work_tx
            .send(WorkRequest::ResizeImage(Box::new(move || {
                work.encode(method)
            })))?;
        self.in_flight = Some(request);
        Ok(())
    }

    fn encode_latest(&mut self) -> Result<()> {
        if self.is_showing
            && let Some(request) = self.desired.as_ref()
            && self.prepared.as_ref().is_none_or(|(key, _)| key != request)
        {
            self.encode(request.clone())?;
        }
        Ok(())
    }

    pub fn image_processing_failed(&mut self, err: &anyhow::Error, _ctx: &Ctx) -> Result<()> {
        let failed = self.in_flight.take();
        if failed.as_ref() != self.desired.as_ref() {
            self.encode_latest()?;
        } else if self.is_showing {
            status_error!("Failed to process album art image: {err:?}");
        }
        Ok(())
    }

    /// Store completed work; terminal writes belong to the next synchronized frame.
    pub fn display(&mut self, data: EncodeData, ctx: &Ctx) -> Result<()> {
        let Some(request) = self.in_flight.take() else {
            return Ok(());
        };
        if self.desired.as_ref() == Some(&request) {
            self.prepared = Some((request, data));
            self.display_pending = self.is_showing;
            if self.display_pending {
                ctx.render()?;
            }
        } else {
            self.encode_latest()?;
        }
        Ok(())
    }

    pub fn hide(&mut self, ctx: &Ctx) -> Result<()> {
        if self.is_showing {
            self.is_showing = false;
            self.hide_pending = self.displayed_area.is_some();
            self.display_pending = false;
            ctx.render()?;
        }
        Ok(())
    }

    /// Remove the old image before Ratatui draws its replacement in the same frame.
    pub fn prepare_frame(&mut self, ctx: &Ctx) -> Result<Option<Rect>> {
        if !self.hide_pending && !self.display_pending {
            return Ok(None);
        }
        self.hide_pending = false;
        let Some(area) = self.displayed_area.take() else {
            return Ok(None);
        };
        let writer = TERMINAL.writer();
        let mut writer = writer.lock();
        let background = ctx
            .config
            .theme
            .background_color
            .map(|color| color.into_crossterm());
        match &mut self.image_backend {
            ImageBackend::Kitty(backend) => backend.hide(writer.by_ref(), area, background)?,
            ImageBackend::Iterm2(backend) => backend.hide(writer.by_ref(), area, background)?,
            ImageBackend::Sixel(backend) => backend.hide(writer.by_ref(), area, background)?,
            ImageBackend::Block(backend) => backend.hide(writer.by_ref(), area, background)?,
            ImageBackend::Ueberzug(backend) => backend.hide(writer.by_ref(), area, background)?,
            ImageBackend::None => {}
        }
        Ok(Some(area))
    }

    /// Paint prepared graphics after Ratatui, before the frame is presented.
    pub fn finish_frame(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.is_showing || !self.display_pending {
            return Ok(());
        }
        self.display_pending = false;
        let Some((request, data)) = self
            .prepared
            .as_ref()
            .filter(|(key, _)| self.desired.as_ref() == Some(key))
        else {
            return Ok(());
        };
        let writer = TERMINAL.writer();
        let mut writer = writer.lock();
        match (&mut self.image_backend, data) {
            (ImageBackend::Kitty(backend), EncodeData::Kitty(data)) => {
                backend.display(writer.by_ref(), data, ctx)?;
            }
            (ImageBackend::Iterm2(backend), EncodeData::Iterm2(data)) => {
                backend.display(writer.by_ref(), data, ctx)?;
            }
            (ImageBackend::Sixel(backend), EncodeData::Sixel(data)) => {
                backend.display(writer.by_ref(), data, ctx)?;
            }
            (ImageBackend::Block(backend), EncodeData::Block(data)) => {
                backend.display(writer.by_ref(), data, ctx)?;
            }
            (ImageBackend::Ueberzug(backend), EncodeData::Ueberzug(data)) => {
                backend.display(writer.by_ref(), data, ctx)?;
            }
            _ => return Ok(()),
        }
        self.displayed_area = Some(request.area);
        Ok(())
    }

    pub fn render(&self, buffer: &mut ratatui::buffer::Buffer, ctx: &Ctx) {
        if self.is_showing
            && let ImageBackend::Kitty(backend) = &self.image_backend
            && backend.uses_placeholders()
        {
            if let Some((_, EncodeData::Kitty(data))) = self
                .prepared
                .as_ref()
                .filter(|(key, _)| self.desired.as_ref() == Some(key))
            {
                backend.render(buffer, data, ctx.config.theme.background_color);
            }
            return;
        }
        if self.is_showing
            && let Some(area) = self.displayed_area
        {
            for position in area.intersection(*buffer.area()).positions() {
                buffer[position].set_diff_option(ratatui::buffer::CellDiffOption::Skip);
            }
        }
    }

    pub fn cleanup(&mut self, ctx: &Ctx) -> Result<()> {
        self.hide_pending = true;
        self.prepare_frame(ctx)?;
        let state = std::mem::take(&mut self.image_backend);
        self.is_showing = false;
        match state {
            ImageBackend::Kitty(backend) => Box::new(backend).cleanup(self.last_size),
            ImageBackend::Iterm2(backend) => Box::new(backend).cleanup(self.last_size),
            ImageBackend::Sixel(backend) => Box::new(backend).cleanup(self.last_size),
            ImageBackend::Block(backend) => Box::new(backend).cleanup(self.last_size),
            ImageBackend::Ueberzug(backend) => Box::new(backend).cleanup(self.last_size),
            ImageBackend::None => Ok(()),
        }
    }

    pub fn set_size(&mut self, area: Rect) {
        self.last_size = area;
    }

    pub fn redraw_current(&mut self, ctx: &Ctx) -> Result<()> {
        // A newly attached terminal may not retain the previous graphics data.
        if let ImageBackend::Kitty(backend) = &mut self.image_backend {
            backend.invalidate();
        }
        self.show_current(ctx)
    }

    pub fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        self.revision = self.revision.wrapping_add(1);
        self.last_size = area;
        self.hide_pending = self.displayed_area.is_some();
        self.show_current(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Size,
        tests::fixtures::{ctx, work_request_channel},
    };
    use crossbeam::channel::Receiver;
    use rstest::rstest;

    fn configured(ctx: &mut Ctx) -> AlbumArtFacade {
        let config = Arc::make_mut(&mut ctx.config);
        config.album_art.method = ImageMethod::Kitty;
        config.album_art.max_size_px = Size {
            width: 32,
            height: 32,
        };
        let mut facade = AlbumArtFacade::new(ctx);
        facade.set_size(Rect::new(1, 1, 8, 4));
        facade
    }

    fn complete(rx: &Receiver<WorkRequest>, facade: &mut AlbumArtFacade, ctx: &Ctx) {
        let WorkRequest::ResizeImage(encode) = rx.try_recv().expect("image encode") else {
            panic!("expected image encode")
        };
        facade
            .display(encode().expect("encode image"), ctx)
            .expect("receive image");
    }

    #[rstest]
    fn returning_to_the_same_cover_reuses_the_prepared_image(
        mut ctx: Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0.clone();
        let mut facade = configured(&mut ctx);
        facade.show_default(&ctx).expect("show cover");
        complete(&work_request_channel.1, &mut facade, &ctx);
        for _ in 0..5 {
            facade.hide(&ctx).expect("hide cover");
            facade.show_current(&ctx).expect("restore cover");
        }
        assert!(
            work_request_channel.1.is_empty(),
            "cached cover must not be recompressed"
        );
        assert!(facade.display_pending);
    }

    #[rstest]
    fn an_encode_completed_while_hidden_is_cached_without_being_painted(
        mut ctx: Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0.clone();
        let mut facade = configured(&mut ctx);
        facade.show_default(&ctx).expect("show cover");
        facade
            .hide(&ctx)
            .expect("switch tab before encode completes");
        complete(&work_request_channel.1, &mut facade, &ctx);
        assert!(!facade.display_pending);
        facade.show_current(&ctx).expect("return to queue");
        assert!(facade.display_pending);
        assert!(work_request_channel.1.is_empty());
    }

    #[rstest]
    fn stale_encodes_are_coalesced_and_never_presented_at_the_old_size(
        mut ctx: Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0.clone();
        let mut facade = configured(&mut ctx);
        facade.show_default(&ctx).expect("initial cover");
        let area = Rect::new(2, 2, 6, 3);
        facade.set_size(area);
        facade
            .show_current(&ctx)
            .expect("new size while encode is running");
        assert_eq!(work_request_channel.1.len(), 1);
        complete(&work_request_channel.1, &mut facade, &ctx);
        assert!(!facade.display_pending);
        assert!(facade.prepared.is_none());
        complete(&work_request_channel.1, &mut facade, &ctx);
        assert_eq!(
            facade.prepared.as_ref().expect("prepared image").0.area,
            area
        );
        assert!(facade.display_pending);
        assert!(work_request_channel.1.is_empty());
    }

    #[rstest]
    fn a_font_resize_reencodes_even_if_the_cell_rectangle_is_unchanged(
        mut ctx: Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0.clone();
        let mut facade = configured(&mut ctx);
        facade.show_default(&ctx).expect("initial cover");
        complete(&work_request_channel.1, &mut facade, &ctx);
        facade.resize(facade.last_size, &ctx).expect("font resize");
        assert!(!facade.display_pending);
        assert_eq!(work_request_channel.1.len(), 1);
    }
}
