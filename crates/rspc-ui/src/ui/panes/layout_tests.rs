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
