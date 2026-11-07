# Tests

Build both release binaries first, then run:

```sh
cargo test --locked -j2 -p rspc --lib
cargo test --locked -j2 -p rspc-ui
cargo clippy --locked -j2 --workspace --all-targets -- -D warnings
python3 tests/test_daemon.py
python3 tests/test_mixed_sources.py
python3 tests/test_pair_on_lan.py
python3 tests/test_release_check.py
python3 scripts/check-release.py --history
```

The daemon tests use isolated XDG directories and the actual rspc frontend,
including song-info catalog navigation, browser filtering and repeated seeks
while player updates arrive. Protocol tests cover late and fragmented idle
cancellation.
Backend tests also cover rejected controls, late Soloist acknowledgements,
cache invalidation during pending writes, and persistence retries. Background
warming tests cover foreground priority, request budgets, offline disk reuse,
cooldown recovery and continuing past a broken collection. Configuration
tests load the original rmpc example, the rspc defaults and the RON examples in
CUSTOMIZING.md through the frontend parser.
Mixed-source tests require `mpd`; they use temporary WAV files, a null output,
and a simulated Soloist. They cover automatic handoffs, controls, queue edits,
library changes, audio URLs and UI lifetime without accessing a real account
or system audio. Pairing tests mock the firewall commands.
Connect reactivation tests verify that current-song state and idle notifications
recover without another playback command. Cache tests cover offline reuse,
edit invalidation and memory bounds; keybind tests cover skipped section headings,
mouse clicks, wrapped navigation and empty sections. Selection-info tests cover
all browsers, Queue, both Search phases, marked songs, unloaded/empty folders,
virtual containers and curator profiles. They also check that late results cannot
replace a newer popup and cached previews render the same cells. Empty browser
folders must not refetch repeatedly. Layout checks cover stable tab positions,
loading-popup geometry and centered text with odd, even and wide glyph widths.

`python3 tests/benchmark_catalog.py` measures warm catalog responses against a
running daemon. Add `--artist "Artist name"` to measure the first release-index
page. It prints timings and counts, without library contents. Artist regression
tests withhold all track metadata and later release pages to catch eager loading;
UI tests verify paging preserves selections, marks and complete enqueue actions.

Live audio and account-specific Spotify behavior need separate verification.
Keep credentials, Soloist binaries and recordings out of the repository.

Privacy regressions check configuration debug output, private runtime directories
and JSON escaping in image commands. The release checker inspects publishable
files and, when requested, reachable Git history; it never prints matched secrets.
