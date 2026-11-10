# rspc

I like rmpc's interface, but most of my music is on Spotify. rspc keeps that
interface and adds Spotify alongside local files and streams, all in one queue.
Close the terminal and the music keeps playing.

![rspc showing the demo queue](assets/screenshot.png)

## Install

The package definitions are ready. AUR and Homebrew publication is pending.
Once published, these installs also provide Spotify's Soloist player:

| Package manager | Command |
| --- | --- |
| AUR | `yay -S rspc-git` |
| Homebrew | `brew install --cask nathanaelluke/tap/rspc` |
| Nix, from a checkout | `nix profile add .#rspc --max-jobs 1 --cores 1` |

Targets are Linux x86-64, Linux AArch64 and Apple Silicon macOS 13 or newer.
Native Windows and Spotify playback on Intel Macs are not supported. Linux
release binaries require OpenSSL 3 and glibc 2.35 on x86-64 or 2.39 on AArch64.
Soloist also needs a working audio service. On Linux, use PipeWire or PulseAudio.

To build from source, install current stable Rust, a C compiler, `pkg-config`
and OpenSSL development headers. Native dependencies:

| Platform | Packages |
| --- | --- |
| Arch | `base-devel rust pkgconf openssl git` |
| Debian / Ubuntu | `build-essential pkg-config libssl-dev git`, plus stable Rust |
| Fedora | `gcc rust cargo pkgconf-pkg-config openssl-devel git` |
| macOS | Xcode command-line tools and `brew install rust pkgconf openssl@3` |

```sh
git clone https://github.com/nathanaelluke/rspc.git
cd rspc
# Alternatively, enter `nix develop` for the build dependencies.
cargo build --release --locked -j1 -p rspc -p rspc-ui
./scripts/install.sh
```

The installer copies both executables into `~/.local/bin`, which needs to be on PATH.
It keeps your config intact and supports `--prefix` and `DESTDIR` for system
packages. Run one build at a time on machines with limited RAM. A Nix package
build uses a separate cache and can still need substantial memory.

Source installs also need Soloist: install `spotify-soloist-bin` from AUR,
`brew install --cask spotify-soloist`, or `nix profile add .#soloist`.
On other systems, put the executable from
[Spotify's downloads](https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates)
on PATH. On NixOS, use the Nix package for its loader and audio libraries.
Set `RSPC_SOLOIST_PATH` to use a different executable.
rspc starts its own player, so do not enable a separate Soloist service for it.

## Spotify setup

You need Spotify Premium and your own
[Soloist API key](https://developer.spotify.com/dashboard/soloist).
Save the key in `~/.config/rspc/soloist-api-key`, restrict it with `chmod 600`,
then run:

```sh
rspc login
rspc
```

Complete the authorization shown in the terminal. That single sign-in covers
library access and playback. You don't need to select a device from your phone.
Use `rspc login --repair` to repair an expired or revoked audio session. It
restarts the player and interrupts playback.

Soloist builds expire 90 days after their build date. Keep its package current:
`yay -Syu` on Arch, or `brew upgrade --cask --greedy spotify-soloist` on Homebrew.
Nix users need an updated rspc flake input. Stop rspc before updating its player.
The player is downloaded separately from Spotify, not bundled in rspc's releases.

## Using rspc

```sh
rspc                          # open the interface
rspc -- add /path/to/song.flac
rspc -- add https://example.org/stream
rspc --local                  # use your existing MPD queue directly
rspc doctor                   # inspect player state and errors
rspc stop                     # stop the daemon and playback
```

Local files and streams need MPD 0.24 or newer with a configured library.
rspc uses a separate partition and leaves your default MPD queue alone. It reads
MPD settings from `~/.config/rmpc/config.ron`, or the file passed with `--config`.
`RSPC_MPD_ADDRESS`, `RSPC_MPD_PASSWORD` and `RSPC_MPD_OUTPUT` override them.
MPD is optional for Spotify-only use.

Albums shows saved albums. Artists opens followed artists' full discographies.
Album Artists stays within saved albums. Directories includes liked songs,
local files and followed users' playlists. Spotify and local tracks can share a
queue, with a short gap when switching players. Spotify playlists cannot store
local files. Local playlists appear under **Directories → Local Playlists**.

## Configuration

Configuration lives in `~/.config/rspc/config.ron`, with themes in `themes/`.
Themes, layouts and keybindings use rmpc 0.11's RON configuration format. See
[rmpc's documentation](https://rmpc.mierak.dev/configuration/) to customize them.
The shipped defaults are in `config/`.
Pass an existing config with `--config`. MPD-specific features such as stickers
and ReplayGain need `--local`. Spotify-specific options live under `spotify`.

To hide playlists or folders, set `spotify.exclude_playlists` and
`spotify.exclude_folders`. Rules accept names and Spotify URIs. Folder rules
also accept paths and include descendants. Names are case insensitive. The
default hides folders named `archive`. Changes reload without restarting music.

Catalog caches live in `~/.cache/rspc`. Queue state, pairing and logs live in
`~/.local/state/rspc`. XDG overrides are supported. Keep these files and your
Soloist key out of commits and bug reports. Logs can contain paths and library
metadata. Inspect them before sharing and use `rspc --demo` for public screenshots.

## Development

`nix develop` provides the build dependencies and MPD for tests. Run `make check`
for the Rust unit and integration tests, formatting, and Clippy. Tests use
isolated directories and players. Live Spotify checks require an explicit
`--ignored` invocation and a saved login.

Release tools live in `crates/xtask`. To check the working tree, index and Git
history for private files and credentials:

```sh
cargo run --locked -p xtask -- check-release --history
```

Spotify replaces its Soloist download archives in place. When they change,
refresh Nix's pinned checksums with
`cargo run --locked -p xtask -- update-soloist --write`.

Version tags create draft releases with binaries, checksums and package recipes.
Native builds run in CI for each release target. The privacy check catches common
mistakes. Review files and history yourself before publishing.

The frontend is derived from [rmpc](https://github.com/mierak/rmpc). Protocol
libraries come from [librespot](https://github.com/librespot-org/librespot).
Audio playback uses Soloist or MPD. See [NOTICE](NOTICE) and the license files
for attribution. rspc is not affiliated with Spotify or its upstream projects.
