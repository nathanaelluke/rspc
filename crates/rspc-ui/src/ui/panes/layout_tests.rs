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
    mpd::commands::Song,
    tests::fixtures::ctx,
};

#[rstest]
#[case(40)]
#[case(80)]
#[case(120)]
fn auto_sized_properties_leave_space_for_the_flexible_pane(#[case] width: u16, mut ctx: Ctx) {
    let file: UiConfigFile = ron::from_str(
        r#"#![enable(implicit_some)]
        (layout: Split(direction: Horizontal, panes: [
            (size: "auto", pane: Pane(Property(content: [(kind: Text("LEFT 123 "))]))),
            (size: "100%", pane: Pane(Empty())),
            (size: "auto", pane: Pane(Property(content: [(kind: Text(" RIGHT"))], align: Right))),
        ]))"#,
    )
    .expect("fixture theme must parse");
    Arc::make_mut(&mut ctx.config).theme = file.try_into().expect("fixture theme must convert");
    let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
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
                .expect("render fixture properties");
        })
        .expect("draw terminal");
    let buffer = terminal.backend().buffer();
    let row: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
    assert!(row.starts_with("LEFT 123 "), "{row:?}");
    assert!(row.ends_with(" RIGHT"), "{row:?}");
    assert_eq!(
        row.trim(),
        format!("LEFT 123{}RIGHT", " ".repeat(usize::from(width) - 13))
    );
}

fn load_default_theme(ctx: &mut Ctx) {
    let theme: UiConfigFile =
        ron::from_str(include_str!("../../../../../config/themes/default.ron"))
            .expect("default theme");
    Arc::make_mut(&mut ctx.config).theme = theme.try_into().expect("convert default theme");
}

fn render_bars(ctx: &Ctx, width: u16) -> (Vec<Rect>, [String; 2]) {
    use super::progress_bar::ProgressBarPane;

    let mut terminal = Terminal::new(TestBackend::new(width, 35)).expect("test terminal");
    let mut areas = Vec::new();
    terminal
        .draw(|frame| {
            ctx.config
                .theme
                .layout
                .for_each_pane(
                    frame.area(),
                    &mut |pane, area, _, _, _| {
                        if area.y != 0 && area.y != 34 {
                            return Ok(());
                        }
                        areas.push(area);
                        match &pane.pane {
                            PaneType::Property {
                                content,
                                align,
                                scroll_speed,
                                wrap,
                                vertical_align,
                            } => {
                                PropertyPane::new(
                                    content,
                                    *align,
                                    u64::from(*scroll_speed),
                                    *wrap,
                                    *vertical_align,
                                )
                                .render(frame, area, ctx)?;
                            }
                            PaneType::ProgressBar => {
                                ProgressBarPane::new().render(frame, area, ctx)?;
                            }
                            _ => {}
                        }
                        Ok(())
                    },
                    ctx,
                )
                .expect("render bars");
        })
        .expect("draw bars");
    areas.sort_by_key(|area| (area.y, area.x));
    let rows = [0, 34].map(|y| {
        (0..width)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
            .collect()
    });
    (areas, rows)
}

#[rstest]
#[case(80)]
#[case(120)]
#[case(180)]
fn playback_and_volume_changes_do_not_move_the_bars(#[case] width: u16, mut ctx: Ctx) {
    use crate::mpd::commands::{State, Volume};

    load_default_theme(&mut ctx);
    let (initial, _) = render_bars(&ctx, width);
    for (state, label) in [
        (State::Play, "PLAYING"),
        (State::Pause, "PAUSED"),
        (State::Stop, "STOPPED"),
    ] {
        ctx.status.state = state;
        for volume in [0, 9, 10, 99, 100] {
            ctx.status.volume = Volume::new(volume);
            let (areas, [header, _]) = render_bars(&ctx, width);
            assert_eq!(
                areas, initial,
                "state {state:?}, volume {volume} moved a section"
            );
            assert_eq!(
                header.split('|').next().expect("state section").trim(),
                label
            );
            assert_eq!(
                header.rsplit('|').next().expect("volume section").trim(),
                format!("VOL {volume}%")
            );
        }
    }
}

#[rstest]
#[case(80)]
#[case(120)]
#[case(180)]
fn commands_replace_the_mode_without_moving_the_footer(#[case] width: u16, mut ctx: Ctx) {
    use crate::{
        config::keys::{CommonAction, Key},
        shared::keys::KeyResolver,
    };
    use crossterm::event::{KeyCode, KeyModifiers};

    load_default_theme(&mut ctx);
    let config = Arc::make_mut(&mut ctx.config);
    let key = |ch| Key {
        key: KeyCode::Char(ch),
        modifiers: KeyModifiers::NONE,
    };
    config.keybinds.navigation.insert(
        "gabcdefghijz".chars().map(key).collect::<Vec<_>>().into(),
        CommonAction::Right,
    );
    ctx.key_resolver = KeyResolver::new(&ctx.config);
    ctx.queue.resize_with(3, Song::default);
    let (initial, [_, before]) = render_bars(&ctx, width);
    let mode_x = before.find("NORMAL").expect("mode label");
    let mut typed = String::new();
    for ch in "gabcdefghij".chars() {
        typed.push(ch);
        ctx.key_resolver.handle_key_event(key(ch), &ctx);
        assert_eq!(ctx.key_resolver.buffer_to_string(), typed);
        let (areas, [_, footer]) = render_bars(&ctx, width);
        assert_eq!(areas, initial, "typing moved a section");
        let expected = &typed[typed.len().saturating_sub(6)..];
        assert_eq!(footer[mode_x..mode_x + 6].trim(), expected);
        assert_eq!(
            &footer[mode_x + 6..],
            &before[mode_x + 6..],
            "typing changed the flags or progress area"
        );
    }
    ctx.key_resolver.handle_key_event(key('z'), &ctx);
    assert_eq!(ctx.key_resolver.buffer_to_string(), "");
    assert_eq!(render_bars(&ctx, width).1[1], before);
    ctx.key_resolver.handle_key_event(key('g'), &ctx);
    ctx.key_resolver.handle_timeout(&ctx);
    assert_eq!(ctx.key_resolver.buffer_to_string(), "");
    assert_eq!(render_bars(&ctx, width).1[1], before);
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
    let file: UiConfigFile = ron::from_str(
        r#"#![enable(implicit_some)]
        #![enable(unwrap_newtypes)]
        #![enable(unwrap_variant_newtypes)]
        (
            scrollbar: None,
            song_table_column_spacing: 2,
            song_table_format: [
                (prop: (kind: Property(Artist)), label_prop: (kind: Text("Artist")), width: "24%"),
                (prop: (kind: Property(Title)), label_prop: (kind: Text("Title")), width: "36%"),
                (prop: (kind: Property(Album)), label_prop: (kind: Text("Album")), width: "32%"),
                (prop: (kind: Property(Duration)), label_prop: (kind: Text("Time")), width: "8%", alignment: Right),
            ],
        )"#,
    )
    .expect("queue fixture theme");
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
