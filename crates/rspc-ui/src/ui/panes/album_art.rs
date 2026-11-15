use anyhow::Result;
use ratatui::{Frame, layout::Rect};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{album_art::ImageMethod, tabs::PaneType},
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::keys::ActionEvent,
    ui::{UiEvent, image::facade::AlbumArtFacade},
};

#[derive(Debug)]
pub struct AlbumArtPane {
    album_art: AlbumArtFacade,
    fetch_needed: bool,
    song_file: Option<String>,
}

const ALBUM_ART: &str = "album_art";

impl AlbumArtPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            album_art: AlbumArtFacade::new(ctx),
            fetch_needed: true,
            song_file: None,
        }
    }

    pub fn prepare_frame(&mut self, ctx: &Ctx) -> Result<Option<Rect>> {
        self.album_art.prepare_frame(ctx)
    }

    pub fn finish_frame(&mut self, ctx: &Ctx) -> Result<()> {
        self.album_art.finish_frame(ctx)
    }

    /// returns none if album art is supposed to be hidden
    fn fetch_album_art(ctx: &Ctx) -> Option<()> {
        if matches!(ctx.config.album_art.method, ImageMethod::None) {
            return None;
        }

        let (_, current_song) = ctx.find_current_song_in_queue()?;

        let disabled_protos = &ctx.config.album_art.disabled_protocols;
        let song_uri = current_song.file.as_str();
        if disabled_protos
            .iter()
            .any(|proto| song_uri.starts_with(proto))
        {
            log::debug!(uri = song_uri; "Not downloading album art because the protocol is disabled");
            return None;
        }

        let song_uri = song_uri.to_owned();
        let order = ctx.config.album_art.order;
        ctx.query().id(ALBUM_ART).replace_id(ALBUM_ART).target(PaneType::AlbumArt).query(move |client| {
            let start = std::time::Instant::now();
            log::debug!(file = song_uri.as_str(); "Searching for album art");
            let result = client.find_album_art(&song_uri, order)?;
            log::debug!(elapsed:? = start.elapsed(), size = result.as_ref().map(|v|v.len()); "Found album art");

            Ok(MpdQueryResult::AlbumArt { song: song_uri, data: result })
        });

        Some(())
    }

    fn prepare_art(&mut self, ctx: &Ctx) -> Result<()> {
        let song = ctx
            .find_current_song_in_queue()
            .map(|(_, song)| song.file.clone());
        if !self.fetch_needed && self.song_file == song && self.album_art.has_current() {
            return self.album_art.prepare_current(ctx);
        }
        self.song_file = song;
        self.fetch_needed = Self::fetch_album_art(ctx).is_some();
        if !self.fetch_needed {
            self.album_art.prepare_default(ctx)?;
        }
        Ok(())
    }
}

impl Pane for AlbumArtPane {
    fn prefetch(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        self.album_art.set_size(area);
        self.prepare_art(ctx)
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.album_art.set_size(area);
        self.album_art.render(frame.buffer_mut(), ctx);
        Ok(())
    }

    fn calculate_areas(&mut self, area: Rect, _ctx: &Ctx) -> Result<()> {
        self.album_art.set_size(area);
        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }

    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.album_art.hide(ctx)
    }

    fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        self.album_art.resize(area, ctx)
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        self.prepare_art(ctx)?;
        if !self.fetch_needed {
            self.album_art.show_current(ctx)?;
        }
        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, data) {
            (ALBUM_ART, MpdQueryResult::AlbumArt { song, data }) => {
                if self.song_file.as_deref() != Some(song.as_str())
                    || ctx
                        .find_current_song_in_queue()
                        .map(|(_, song)| song.file.as_str())
                        != Some(song.as_str())
                {
                    return Ok(());
                }
                self.fetch_needed = false;
                match (data, is_visible) {
                    (Some(data), true) => self.album_art.show(data, ctx)?,
                    (Some(data), false) => self.album_art.prepare(data, ctx)?,
                    (None, true) => self.album_art.show_default(ctx)?,
                    (None, false) => self.album_art.prepare_default(ctx)?,
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::SongChanged | UiEvent::Reconnected => {
                self.fetch_needed = true;
                self.prepare_art(ctx)?;
                if is_visible && !self.fetch_needed {
                    self.album_art.show_current(ctx)?;
                }
            }
            UiEvent::Displayed if is_visible => {
                self.album_art.redraw_current(ctx)?;
            }
            UiEvent::ModalOpened if is_visible => {
                self.album_art.set_modal_open(true, ctx)?;
            }
            UiEvent::ModalClosed if is_visible => {
                self.album_art.set_modal_open(false, ctx)?;

                if self.fetch_needed {
                    self.before_show(ctx)?;
                    return Ok(());
                }
            }
            UiEvent::ConfigChanged => {
                self.fetch_needed = true;
                if is_visible {
                    self.album_art.show_current(ctx)?;
                }
            }
            UiEvent::Exit => {
                self.album_art.cleanup(ctx)?;
            }
            UiEvent::ImageEncoded { data } => {
                self.album_art.display(std::mem::take(data), ctx)?;
            }
            UiEvent::ImageEncodeFailed { err } => {
                self.album_art.image_processing_failed(err, ctx)?;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
    use rstest::rstest;

    use super::AlbumArtPane;
    use crate::{
        config::{Config, album_art::ImageMethod, tabs::PaneType},
        mpd::commands::{Song, State},
        shared::{
            events::{AppEvent, ClientRequest, WorkRequest},
            mpd_query::MpdQuery,
        },
        tests::fixtures::{app_event_channel, client_request_channel, ctx, work_request_channel},
        ui::{
            UiEvent,
            panes::{Pane, album_art::ALBUM_ART},
        },
    };

    #[rstest]
    fn song_changes_fetch_covers_while_the_queue_is_hidden(
        mut ctx: crate::ctx::Ctx,
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        ctx.client_request_sender = client_request_channel.0;
        std::sync::Arc::make_mut(&mut ctx.config).album_art.method = ImageMethod::Block;
        ctx.queue.push(Song {
            id: 1,
            file: "new-song.flac".into(),
            ..Song::default()
        });
        ctx.status.songid = Some(1);
        ctx.status.state = State::Play;
        let mut pane = AlbumArtPane::new(&ctx);
        pane.on_event(&mut crate::ui::UiEvent::SongChanged, false, &ctx)
            .expect("prepare hidden cover");
        assert!(matches!(
            client_request_channel.1.try_recv(),
            Ok(ClientRequest::Query(MpdQuery { id: ALBUM_ART, .. }))
        ));
    }

    #[rstest]
    fn a_hidden_cover_result_is_prepared_before_the_first_visit(
        mut ctx: crate::ctx::Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0;
        std::sync::Arc::make_mut(&mut ctx.config).album_art.method = ImageMethod::Block;
        let mut pane = AlbumArtPane::new(&ctx);
        ctx.queue.push(Song {
            id: 1,
            file: "cover-test.flac".into(),
            ..Song::default()
        });
        ctx.status.songid = Some(1);
        ctx.status.state = State::Play;
        pane.song_file = Some("cover-test.flac".into());
        pane.calculate_areas(ratatui::layout::Rect::new(0, 0, 20, 10), &ctx)
            .unwrap();
        pane.on_query_finished(
            ALBUM_ART,
            crate::MpdQueryResult::AlbumArt {
                song: "cover-test.flac".into(),
                data: None,
            },
            false,
            &ctx,
        )
        .unwrap();
        assert!(!pane.fetch_needed);
        assert!(pane.album_art.has_current());
        assert!(matches!(
            work_request_channel.1.try_recv(),
            Ok(WorkRequest::ResizeImage(_))
        ));
    }

    #[rstest]
    fn late_artwork_for_a_previous_song_does_not_replace_the_current_cover(
        mut ctx: crate::ctx::Ctx,
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    ) {
        ctx.work_sender = work_request_channel.0;
        ctx.queue.push(Song {
            id: 1,
            file: "current.flac".into(),
            ..Song::default()
        });
        ctx.status.songid = Some(1);
        ctx.status.state = State::Play;
        let mut pane = AlbumArtPane::new(&ctx);
        pane.song_file = Some("current.flac".into());
        pane.on_query_finished(
            ALBUM_ART,
            crate::MpdQueryResult::AlbumArt {
                song: "previous.flac".into(),
                data: None,
            },
            false,
            &ctx,
        )
        .unwrap();
        assert!(pane.fetch_needed);
        assert!(!pane.album_art.has_current());
        assert!(work_request_channel.1.is_empty());
    }

    #[rstest]
    #[case(ImageMethod::Kitty, true)]
    #[case(ImageMethod::None, false)]
    fn searches_for_album_art_before_show(
        #[case] method: ImageMethod,
        #[case] should_search: bool,
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let rx = client_request_channel.1.clone();
        let mut ctx = ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        );
        let selected_song_id = 333;
        let mut config = Config::default();
        config.album_art.method = method;
        ctx.config = std::sync::Arc::new(config);
        ctx.queue.push(Song {
            id: selected_song_id,
            ..Default::default()
        });
        ctx.status.songid = Some(selected_song_id);
        ctx.status.state = State::Play;
        let mut screen = AlbumArtPane::new(&ctx);

        screen.before_show(&ctx).unwrap();

        if should_search {
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(100)).unwrap(),
                ClientRequest::Query(MpdQuery {
                    id: ALBUM_ART,
                    replace_id: Some(ALBUM_ART),
                    target: Some(PaneType::AlbumArt),
                    ..
                })
            ));
        } else {
            assert!(
                rx.recv_timeout(Duration::from_millis(100))
                    .is_err_and(|err| RecvTimeoutError::Timeout == err)
            );
        }
    }

    #[rstest]
    #[case(ImageMethod::Kitty, true)]
    #[case(ImageMethod::None, false)]
    fn searches_for_album_art_on_event(
        #[case] method: ImageMethod,
        #[case] should_search: bool,
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let rx = client_request_channel.1.clone();
        let mut ctx = ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        );
        let selected_song_id = 333;
        let mut config = Config::default();
        config.album_art.method = method;
        ctx.config = std::sync::Arc::new(config);
        ctx.queue.push(Song {
            id: selected_song_id,
            ..Default::default()
        });
        ctx.status.songid = Some(selected_song_id);
        ctx.status.state = State::Play;
        let mut screen = AlbumArtPane::new(&ctx);

        screen
            .on_event(&mut UiEvent::SongChanged, true, &ctx)
            .unwrap();

        if should_search {
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(100)).unwrap(),
                ClientRequest::Query(MpdQuery {
                    id: ALBUM_ART,
                    replace_id: Some(ALBUM_ART),
                    target: Some(PaneType::AlbumArt),
                    ..
                })
            ));
        } else {
            let result = rx.recv_timeout(Duration::from_millis(100));
            assert!(result.is_err_and(|err| RecvTimeoutError::Timeout == err));
        }
    }

    #[rstest]
    fn tab_returns_only_reuse_a_completed_album_art_query(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let queries = client_request_channel.1.clone();
        let _work_receiver = work_request_channel.1.clone();
        let mut ctx = ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        );
        std::sync::Arc::make_mut(&mut ctx.config).album_art.method = ImageMethod::Kitty;
        ctx.queue.push(Song {
            id: 1,
            file: "cover-test.flac".into(),
            ..Song::default()
        });
        ctx.status.songid = Some(1);
        ctx.status.state = State::Play;
        let mut pane = AlbumArtPane::new(&ctx);
        pane.before_show(&ctx).expect("initial art query");
        assert_eq!(queries.try_iter().count(), 1);
        // A request that has not completed may have failed. Returning must retry.
        pane.before_show(&ctx).expect("retry incomplete query");
        assert_eq!(queries.try_iter().count(), 1);
        pane.on_query_finished(
            ALBUM_ART,
            crate::MpdQueryResult::AlbumArt {
                song: "cover-test.flac".into(),
                data: None,
            },
            true,
            &ctx,
        )
        .expect("completed query");
        pane.before_show(&ctx).expect("reuse completed art query");
        assert_eq!(queries.try_iter().count(), 0);
    }
}
