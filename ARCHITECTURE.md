# Architecture

The workspace builds two executables. `rspc` launches the interface and runs a
detached playback daemon. `rspc-ui` renders the terminal interface and speaks
MPD over a private Unix socket. Closing the frontend leaves the daemon running;
`rspc stop` shuts it down.

## Playback

`src/daemon/engine.rs` owns the queue, playback modes and source transitions.
Spotify URIs go to Soloist through `src/soloist.rs`; `mpd:` entries go to a
separate MPD partition managed by `src/daemon/local.rs`. Local files and audio
URLs are loaded one at a time. The default MPD queue stays intact, and a borrowed
output is released when local playback stops.

Both players produce the events in `src/playback.rs`. Backend epochs reject
updates from replaced connections, and request IDs reject events from an earlier
track. Source handoffs stop the previous player before starting the next.
Controls are serialized without holding the player-state lock across network I/O.
State changes commit after successful control responses.

Soloist acknowledgements name a command but have no request ID. A timed-out
command therefore forces a transport reconnect before another command is sent.
Connect device changes release old playback ownership; a later snapshot can
restore the current track without another play command.

The daemon owns its connection and maintenance tasks. Slow source transitions
run outside the accept loop. Queue persistence retries failed writes and uses
atomic replacement.

## Library access

`src/spotify` adapts Spotify's catalog protocols. `src/daemon/catalog` adds
persistent page caches, request coalescing, batching and prepared playlist views.
Interactive cache misses have priority over background work. Stale foreground
reads return cached data while a refresh runs separately. Rate-limit responses
pause new requests while cached browsing remains available.

A background plan alternates playlist pages, saved-album tracks and the first
release page of a small set of followed artists. A pass has a budget of 128
catalog requests; disk hits do not count. Unfinished work resumes after 30 seconds,
and a completed plan is revisited after 15 minutes. Cache generations and
serialized disk writes prevent invalidated pages from reappearing.

Artists uses a paged release index instead of resolving an entire discography's
tracks. Only the selected album needs track metadata. Explicit whole-artist
queue actions still resolve every track. Albums and Album Artists use saved
albums; liked songs do not populate those roots. Local MPD entries are merged
where applicable. MPD database and playlist changes are forwarded through idle
notifications.

The daemon advertises `rspc_identity`, `rspc_artist_albums` and
`rspc_selection_info` as optional protocol extensions. The frontend checks for
these before using them. Folder summaries return counts rather than full song
payloads. Virtual library paths encode source names separately from display
labels, including names containing slashes. `listallinfo` resolves virtual trees
for bulk actions.

## Frontend

`crates/rspc-ui` retains rmpc's RON configuration model, panes and key handling.
`--local` connects it directly to MPD and bypasses the shared daemon. Supported
settings keep their meanings; Spotify-only options live under `spotify`.

Browser lists format visible rows while retaining full-list selection, filter
and mouse offsets. Loaded empty directories are distinct from unfinished
previews. Song previews and wrapped information rows are reused until their inputs
change. Asynchronous popup results are scoped to the modal that requested them.

Image workers prepare data and never paint directly. Image removal, terminal
cells and image placement are presented in one synchronized frame. Prepared
covers and Kitty's decoded image are retained across tab changes. Resizing
invalidates encoded pixels, while terminal reattachment forces a fresh upload.
Kitty uses direct placements normally and Unicode placeholders inside tmux.

## Files and configuration

The launcher installs the files in `config/` only when they are absent. User
configuration is not overwritten during builds or updates. The daemon stores
private state under the XDG state and cache directories. Frontend logs and IPC
sockets also use private directories; the UI and backend keep separate socket
namespaces.

See [CUSTOMIZING.md](CUSTOMIZING.md) for configuration examples and
[TESTING.md](TESTING.md) for regression coverage. Protocol references:
[MPD](https://mpd.readthedocs.io/en/stable/protocol.html) and
[Soloist](https://developer.spotify.com/documentation/soloist/reference/websocket-api).
