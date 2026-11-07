# Customizing rspc

This guide is for people editing their setup and for agents asked to change its
appearance. Start with configuration. Most changes to colors, spacing, pane
order, song details, borders and controls do not need Rust edits.

## Find the files in use

The launcher uses `${XDG_CONFIG_HOME:-~/.config}/rspc/config.ron` unless the user
passes `--config`. That file selects a theme with `theme: Some("name")`.
The theme is normally `themes/name.ron` beside the config. An explicit theme
passed after `--` takes precedence:

```sh
rspc --config /path/to/config.ron -- --theme /path/to/theme.ron
```

Read the active files before editing. An installed config may differ from the
repository defaults, and the launcher never overwrites it. For repository-wide
changes, edit `config/config.ron` and `config/themes/default.ron`. Do not copy a
person's installed configuration back into the repo.

| Change | File and field |
| --- | --- |
| Palette, text and selection colors | Theme: `background_color`, `text_color`, `current_item_style`, `highlighted_item_style` |
| Popup colors | Theme: `modal_background_color`, `modal_backdrop`, `borders_style` |
| Tabs, their order and the contents of each tab | Main config: `tabs` |
| Header, footer and reusable pane groups | Theme: `layout`, `components` |
| Tab padding and selection background | Theme: `tab_bar` |
| Queue columns and their headings | Theme: `song_table_format`, `song_table_column_spacing` |
| Browser column proportions and song rows | Theme: `browser_column_widths`, `browser_song_format` |
| Progress bar | Theme: `progress_bar` and the `ProgressBar` pane in `layout` |
| Volume text or slider | Theme: the `volume` component; `Volume` pane for a slider |
| Cover size and placement | Main config: `album_art`; theme/tab layout: `AlbumArt` pane |
| Scrollbars | Theme: `scrollbar` (`None` hides them) |
| Help-menu selection | Theme: `keybinds_selection_style` |
| Keys, volume increments, mouse and scrolling | Main config: `keybinds`, `volume_step`, `enable_mouse`, `scrolloff`, `scroll_amount` |
| Playlist exclusions | Main config: `spotify.exclude_folders`, `spotify.exclude_playlists` |

RON field names are case sensitive. The examples below are complete RON objects
so tests can parse them, but they are **examples to merge into existing files**.
Replacing an entire file with a small example would reset omitted settings.

## Colors and text styles

Colors accept hexadecimal values such as `"#d4d4d4"` and named terminal colors.
Styles use `fg`, `bg` and `modifiers`; combine modifiers with `|`, for example
`"Bold | Italic"`. Leaving a background unset allows the surrounding pane's
background to show through.

```ron
// theme.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    background_color: Some("#181614"),
    text_color: Some("#d7cec2"),
    modal_background_color: Some("#211e1b"),
    borders_style: (fg: "#625b51"),
    current_item_style: (fg: "#fff1db", bg: "#40382f", modifiers: "Bold"),
    highlighted_item_style: (fg: "#dfbe86", modifiers: "Bold"),
    keybinds_selection_style: (fg: "#fff1db", bg: "#40382f", modifiers: "Bold"),
)
```

`current_item_style` is the selection cursor. `highlighted_item_style` is used
for highlights such as the playing song. The keybind section headings are bold
text and cannot be selected; they do not need a competing background band.

## Layout and spacing

A `Split` arranges child panes horizontally or vertically. A child is a `Pane`,
another `Split`, or a named `Component` defined in the theme. The shipped
`Queue` tab refers to `Component("now_playing")`, which contains cover art and
`Component("song_details")`. The theme's outer `layout` contains the common
header, `TabContent` and footer.

Sizes are strings:

- `"1"` means one terminal column in a horizontal split, or one row in a vertical split.
- `"35%"` means a share of that split's available space.
- `"0.45r"` is relative to the other axis: in a vertical split it is 0.45 times
  the available width in columns. It is useful for cover-art height; it is not
  a percentage of the screen height or a pixel aspect ratio.
- `"auto"` fits a Property pane's rendered content. It is supported only in
  horizontal splits, including components that resolve to Property panes.

Borders consume cells. An empty pane with size `"1"` creates a deliberate
gutter. Avoid stacking that gutter with extra borders or another empty pane.
If a field should keep a fixed width while its value changes, use a numeric size
instead of `"auto"`.

This theme example defines a one-row header and footer around the active tab:

```ron
// theme.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    layout: Split(direction: Vertical, panes: [
        (size: "1", pane: Split(direction: Horizontal, panes: [
            (size: "11", pane: Pane(Property(content: [
                (kind: Property(Status(State))),
            ], align: Center))),
            (size: "100%", pane: Pane(Tabs)),
            (size: "9", pane: Pane(Property(content: [
                (kind: Text("VOL ")),
                (kind: Property(Status(Volume))),
                (kind: Text("%")),
            ], align: Right))),
        ])),
        (size: "100%", pane: Pane(TabContent)),
        (size: "1", pane: Pane(ProgressBar)),
    ]),
)
```

For tabs, `fill: true` spreads their cells across the strip. `padding` is added
on each side of the label; `min_width` is a minimum label width before padding.
Labels keep their positions when selection changes. If they cannot all fit,
the strip scrolls when the selection leaves the visible range.

```ron
// theme.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    tab_bar: (
        fill: true,
        padding: 2,
        min_width: 0,
        alignment: Center,
        active_style: (fg: "#ffffff", bg: "#505050", modifiers: "Bold"),
        inactive_style: (fg: "#a0a0a0", bg: "#191919"),
    ),
    scrollbar: None,
    song_table_column_spacing: 2,
)
```

Queue header and rows share `song_table_format`; do not give their columns
separate spacing fixes. Use `width: "24%"` for proportional columns and a
numeric width for a short duration column. The sum must leave room for the
configured column spacing and any borders.

## Song details, covers and controls

Properties combine literal text with song or player data. Explicit `\n` creates
a new line. A wrapping Property pane centers each wrapped line independently;
`vertical_align` positions the complete text block inside the pane.

```ron
// theme.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    components: {
        "song_details": Pane(Property(
            content: [
                (kind: Property(Song(Title)), style: (modifiers: "Bold")),
                (kind: Text("\n\n")),
                (kind: Property(Song(Artist))),
                (kind: Text("\n\n")),
                (kind: Property(Song(Album))),
            ],
            align: Center,
            wrap: true,
            vertical_align: Center,
        )),
    },
)
```

Keep cover art and song details in the same horizontal region if they should
share a center line. `album_art.max_size_px` limits the actual image size; the
layout's `AlbumArt` pane determines its available cell area. `Auto` chooses an
image protocol supported by the terminal. Use Kitty's image protocol rather
than the `Block` fallback when an actual image is wanted.

Measure cover margins in pixels when matching the top to the sides. Terminal
rows are taller than columns are wide. The default uses one empty row above
the cover and three empty columns on each side; with the border, those margins
are approximately equal in a typical terminal font. Check the result with the
user's font and cell dimensions.

```ron
// config.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    theme: Some("warm"),
    volume_step: 5,
    album_art: (
        method: Auto,
        horizontal_align: Center,
        vertical_align: Top,
        max_size_px: (width: 1200, height: 1200),
    ),
)
```

For plain volume text, use `Property(Status(Volume))`, with your own `VOL` label
and `%` suffix as in the header example. `Property(Widget(Volume))` includes a
meter and label; it is a different presentation. A standalone `Volume` pane can
also render a slider configured through `volume_slider`.

The five progress symbols are start, elapsed section, thumb, remaining section
and end. The older three-symbol form is also accepted. Scrollbar symbols, when
enabled, need exactly four entries: track, thumb, up and down.

```ron
// theme.ron
#![enable(implicit_some)]
#![enable(unwrap_newtypes)]
#![enable(unwrap_variant_newtypes)]
(
    progress_bar: (
        symbols: ["━", "━", "╸", "─", "━"],
        elapsed_style: (fg: "#c0b8ad"),
        thumb_style: (fg: "#e7c48c"),
        track_style: (fg: "#49433b"),
        use_track_when_empty: true,
    ),
)
```

## Keys and optional panes

`keybinds.global` contains application actions. `keybinds.navigation` contains
common browser actions. Keys can be sequences (`"oi"`), modifiers (`"<C-j>"`) or
named keys (`"<Enter>"`). Uppercase and lowercase are different: the defaults use
`oi` for selection information and `oI` for the current song.

When changing a key map, keep the existing entries and edit only the requested
bindings. A supplied map replaces that section rather than extending its defaults.
The full action names and key syntax are in `crates/rspc-ui/src/config/keys/`.

Lyrics remain supported. To enable a lyrics view, add a `Lyrics` pane to the tab
layout, configure `lyrics_dir` and, if wanted, enable `enable_lyrics_index` and
`enable_lyrics_hot_reload`. Do not delete the lyrics implementation to hide it.

## Applying and checking a change

Config and theme hot reload is enabled by default through
`enable_config_hot_reload`. Saving the active files normally updates the UI.
Reopen the UI if hot reload is disabled. Appearance changes do not require
stopping the playback daemon.

For an agent working in the source tree:

1. Identify the active config and theme, and save a backup outside the repo.
2. Make the smallest change that expresses the request. Preserve unrelated keys,
   pane order, backend settings and the user's own styles.
3. Parse changed examples and run the relevant config/layout tests. Build
   `rspc-ui` only when Rust changed; editing a theme does not need compilation.
4. Inspect actual terminal screenshots at a wide and narrow size. Check long
   metadata, empty lists, selection, popups and image cleanup as well as the
   initial Queue view. Use `rspc --demo` for screenshots intended for publication.
5. Say which files changed and whether the user needs to reopen the UI. Never
   restart active playback merely to apply a theme.

The guide's RON examples are checked by frontend tests. For an option not covered
here, inspect the parser rather than inventing a setting:

| Definition | Source |
| --- | --- |
| Main settings | `crates/rspc-ui/src/config/mod.rs` |
| Theme fields | `crates/rspc-ui/src/config/theme/mod.rs` |
| Panes, splits and components | `crates/rspc-ui/src/config/tabs.rs` |
| Text properties and transforms | `crates/rspc-ui/src/config/theme/properties.rs` |
| Column sizing | `crates/rspc-ui/src/config/theme/queue_table.rs` |
| Cover settings | `crates/rspc-ui/src/config/album_art.rs` |
| Keys and actions | `crates/rspc-ui/src/config/keys/` |

Only change Rust when the requested behavior cannot be expressed by these
settings. Pane renderers live in `crates/rspc-ui/src/ui/panes`, shared widgets in
`ui/widgets`, and popup behavior in `ui/modals`. Keep the RON schema compatible
with existing configurations, and test both terminal cells and real images.
