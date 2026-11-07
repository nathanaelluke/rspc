use std::{collections::HashMap, sync::Arc, time::Duration};

use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use rstest::rstest;

use super::{Pane, PropertyPane, queue::QueuePane, queue_header::QueueHeaderPane};
use crate::{
    config::{
        tabs::PaneType,
        theme::{UiConfigFile, borders::BorderSetLib},
    },
    ctx::Ctx,
    mpd::commands::{Song, State, Volume},
    tests::fixtures::ctx,
};

#[rstest]
#[case(80, State::Pause, "Paused", 7, "0:07", 1)]
#[case(120, State::Play, "Playing", 3607, "1:00:07", 123)]
#[case(80, State::Stop, "Stopped", 0, "0:00", 12)]
fn bar_gutters_follow_content_at_different_widths(
    #[case] width: u16,
    #[case] state: State,
    #[case] label: &str,
    #[case] elapsed: u64,
    #[case] formatted_elapsed: &str,
    #[case] count: usize,
    mut ctx: Ctx,
) {
    let theme: UiConfigFile =
        ron::from_str(include_str!("../../../../../config/themes/default.ron"))
            .expect("rspc theme must parse");
    Arc::make_mut(&mut ctx.config).theme = theme.try_into().expect("rspc theme must convert");
    ctx.status.state = state;
    ctx.status.elapsed = Duration::from_secs(elapsed);
    ctx.status.duration = Duration::from_secs(5000);
    ctx.status.volume = Volume::new(100);
    ctx.status.songid = Some(0);
    ctx.queue = (0..count)
        .map(|id| Song {
            id: id as u32,
            ..Song::default()
        })
        .collect();

    let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("test terminal");
    terminal
        .draw(|frame| {
            ctx.config
                .theme
                .layout
                .for_each_pane(
                    frame.area(),
                    &mut |pane, area, _, _, _| {
                        if let PaneType::Property {
                            content,
                            align,
                            scroll_speed,
                            wrap,
                            vertical_align,
                        } = &pane.pane
                        {
                            PropertyPane::new(
                                content,
                                *align,
                                u64::from(*scroll_speed),
                                *wrap,
                                *vertical_align,
                            )
                            .render(frame, area, &ctx)?;
                        }
                        Ok(())
                    },
                    &ctx,
                )
                .expect("render theme properties");
        })
        .expect("draw terminal");
    let buffer = terminal.backend().buffer();
    let row = |y| {
        (0..width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let header = row(0);
    let footer = row(7);
    assert!(header.starts_with(&format!(" [{label}] ")), "{header:?}");
    assert!(header.ends_with(" VOL 100% "), "{header:?}");
    assert_eq!(header.matches('│').count(), 2, "{header:?}");
    assert!(header.starts_with(&format!(" [{label}] │")), "{header:?}");
    assert!(header.ends_with("│ VOL 100% "), "{header:?}");
    assert!(
        footer.starts_with(&format!(" NORMAL [zxcv] │ {formatted_elapsed} ")),
        "{footer:?}"
    );
    assert!(
        footer.ends_with(&format!(" 1:23:20 │ {count} tracks ")),
        "{footer:?}"
    );
}

#[rstest]
fn auto_size_rejects_non_property_panes_and_vertical_splits() {
    use crate::config::tabs::PaneOrSplitFile;

    for text in [
        "Split(direction:Horizontal,panes:[(size:\"auto\",pane:Pane(Queue))])",
        "Split(direction:Vertical,panes:[(size:\"auto\",pane:Pane(Property(content:[])))])",
    ] {
        let pane: PaneOrSplitFile = ron::from_str(text).expect("layout syntax");
        let error = pane
            .convert(&HashMap::default(), &BorderSetLib::default())
            .expect_err("unsupported auto layout");
        assert!(
            error
                .to_string()
                .contains("size auto requires a Property pane")
        );
    }
}

#[rstest]
#[case(50, false)]
#[case(80, true)]
#[case(120, false)]
fn queue_headings_and_rows_share_gutters_and_edges(
    #[case] width: u16,
    #[case] scrollbar: bool,
    mut ctx: Ctx,
) {
    let file: UiConfigFile =
        ron::from_str(include_str!("../../../../../config/themes/default.ron"))
            .expect("rspc theme");
    let theme = &mut Arc::make_mut(&mut ctx.config).theme;
    *theme = file.try_into().expect("convert theme");
    if scrollbar {
        theme.scrollbar = crate::config::theme::UiConfig::default().scrollbar;
    }
    ctx.queue = vec![Song {
        duration: Some(Duration::from_secs(225)),
        metadata: HashMap::from([
            ("artist".into(), "A".repeat(200).into()),
            ("title".into(), "T".repeat(200).into()),
            ("album".into(), "B".repeat(200).into()),
        ]),
        ..Song::default()
    }];
    let mut header = QueueHeaderPane::new(&ctx);
    let mut queue = QueuePane::new(&ctx);
    let mut terminal = Terminal::new(TestBackend::new(width, 4)).expect("test terminal");
    terminal
        .draw(|frame| {
            header
                .render(frame, Rect::new(0, 0, width, 1), &ctx)
                .expect("header");
            queue
                .render(frame, Rect::new(0, 1, width, 3), &ctx)
                .expect("rows");
        })
        .expect("draw queue");
    let buffer = terminal.backend().buffer();
    let row = |y| {
        (0..width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let heading = row(0);
    let song = row(1);
    assert!(song.starts_with('A'), "{song:?}");
    for (label, symbol) in [("Title", 'T'), ("Album", 'B')] {
        let start = song.find(symbol).expect("song column");
        assert_eq!(heading.find(label), Some(start), "{heading:?}\n{song:?}");
        assert_eq!(&song[start - 2..start], "  ", "{song:?}");
    }
    let right = usize::from(width - if scrollbar { 2 } else { 0 });
    assert!(heading[..right].ends_with("Time"), "{heading:?}");
    assert!(song[..right].ends_with("3:45"), "{song:?}");
}
