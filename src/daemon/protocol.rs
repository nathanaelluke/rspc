//! MPD protocol server for the shared queue.
use super::{
    engine::Engine,
    queue::{Entry, Queue},
};
use anyhow::{Context, Result, bail, ensure};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Debug)]
struct UnknownCommand(String);
impl std::fmt::Display for UnknownCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported command '{}' for the Spotify backend",
            self.0
        )
    }
}
impl std::error::Error for UnknownCommand {}

#[derive(Debug)]
struct MissingArtwork;
impl std::fmt::Display for MissingArtwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("track has no album artwork")
    }
}
impl std::error::Error for MissingArtwork {}

fn failure(command: &str, index: usize, error: &anyhow::Error) -> String {
    let code = if error.is::<UnknownCommand>() {
        5
    } else if error.is::<super::queue::MissingQueueId>() || error.is::<MissingArtwork>() {
        50
    } else {
        52
    };
    format!(
        "ACK [{code}@{index}] {{{command}}} {}\n",
        clean(&format!("{error:#}"))
    )
}

const COMMANDS: &[&str] = &[
    "rspc_identity",
    "rspc_artist_albums",
    "rspc_selection_info",
    "rspc_diagnostics",
    "rspc_pair",
    "rspc_login",
    "rspc_check_spotify",
    "ping",
    "password",
    "close",
    "commands",
    "notcommands",
    "binarylimit",
    "tagtypes",
    "status",
    "stats",
    "currentsong",
    "playlistinfo",
    "playlistid",
    "plchanges",
    "plchangesposid",
    "add",
    "addid",
    "clear",
    "delete",
    "deleteid",
    "move",
    "moveid",
    "swap",
    "swapid",
    "shuffle",
    "play",
    "playid",
    "pause",
    "stop",
    "next",
    "previous",
    "seek",
    "seekid",
    "seekcur",
    "setvol",
    "volume",
    "getvol",
    "repeat",
    "random",
    "single",
    "consume",
    "idle",
    "noidle",
    "list",
    "find",
    "search",
    "findadd",
    "searchadd",
    "lsinfo",
    "listall",
    "listallinfo",
    "listfiles",
    "listplaylists",
    "listplaylist",
    "listplaylistinfo",
    "load",
    "playlistadd",
    "playlistdelete",
    "playlistmove",
    "playlistclear",
    "save",
    "rename",
    "rm",
    "albumart",
    "readpicture",
    "outputs",
    "decoders",
    "getpartitions",
    "listpartitions",
    "partition",
    "clearerror",
    "update",
    "rescan",
    "config",
    "urlhandlers",
    "channels",
];

const MAX_COMMAND_BYTES: usize = 65_536;
const MAX_COMMAND_LIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMMAND_LIST_LENGTH: usize = 10_000;

struct CommandList {
    separators: bool,
    bytes: usize,
    commands: Vec<Vec<String>>,
}

impl CommandList {
    fn push(&mut self, args: Vec<String>, bytes: usize) -> Result<()> {
        ensure!(
            self.commands.len() < MAX_COMMAND_LIST_LENGTH
                && self.bytes + bytes <= MAX_COMMAND_LIST_BYTES,
            "command list too large"
        );
        self.bytes += bytes;
        self.commands.push(args);
        Ok(())
    }
}

async fn read_command(read: &mut (impl AsyncBufRead + Unpin), line: &mut Vec<u8>) -> Result<usize> {
    // Keep partial bytes in `line` when an idle event cancels the read. Limit
    // the read itself so a missing newline cannot grow the buffer indefinitely.
    let remaining = MAX_COMMAND_BYTES.saturating_sub(line.len()) + 1;
    let bytes = read.take(remaining as u64).read_until(b'\n', line).await?;
    ensure!(line.len() <= MAX_COMMAND_BYTES, "command too long");
    Ok(bytes)
}

pub fn words(line: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut token = String::new();
        let quote = if matches!(chars.peek(), Some('"') | Some('\'')) {
            chars.next()
        } else {
            None
        };
        let mut closed = quote.is_none();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                token.push(chars.next().context("truncated escape")?);
            } else if Some(ch) == quote {
                closed = true;
                break;
            } else if quote.is_none() && ch.is_whitespace() {
                break;
            } else {
                token.push(ch);
            }
        }
        ensure!(closed, "unterminated quoted argument");
        result.push(token);
    }
    Ok(result)
}
pub fn clean(value: &str) -> String {
    value.replace(['\n', '\r', '\0'], " ")
}
pub fn song(entry: &Entry, position: Option<usize>) -> String {
    let t = &entry.track;
    let mut s = format!(
        "file: {}\nTitle: {}\nAlbum: {}\nTrack: {}\nDate: {}\nTime: {}\nduration: {:.3}\n",
        clean(t.uri.as_deref().unwrap_or("")),
        clean(&t.name),
        clean(&t.album),
        t.track_number,
        clean(t.release_date.as_deref().unwrap_or("")),
        t.duration_ms / 1000,
        t.duration_ms as f64 / 1000.0
    );
    for artist in &t.artists {
        s.push_str(&format!("Artist: {}\n", clean(artist)));
    }
    for genre in &t.genres {
        s.push_str(&format!("Genre: {}\n", clean(genre)));
    }
    for artist in t.album_artists() {
        s.push_str(&format!("AlbumArtist: {}\n", clean(artist)));
    }
    if let Some(position) = position {
        s.push_str(&format!("Pos: {position}\nId: {}\n", entry.id));
    }
    s
}
fn arg(args: &[String], n: usize) -> Result<&str> {
    args.get(n)
        .map(String::as_str)
        .context("missing command argument")
}
fn range(text: &str, len: usize) -> Result<(usize, usize)> {
    if let Some((start, end)) = text.split_once(':') {
        Ok((
            start.parse()?,
            if end.is_empty() { len } else { end.parse()? },
        ))
    } else {
        let start: usize = text.parse()?;
        let end = start
            .checked_add(1)
            .context("queue position is too large")?;
        Ok((start, end))
    }
}

fn queue_index(queue: &Queue, text: &str, by_id: bool) -> Result<usize> {
    if by_id {
        queue.index(text.parse()?)
    } else {
        Ok(text.parse()?)
    }
}

fn queue_range(queue: &Queue, text: &str, by_id: bool) -> Result<(usize, usize)> {
    if by_id {
        let index = queue.index(text.parse()?)?;
        Ok((index, index + 1))
    } else {
        range(text, queue.entries.len())
    }
}
async fn list_songs(engine: &Engine, tracks: Vec<crate::model::TrackInfo>) -> Vec<u8> {
    let tracks = engine.catalog.remember(tracks).await;
    tracks
        .into_iter()
        .map(|track| song(&Entry { id: 0, track }, None))
        .collect::<String>()
        .into_bytes()
}

pub async fn execute(engine: &Engine, args: &[String]) -> Result<Vec<u8>> {
    let command = arg(args, 0)?;
    let text = match command {
        "ping" | "password" | "binarylimit" | "noidle" => String::new(),
        "rspc_selection_info" => {
            let info = engine
                .catalog
                .selection_info(arg(args, 1)?, arg(args, 2)?)
                .await?;
            format!("selection_info: {}\n", serde_json::to_string(&info)?)
        }
        "rspc_artist_albums" => {
            let page = engine
                .catalog
                .artist_album_page(arg(args, 1)?, arg(args, 2)?.parse()?)
                .await?;
            format!("album_page: {}\n", serde_json::to_string(&page)?)
        }
        "rspc_diagnostics" => engine.diagnostics().await,
        "rspc_pair" => engine.prepare_audio().await?,
        "rspc_login" => engine.login_spotify().await?,
        "rspc_check_spotify" => {
            ensure!(
                !engine.catalog.demo,
                "Spotify login is unavailable in demo mode"
            );
            crate::spotify::connection::check_pairing().await?;
            format!(
                "catalog_auth: {}\n",
                crate::spotify::connection::description()
            )
        }
        "rspc_identity" => format!(
            "name: rspc\nengine: rspc\nfrontend: rspc-ui\ndemo: {}\n",
            u8::from(engine.catalog.demo)
        ),
        "commands" => COMMANDS.iter().map(|c| format!("command: {c}\n")).collect(),
        "notcommands" | "channels" | "urlhandlers" => String::new(),
        "tagtypes" => {
            if args.len() == 1 {
                [
                    "Artist",
                    "AlbumArtist",
                    "Album",
                    "Title",
                    "Track",
                    "Date",
                    "Genre",
                ]
                .iter()
                .map(|t| format!("tagtype: {t}\n"))
                .collect()
            } else {
                String::new()
            }
        }
        "config" => "music_directory: /rspc\n".into(),
        "status" => {
            let p = engine.player.lock().await;
            let state = p.playback();
            let mode = if state.stopped {
                "stop"
            } else if state.playing {
                "play"
            } else {
                "pause"
            };
            let single = if p.single == 2 {
                "oneshot".into()
            } else {
                p.single.to_string()
            };
            let mut s = format!(
                "partition: default\nvolume: {}\nrepeat: {}\nrandom: {}\nsingle: {}\nconsume: {}\nplaylist: {}\nplaylistlength: {}\nstate: {mode}\nelapsed: {:.3}\nduration: {:.3}\n",
                state.volume,
                u8::from(p.repeat),
                u8::from(p.random),
                single,
                if p.queue.consume_oneshot {
                    "oneshot".to_owned()
                } else {
                    u8::from(p.queue.consume).to_string()
                },
                p.queue.version,
                p.queue.entries.len(),
                state.position_ms as f64 / 1000.0,
                state.entry.as_ref().map_or(0, |e| e.track.duration_ms) as f64 / 1000.0
            );
            if let Some(i) = p.queue.cursor() {
                s.push_str(&format!("song: {i}\nsongid: {}\n", p.queue.entries[i].id));
                if let Some(next) = p.queue.entries.get(i + 1) {
                    s.push_str(&format!("nextsong: {}\nnextsongid: {}\n", i + 1, next.id));
                }
            }
            if !state.error.is_empty() {
                s.push_str(&format!("error: {}\n", clean(&state.error)));
            }
            s
        }
        "stats" => {
            let p = engine.player.lock().await;
            format!(
                "artists: 0\nalbums: 0\nsongs: {}\nuptime: 0\ndb_playtime: {}\ndb_update: 0\nplaytime: 0\n",
                p.queue.entries.len(),
                p.queue
                    .entries
                    .iter()
                    .map(|e| e.track.duration_ms / 1000)
                    .sum::<u64>()
            )
        }
        "getvol" => format!("volume: {}\n", engine.playback().await.volume),
        "currentsong" => {
            let p = engine.player.lock().await;
            p.queue
                .cursor()
                .map(|i| song(&p.queue.entries[i], Some(i)))
                .unwrap_or_default()
        }
        "playlistinfo" | "playlistid" | "plchanges" | "plchangesposid" => {
            let p = engine.player.lock().await;
            let wanted = args.get(1).map(String::as_str);
            let selected = if command == "playlistinfo" {
                wanted
                    .map(|v| range(v, p.queue.entries.len()))
                    .transpose()?
            } else {
                None
            };
            let id = if command == "playlistid" {
                wanted.map(str::parse::<u32>).transpose()?
            } else {
                None
            };
            p.queue
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| {
                    selected.is_none_or(|(a, b)| *i >= a && *i < b)
                        && id.is_none_or(|id| e.id == id)
                })
                .map(|(i, e)| {
                    if command == "plchangesposid" {
                        format!("cpos: {i}\nId: {}\n", e.id)
                    } else {
                        song(e, Some(i))
                    }
                })
                .collect()
        }
        "add" | "addid" => {
            let tracks = engine.catalog.resolve(arg(args, 1)?).await?;
            ensure!(
                command != "addid" || !tracks.is_empty(),
                "no playable tracks"
            );
            let ids = engine
                .edit_queue(|queue| {
                    let position = args
                        .get(2)
                        .map(|v| queue.position(v))
                        .transpose()?
                        .unwrap_or(queue.entries.len());
                    queue.add(tracks, position)
                })
                .await?;
            if command == "addid" {
                format!("Id: {}\n", ids.first().context("no playable tracks")?)
            } else {
                String::new()
            }
        }
        "clear" => {
            engine.clear_queue().await;
            String::new()
        }
        "delete" | "deleteid" => {
            engine
                .edit_queue(|queue| {
                    let (a, b) = queue_range(queue, arg(args, 1)?, command == "deleteid")?;
                    queue.delete(a, b)
                })
                .await?;
            String::new()
        }
        "move" | "moveid" => {
            engine
                .edit_queue(|queue| {
                    let (a, b) = queue_range(queue, arg(args, 1)?, command == "moveid")?;
                    let dest = queue.position(arg(args, 2)?)?;
                    queue.move_range(a, b, dest)
                })
                .await?;
            String::new()
        }
        "shuffle" => {
            engine
                .edit_queue(|queue| {
                    let (a, b) = args
                        .get(1)
                        .map(|v| range(v, queue.entries.len()))
                        .transpose()?
                        .unwrap_or((0, queue.entries.len()));
                    ensure!(a <= b && b <= queue.entries.len(), "invalid shuffle range");
                    use rand::seq::SliceRandom;
                    queue.entries[a..b].shuffle(&mut rand::rng());
                    queue.changed();
                    Ok(())
                })
                .await?;
            String::new()
        }
        "swap" | "swapid" => {
            engine
                .edit_queue(|queue| {
                    let a = queue_index(queue, arg(args, 1)?, command == "swapid")?;
                    let b = queue_index(queue, arg(args, 2)?, command == "swapid")?;
                    ensure!(
                        a < queue.entries.len() && b < queue.entries.len(),
                        "queue index out of range"
                    );
                    queue.entries.swap(a, b);
                    queue.changed();
                    Ok(())
                })
                .await?;
            String::new()
        }
        "play" | "playid" => {
            if command == "play" && args.len() == 1 {
                engine.pause(Some(false)).await?;
                return Ok(Vec::new());
            }
            if command == "playid" {
                engine.play_id(arg(args, 1)?.parse()?).await?;
            } else {
                engine.play(arg(args, 1)?.parse()?).await?;
            }
            String::new()
        }
        "pause" => {
            engine.pause(args.get(1).map(|v| v != "0")).await?;
            String::new()
        }
        "stop" => {
            engine.stop().await;
            String::new()
        }
        "next" | "previous" => {
            engine.next(command == "previous").await?;
            String::new()
        }
        "seekcur" => {
            let value = arg(args, 1)?;
            engine
                .seek(
                    value.parse()?,
                    value.starts_with('+') || value.starts_with('-'),
                )
                .await?;
            String::new()
        }
        "seek" | "seekid" => {
            let seconds = arg(args, 2)?.parse()?;
            if command == "seekid" {
                engine.seek_id(arg(args, 1)?.parse()?, seconds).await?;
            } else {
                engine
                    .seek_position(arg(args, 1)?.parse()?, seconds)
                    .await?;
            }
            String::new()
        }
        "setvol" | "volume" => {
            engine
                .volume(arg(args, 1)?.parse()?, command == "volume")
                .await;
            String::new()
        }
        "repeat" | "random" | "single" | "consume" => {
            let mode = match arg(args, 1)? {
                "0" => 0,
                "1" => 1,
                "oneshot" if matches!(command, "single" | "consume") => 2,
                _ => bail!("invalid playback mode"),
            };
            {
                let mut p = engine.player.lock().await;
                match command {
                    "repeat" => p.repeat = mode != 0,
                    "random" => p.random = mode != 0,
                    "single" => p.single = mode,
                    "consume" => {
                        p.queue.consume = mode != 0;
                        p.queue.consume_oneshot = mode == 2;
                    }
                    _ => unreachable!(),
                }
            }
            engine.sync_pending().await;
            engine.mark_dirty();
            engine.changed(vec!["options"]);
            String::new()
        }
        "list" => {
            let tag = arg(args, 1)?;
            engine
                .catalog
                .tags(tag, args.get(2).map(String::as_str).unwrap_or(""))
                .await?
                .iter()
                .map(|v| format!("{}: {}\n", clean(tag), clean(v)))
                .collect()
        }
        "find" | "search" | "findadd" | "searchadd" => {
            let tracks = engine
                .catalog
                .find(arg(args, 1)?, command.starts_with("search"))
                .await?;
            if command.ends_with("add") {
                engine
                    .edit_queue(|queue| {
                        let position = if args.get(2).is_some_and(|s| s == "position") {
                            queue.position(arg(args, 3)?)?
                        } else {
                            queue.entries.len()
                        };
                        queue.add(tracks, position)
                    })
                    .await?;
                String::new()
            } else {
                return Ok(list_songs(engine, tracks).await);
            }
        }
        "listplaylists" => {
            let (spotify, local) = tokio::join!(engine.catalog.playlist_names(), async {
                if engine.catalog.demo {
                    Ok(Vec::new())
                } else {
                    engine.catalog.local.playlists().await
                }
            });
            let local_available = local.is_ok();
            let mut names = local.unwrap_or_default();
            match spotify {
                Ok(p) => names.extend(p.into_keys()),
                Err(e) if !local_available => return Err(e),
                Err(_) => {}
            }
            names.sort();
            names
                .iter()
                .map(|name| format!("playlist: {}\n", clean(name)))
                .collect()
        }
        "playlistadd" | "playlistdelete" | "playlistmove" | "playlistclear" | "save" | "rename"
        | "rm" => {
            let name = arg(args, 1)?;
            match command {
                "playlistadd" => {
                    engine
                        .catalog
                        .playlist_add(
                            name,
                            arg(args, 2)?,
                            args.get(3).map(|s| s.parse()).transpose()?,
                        )
                        .await?
                }
                "playlistdelete" => {
                    let len = engine.catalog.playlist_tracks(name).await?.len();
                    let (start, end) = range(arg(args, 2)?, len)?;
                    engine.catalog.playlist_remove(name, start, end).await?;
                }
                "playlistmove" => {
                    let len = engine.catalog.playlist_tracks(name).await?.len();
                    let (start, end) = range(arg(args, 2)?, len)?;
                    engine
                        .catalog
                        .playlist_move(name, start, end, arg(args, 3)?.parse()?)
                        .await?
                }
                "playlistclear" => engine.catalog.playlist_clear(name).await?,
                "rename" => engine.catalog.playlist_rename(name, arg(args, 2)?).await?,
                "rm" => engine.catalog.playlist_delete(name).await?,
                "save" => {
                    let tracks = engine
                        .player
                        .lock()
                        .await
                        .queue
                        .entries
                        .iter()
                        .map(|e| e.track.clone())
                        .collect();
                    engine.catalog.playlist_save(name, tracks).await?;
                }
                _ => unreachable!(),
            }
            engine.changed(vec!["stored_playlist"]);
            String::new()
        }
        "listplaylist" | "listplaylistinfo" | "load" => {
            let mut tracks = engine.catalog.playlist_tracks(arg(args, 1)?).await?;
            if command == "listplaylistinfo"
                && let Some(r) = args.get(2)
            {
                let (a, b) = range(r, tracks.len())?;
                ensure!(a <= b && b <= tracks.len(), "invalid playlist range");
                tracks = tracks[a..b].to_vec();
            }
            if command == "load" {
                engine
                    .edit_queue(|queue| {
                        let n = queue.entries.len();
                        queue.add(tracks, n)
                    })
                    .await?;
                String::new()
            } else if command == "listplaylist" {
                tracks
                    .iter()
                    .map(|t| format!("file: {}\n", clean(t.uri.as_deref().unwrap_or(""))))
                    .collect()
            } else {
                return Ok(list_songs(engine, tracks).await);
            }
        }
        "lsinfo" | "listfiles" | "listall" | "listallinfo" => {
            let path = args.get(1).map(String::as_str).unwrap_or("");
            if command == "listallinfo" {
                return Ok(list_songs(engine, engine.catalog.selection_tracks(path).await?).await);
            }
            if path == "People" || path.starts_with("People/") {
                let (directories, tracks) = engine.catalog.browse_people(path).await?;
                let mut reply = directories
                    .iter()
                    .map(|path| format!("directory: {}\n", clean(path)))
                    .collect::<String>()
                    .into_bytes();
                reply.extend(list_songs(engine, tracks).await);
                return Ok(reply);
            }
            if path.starts_with("rspc://artist-album/") || path.starts_with("rspc://library/") {
                return Ok(list_songs(engine, engine.catalog.resolve(path).await?).await);
            }
            if path.starts_with("rspc://catalog/") {
                let (directories, tracks) = engine.catalog.browse_catalog(path).await?;
                let mut reply = directories
                    .iter()
                    .map(|path| format!("directory: {}\n", clean(path)))
                    .collect::<String>()
                    .into_bytes();
                reply.extend(list_songs(engine, tracks).await);
                return Ok(reply);
            }
            if path.is_empty() || path == "/" {
                [
                    "Liked Songs",
                    "Recently Played",
                    "Playlists",
                    "Albums",
                    "Artists",
                    "People",
                    "Local Files",
                    "Local Playlists",
                ]
                .iter()
                .map(|d| format!("directory: {d}\n"))
                .collect()
            } else if path == "Local Files" || path.starts_with("Local Files/") {
                let local_path = path.strip_prefix("Local Files/").unwrap_or("");
                let browse = engine
                    .catalog
                    .local
                    .browse(local_path, command == "listall" || command == "listallinfo")
                    .await?;
                let mut reply: Vec<u8> = browse
                    .directories
                    .iter()
                    .map(|d| format!("directory: {}\n", clean(d)))
                    .collect::<String>()
                    .into_bytes();
                reply.extend(list_songs(engine, browse.tracks).await);
                return Ok(reply);
            } else if path == "Local Playlists" {
                engine
                    .catalog
                    .local
                    .playlists()
                    .await?
                    .iter()
                    .map(|name| format!("playlist: {}\n", clean(name)))
                    .collect()
            } else if path == "Playlists" {
                engine
                    .catalog
                    .playlist_names()
                    .await?
                    .keys()
                    .map(|name| format!("directory: Playlists/{}\n", clean(name)))
                    .collect()
            } else if path == "Albums" || path == "Artists" {
                let (tag, kind) = if path == "Albums" {
                    ("Album", "album")
                } else {
                    ("Artist", "artist")
                };
                super::catalog::Catalog::library_directories(
                    kind,
                    engine.catalog.tags(tag, "").await?,
                )
                .into_iter()
                .map(|path| format!("directory: {path}\n"))
                .collect()
            } else {
                return Ok(list_songs(engine, engine.catalog.resolve(path).await?).await);
            }
        }
        "albumart" | "readpicture" => {
            if let Some(file) = arg(args, 1)?.strip_prefix("mpd:") {
                let offset: usize = arg(args, 2)?.parse()?;
                return engine
                    .catalog
                    .local
                    .request(&format!(
                        "{command} {} {offset}",
                        super::local::quote(file)?
                    ))
                    .await;
            }
            let tracks = engine.catalog.resolve(arg(args, 1)?).await?;
            let track = tracks.first().context("track not found")?;
            let uri = track.image_url.as_deref().ok_or(MissingArtwork)?;
            let data = engine.catalog.artwork(uri).await?;
            let offset: usize = arg(args, 2)?.parse()?;
            ensure!(offset <= data.len(), "artwork offset out of range");
            let end = (offset + 65536).min(data.len());
            let mut reply = format!(
                "size: {}\ntype: image/jpeg\nbinary: {}\n",
                data.len(),
                end - offset
            )
            .into_bytes();
            reply.extend_from_slice(&data[offset..end]);
            reply.push(b'\n');
            return Ok(reply);
        }
        "outputs" => {
            "outputid: 0\noutputname: rspc shared queue\noutputenabled: 1\nplugin: Soloist+MPD\n"
                .into()
        }
        "decoders" => {
            let mut reply = b"plugin: Soloist\nsuffix: ogg\nsuffix: flac\nsuffix: aac\nmime_type: audio/ogg\nmime_type: audio/flac\nmime_type: audio/aac\n".to_vec();
            if !engine.catalog.demo
                && let Ok(local) = engine.catalog.local.request("decoders").await
            {
                reply.extend(local);
            }
            return Ok(reply);
        }
        "listpartitions" | "getpartitions" => "partition: default\n".into(),
        "partition" => {
            ensure!(arg(args, 1)? == "default", "rspc has one Spotify session");
            String::new()
        }
        "clearerror" => {
            engine.clear_error().await;
            String::new()
        }
        "update" | "rescan" => {
            engine.catalog.invalidate().await;
            if !engine.catalog.demo {
                let _ = engine.catalog.local.request(command).await;
            }
            engine.changed(vec!["database", "update"]);
            "updating_db: 1\n".into()
        }
        "rspc_shutdown" => bail!("shutdown is not allowed in a command list"),
        _ => return Err(UnknownCommand(command.to_owned()).into()),
    };
    Ok(text.into_bytes())
}
pub async fn serve(stream: UnixStream, engine: Arc<Engine>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    write.write_all(b"OK MPD 0.24.0\n").await?;
    let mut pending: Option<CommandList> = None;
    let mut events = engine.events.subscribe();
    let mut seen = events.borrow_and_update().clone();
    // Shared with the idle wait so a cancelled read keeps any partial command.
    let mut line = Vec::new();
    loop {
        if read_command(&mut read, &mut line).await? == 0 {
            return Ok(());
        }
        let command_bytes = line.len();
        let args = words(std::str::from_utf8(&line)?);
        line.clear();
        let args = match args {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => continue,
            Err(e) => {
                write
                    .write_all(format!("ACK [2@0] {{}} {}\n", clean(&e.to_string())).as_bytes())
                    .await?;
                continue;
            }
        };
        let cmd = args[0].as_str();
        if cmd == "close" {
            return Ok(());
        }
        // An event may have completed idle before its cancellation arrives.
        // MPD ignores noidle in that case; another OK would desynchronize the frontend.
        if cmd == "noidle" && pending.is_none() {
            continue;
        }
        if cmd == "rspc_shutdown" && pending.is_none() {
            write.write_all(b"OK\n").await?;
            engine.shutdown.send_replace(true);
            return Ok(());
        }
        if cmd == "command_list_begin" || cmd == "command_list_ok_begin" {
            ensure!(pending.is_none(), "nested command list");
            pending = Some(CommandList {
                separators: cmd == "command_list_ok_begin",
                bytes: 0,
                commands: Vec::new(),
            });
            continue;
        }
        if cmd == "command_list_end" {
            let CommandList {
                separators,
                commands,
                ..
            } = pending.take().context("no active command list")?;
            let mut failed = false;
            for (i, args) in commands.iter().enumerate() {
                match execute(&engine, args).await {
                    Ok(reply) => {
                        write.write_all(&reply).await?;
                        if separators {
                            write.write_all(b"list_OK\n").await?;
                        }
                    }
                    Err(e) => {
                        write.write_all(failure(&args[0], i, &e).as_bytes()).await?;
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                write.write_all(b"OK\n").await?;
            }
            continue;
        }
        if let Some(list) = &mut pending {
            list.push(args, command_bytes)?;
            continue;
        }
        if cmd == "idle" {
            loop {
                let versions = events.borrow_and_update().clone();
                let changed: Vec<_> = versions
                    .iter()
                    .filter(|(key, n)| {
                        seen.get(*key).copied().unwrap_or(0) != **n
                            && (args.len() == 1 || args.iter().skip(1).any(|s| s == *key))
                    })
                    .map(|(key, n)| (*key, *n))
                    .collect();
                if !changed.is_empty() {
                    for (subsystem, n) in changed {
                        seen.insert(subsystem, n);
                        write
                            .write_all(format!("changed: {subsystem}\n").as_bytes())
                            .await?;
                    }
                    write.write_all(b"OK\n").await?;
                    break;
                }
                tokio::select! {
                    result=events.changed()=>{if result.is_err(){return Ok(());}}
                    n=read_command(&mut read, &mut line)=>{
                        if n?==0{return Ok(());}
                        let cancel = std::str::from_utf8(&line)?.trim() == "noidle";
                        line.clear();
                        if !cancel{write.write_all(b"ACK [2@0] {idle} expected noidle\n").await?;}else{write.write_all(b"OK\n").await?;}
                        break;
                    }
                }
            }
            continue;
        }
        match execute(&engine, &args).await {
            Ok(reply) => {
                write.write_all(&reply).await?;
                write.write_all(b"OK\n").await?;
            }
            Err(e) => write.write_all(failure(cmd, 0, &e).as_bytes()).await?,
        }
    }
}
#[cfg(test)]
mod tests;
