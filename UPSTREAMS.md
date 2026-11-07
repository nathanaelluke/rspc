# Source provenance

## Frontend

`crates/rspc-ui` began with [rmpc 0.11.0](https://github.com/mierak/rmpc/tree/v0.11.0),
licensed under BSD-3-Clause. It is maintained as rspc's frontend, with additional
catalog navigation, caching, layout options and MPD-compatible extensions. Its
configuration parser and original example configurations remain for compatibility.
The original license is in `crates/rspc-ui/LICENSE` and `LICENSE-BSD-3-Clause`.

## Spotify protocol code

The helpers in `src/spotify` and metadata types in `src/model.rs` derive from
[Spotatui 0.46.0](https://github.com/LargeModGames/spotatui/tree/v0.46.0), licensed
under MIT. They retain Spotatui's spotify-tui ancestry. rspc uses the protocol
helpers rather than the Spotatui application or event loop. The copyright notice
is retained in `LICENSE-MIT`.

The `spotatui-librespot-*` 0.8.6 crates provide metadata and authentication
protocols. Playback goes through Spotify's external Soloist player; rspc does
not link a librespot playback engine.

## External player

Soloist is downloaded from Spotify by `scripts/setup-soloist.sh` and installed
in the user's private state directory. The repository does not distribute the
player, its API keys or pairing state. Spotify supplies its terms and notices.

See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for copyright notices. rspc is
not endorsed by Spotify, rmpc or Spotatui.
