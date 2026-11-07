# rspc

rspc puts Spotify tracks, local music and streams in one terminal queue. A
background daemon owns playback, so closing the interface leaves the music
running.

The interface has configurable panes, themes and keybindings. Albums shows saved
albums, Artists opens the full discographies of followed artists, and Album
Artists stays within saved albums. Directories also includes liked songs, local
files and playlists made by people you follow.

## Build

The current setup targets Linux on x86-64. With Nix installed:

```sh
nix develop
cargo build --release --locked -j2 -p rspc -p rspc-ui
scripts/install.sh
```

The installer links both executables into `~/.local/bin`. Add that directory to
your shell's `PATH` if it is not there already. Without Nix, building needs Rust,
`pkg-config` and the OpenSSL development libraries; the Soloist setup script
below still uses Nix.

## Spotify

You need Spotify Premium and your own [Soloist API key](https://developer.spotify.com/dashboard/soloist).
The key and player are installed locally; neither is included in this repo.

1. Save the key in `~/.config/rspc/soloist-api-key` and run
   `chmod 600 ~/.config/rspc/soloist-api-key`.
2. Run `scripts/setup-soloist.sh` to download Spotify's player.
3. Run `rspc login`, then `rspc pair`.
4. Select **rspc** once in Spotify's device menu to complete pairing.

After pairing, playing a track in rspc activates its output automatically. The
interface can be closed and reopened without repeating setup. Soloist builds
expire; rerun the setup script when an update is required.

If the device is missing on a NixOS machine, `scripts/pair-on-lan.py` can open the
discovery ports temporarily. Use `--dry-run` to inspect the rules first; a normal
run removes its rules when pairing finishes.

## Local music and streams

Local playback needs a running MPD with partition support and a configured music
library. rspc uses its own partition and returns the borrowed output when local
playback stops. The default MPD queue is left alone.

MPD connection settings come from `~/.config/rmpc/config.ron`, or the file passed
with `--config`. `RSPC_MPD_ADDRESS`, `RSPC_MPD_PASSWORD` and `RSPC_MPD_OUTPUT`
override them. MPD is optional for Spotify-only use.

```sh
rspc
rspc -- add /path/to/song.flac
rspc -- add https://example.org/stream
rspc --local                  # use your existing MPD queue directly
rspc doctor                   # inspect player state and errors
rspc stop                     # stop the daemon and playback
```

Use **Directories → Local Files** to browse MPD's library. Spotify and local
entries can be reordered together. Switching between the two players may leave
a short gap. Spotify playlists cannot store local files; local playlists appear
under **Directories → Local Playlists**.

## Make it yours

Configuration lives in `~/.config/rspc/config.ron`, with themes alongside it in
`themes/`. rspc keeps rmpc's RON configuration model. Existing configurations can
be passed with `--config`; MPD-specific features such as stickers, crossfade and
ReplayGain need `--local`.

[CUSTOMIZING.md](CUSTOMIZING.md) explains which file controls each part of the
interface, with examples and instructions for agents. For example:

> Read AGENTS.md and CUSTOMIZING.md. Make the header compact, use a warm gray
> palette, and keep my existing keys and pane order. Preview the result at wide
> and narrow terminal sizes.

To hide playlists or folders, set `spotify.exclude_playlists` and
`spotify.exclude_folders` in the main config. Rules accept names and Spotify
URIs; folder rules also accept full folder paths and include descendants. Names
are case insensitive, IDs are not. The default hides folders named `archive`.
Changes reload without restarting playback.

Library pages are cached under `~/.cache/rspc` and warmed in the background.
The first sync needs a network connection. Queue state and player logs live under
`~/.local/state/rspc`. XDG directory overrides are supported. Keep those private
files out of bug reports and commits; see [SECURITY.md](SECURITY.md).

## Development and credits

See [CONTRIBUTING.md](CONTRIBUTING.md) for the workspace layout and checks, and
[TESTING.md](TESTING.md) for test coverage.

The frontend is derived from [rmpc](https://github.com/mierak/rmpc). Some Spotify
protocol helpers came from [Spotatui](https://github.com/LargeModGames/spotatui).
Their copyright notices are retained; [UPSTREAMS.md](UPSTREAMS.md) records the
sources and licenses. rspc is not affiliated with Spotify or either upstream.
