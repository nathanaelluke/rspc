# Working on rspc

For appearance or keybinding requests, read [CUSTOMIZING.md](CUSTOMIZING.md)
first. Prefer configuration changes when the schema already supports the request.

## Code map

- `src/daemon`: queue ownership, persistence, catalog caching and the MPD-compatible socket.
- `src/playback.rs`: events shared by the Spotify and MPD players.
- `src/soloist.rs`: Spotify's player and its persistent local WebSocket transport.
- `src/spotify`: catalog, metadata and login protocols.
- `src/daemon/local.rs`: MPD library access and the dedicated playback partition.
- `crates/rspc-ui`: the terminal frontend, derived from rmpc.
- `config`: the default theme and tab layout shipped by the launcher.

[ARCHITECTURE.md](ARCHITECTURE.md) describes request ownership, cache invalidation
and rendering. Keep source attribution in [UPSTREAMS.md](UPSTREAMS.md) and the
license files when moving or rewriting inherited code.

## Behavior to preserve

Keep rmpc's configuration schema, layout model and keybinding meanings compatible.
Put Spotify-only options under `spotify`. Keep optional lyrics support; it is
hidden in the supplied layout, not removed from the application.

Closing the UI must leave the daemon and playback running. The daemon owns a
shared queue of Spotify and `mpd:` entries. Release one player before starting
the other, reject stale events by backend and request generation, and preserve
the user's default MPD queue and output ownership. `--local` connects directly
to MPD without the shared daemon.

Image workers prepare data; terminal painting belongs to the synchronized frame.
Retain prepared covers across tab changes. Keep expensive library work off the
UI thread, coalesce concurrent requests, and respect Spotify's cooldowns.

## Build and verify

Use `nix develop` for Rust and native build inputs. Keep the default two build
jobs and disabled LTO; the release binaries are the normal installed executables.

```sh
cargo fmt --all -- --check
cargo test --locked -j2 --workspace
cargo clippy --locked -j2 --workspace --all-targets -- -D warnings
cargo build --release --locked -j2 -p rspc -p rspc-ui
python3 tests/test_daemon.py
python3 tests/test_mixed_sources.py
python3 tests/test_pair_on_lan.py
python3 tests/test_release_check.py
python3 scripts/check-release.py
```

The mixed-source tests need MPD and use a null output. Keep tests isolated from
real login, audio devices and runtime sockets. Distinguish simulated playback
checks from live audio verification. Do not edit a person's playlists to test
mutations; use the fixtures.

Do not commit credentials, Soloist keys or binaries, caches, queue history,
private configs, logs or screenshots of a real account. Use `--demo` for public
screenshots. Never print a secret while investigating one. A change to docs or
configuration does not require restarting the playback daemon.
