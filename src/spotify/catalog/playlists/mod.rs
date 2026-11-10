//! Playlist reads, folder filtering and account-scoped rootlist caching.
mod edit;
use super::super::{
    client::Client,
    request::{self, Window, parameter},
};
use super::{
    entities,
    records::{self, Page},
};
use librespot_core::SpotifyId;
use librespot_metadata::{Episode, Track};
use librespot_protocol::playlist4_external::{Item, ListAttributes, SelectedListContent};
use protobuf::Message;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};

pub use edit::{add_items, create, edit, unlist};
#[derive(Default)]
pub struct PlaylistCache {
    root: RwLock<Option<(Instant, Arc<SelectedListContent>)>>,
    refresh: Mutex<()>,
    edits: Mutex<()>,
}
#[derive(Clone, Copy)]
enum Target<'a> {
    Root,
    Playlist(&'a str),
}
impl Target<'_> {
    fn path(self, client: &Client) -> anyhow::Result<String> {
        match self {
            Self::Root => {
                let username: String =
                    url::form_urlencoded::byte_serialize(client.username().as_bytes()).collect();
                Ok(format!("/playlist/v2/user/{username}/rootlist"))
            }
            Self::Playlist(id) => Ok(format!(
                "/playlist/v2/playlist/{}",
                SpotifyId::from_base62(id)?.to_base62()?
            )),
        }
    }
}
async fn fetch(
    client: &Client,
    target: Target<'_>,
    window: Window,
) -> anyhow::Result<SelectedListContent> {
    let path = format!(
        "{}?decorate=revision,attributes,length,owner,capabilities,status_code&from={}&length={}",
        target.path(client)?,
        window.offset,
        window.limit
    );
    let body = request::read(|| client.get(&path, "application/x-protobuf"))
        .await
        .map_err(request::upstream)?;
    Ok(SelectedListContent::parse_from_bytes(&body)?)
}
async fn complete(client: &Client, target: Target<'_>) -> anyhow::Result<SelectedListContent> {
    let mut all = fetch(
        client,
        target,
        Window {
            offset: 0,
            limit: 1000,
        },
    )
    .await?;
    let expected = usize::try_from(all.length().max(0))?;
    anyhow::ensure!(expected <= 100_000, "Playlist exceeds the supported size");
    while all.contents.items.len() < expected {
        let offset = all.contents.items.len();
        let page = fetch(
            client,
            target,
            Window {
                offset,
                limit: 1000,
            },
        )
        .await?;
        anyhow::ensure!(
            page.revision() == all.revision(),
            "Playlist changed while loading; reopen it"
        );
        anyhow::ensure!(
            !page.contents.items.is_empty(),
            "Incomplete playlist response"
        );
        let destination = all.contents.mut_or_insert_default();
        if let Some(mut contents) = page.contents.into_option() {
            destination.meta_items.resize_with(offset, Default::default);
            contents
                .meta_items
                .resize_with(contents.items.len(), Default::default);
            destination.items.append(&mut contents.items);
            destination.meta_items.append(&mut contents.meta_items);
        }
        anyhow::ensure!(
            destination.items.len() <= 100_000,
            "Playlist response exceeds its size limit"
        );
    }
    Ok(all)
}
async fn root(client: &Client, fresh: bool) -> anyhow::Result<Arc<SelectedListContent>> {
    if !fresh
        && let Some((_, value)) = client
            .playlists
            .root
            .read()
            .await
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
    {
        return Ok(Arc::clone(value));
    }
    let _refresh = client.playlists.refresh.lock().await;
    if !fresh
        && let Some((_, value)) = client
            .playlists
            .root
            .read()
            .await
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
    {
        return Ok(Arc::clone(value));
    }
    let value = Arc::new(complete(client, Target::Root).await?);
    *client.playlists.root.write().await = Some((Instant::now(), Arc::clone(&value)));
    Ok(value)
}
fn decoded(text: &str, spaces: bool) -> String {
    let input = if spaces {
        text.replace('+', " ")
    } else {
        text.to_owned()
    };
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    while pos < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16).map(|n| n as u8);
        if bytes[pos] == b'%'
            && pos + 2 < bytes.len()
            && let (Some(a), Some(b)) = (hex(bytes[pos + 1]), hex(bytes[pos + 2]))
        {
            result.push(a * 16 + b);
            pos += 3;
            continue;
        }
        result.push(bytes[pos]);
        pos += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}
fn visible(list: &SelectedListContent, rules: &crate::config::PlaylistExclusions) -> Vec<usize> {
    let mut stack: Vec<(String, bool)> = Vec::new();
    let mut output = Vec::new();
    for (index, row) in list.contents.items.iter().enumerate() {
        match row.uri().strip_prefix("spotify:start-group:") {
            Some(folder) => {
                let (id, label) = folder.split_once(':').unwrap_or((folder, ""));
                let label = decoded(label, false);
                let path = stack
                    .last()
                    .map_or(label.clone(), |(parent, _)| format!("{parent}/{label}"));
                let excluded = stack.last().is_some_and(|(_, hidden)| *hidden)
                    || rules.folder(id, &label, &path);
                stack.push((path, excluded));
            }
            None if row.uri().starts_with("spotify:end-group:") => {
                stack.pop();
            }
            None if row.uri().starts_with("spotify:playlist:") => {
                let metadata = list.contents.meta_items.get(index);
                let excluded = stack.last().is_some_and(|(_, hidden)| *hidden)
                    || metadata.is_some_and(|meta| meta.status_code() >= 400)
                    || rules.playlist(
                        row.uri(),
                        metadata.map_or("", |meta| meta.attributes.name()),
                    );
                if !excluded {
                    output.push(index);
                }
            }
            None => {}
        }
    }
    output
}
fn picture_list(attributes: &ListAttributes) -> Vec<records::Picture> {
    let mut pictures: Vec<_> = attributes
        .picture_size
        .iter()
        .map(|picture| {
            let size = match picture.target_name() {
                "xlarge" => 1280,
                "large" => 640,
                "default" => 300,
                _ => 64,
            };
            records::Picture {
                url: picture.url().into(),
                width: Some(size),
                height: Some(size),
            }
        })
        .collect();
    pictures.sort_by_key(|picture| std::cmp::Reverse(picture.width));
    if pictures.is_empty() && !attributes.picture().is_empty() {
        pictures.push(records::Picture {
            url: format!("https://i.scdn.co/image/{}", revision(attributes.picture())),
            width: None,
            height: None,
        });
    }
    pictures
}
fn revision(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
#[derive(Serialize)]
struct Owner<'a> {
    id: &'a str,
    display_name: &'a str,
}
fn summary(uri: &str, attrs: &ListAttributes, length: i32, owner: &str, rev: &[u8]) -> Value {
    json!({"uri":uri,"id":uri.strip_prefix("spotify:playlist:").unwrap_or_default(),"name":attrs.name(),"description":attrs.description(),"owner":Owner{id:owner,display_name:owner},"images":picture_list(attrs),"collaborative":attrs.collaborative(),"snapshot_id":revision(rev),"items":{"total":length.max(0)}})
}
pub async fn user_playlists(
    client: &Client,
    query: &[(&str, String)],
    rules: &crate::config::PlaylistExclusions,
) -> anyhow::Result<Value> {
    let list = root(client, false).await?;
    let indices = visible(&list, rules);
    let window = Window::query(query, 20);
    let rows: Vec<_> = window
        .select(&indices)
        .iter()
        .map(|&index| {
            let uri = list.contents.items[index].uri();
            match list.contents.meta_items.get(index) {
                Some(meta) => summary(
                    uri,
                    &meta.attributes,
                    meta.length(),
                    meta.owner_username(),
                    meta.revision(),
                ),
                None => summary(uri, &ListAttributes::new(), 0, "", &[]),
            }
        })
        .collect();
    Ok(records::value(Page::new(rows, window, indices.len())))
}
fn local(uri: &str) -> Value {
    let fields: Vec<_> = uri
        .splitn(6, ':')
        .skip(2)
        .map(|s| decoded(s, true))
        .collect();
    let field = |index| fields.get(index).map(String::as_str).unwrap_or_default();
    let duration = field(3)
        .parse::<u64>()
        .unwrap_or_default()
        .saturating_mul(1000);
    json!({"id":null,"uri":uri,"name":field(2),"artists":[{"name":field(0)}],"album":{"name":field(1),"artists":[],"images":[]},"duration_ms":duration,"track_number":0,"explicit":false,"is_local":true,"is_playable":false})
}
pub async fn playlist_items(
    client: &Client,
    id: &str,
    query: &[(&str, String)],
) -> anyhow::Result<Value> {
    let window = Window::query(query, 100);
    let list = fetch(client, Target::Playlist(id), window).await?;
    let items = &list.contents.items;
    let tracks: Vec<_> = items
        .iter()
        .filter(|i| i.uri().starts_with("spotify:track:"))
        .map(|i| i.uri().to_owned())
        .collect();
    let episodes: Vec<_> = items
        .iter()
        .filter(|i| i.uri().starts_with("spotify:episode:"))
        .map(|i| i.uri().to_owned())
        .collect();
    let (songs, spoken) = tokio::try_join!(
        entities::many::<Track>(client, &tracks),
        entities::many::<Episode>(client, &episodes)
    )?;
    let mut resolved = std::collections::HashMap::new();
    for (id, track) in tracks.into_iter().zip(songs) {
        if let Some(track) = track {
            resolved.insert(id, records::value(records::Song::from(track.as_ref())));
        }
    }
    for (id, episode) in episodes.into_iter().zip(spoken) {
        if let Some(episode) = episode {
            resolved.insert(
                id,
                records::value(records::SpokenTrack::from(episode.as_ref())),
            );
        }
    }
    let reduced = parameter(query, "fields").is_some();
    let rows: Vec<_> = items
        .iter()
        .map(|entry| {
            let is_local = entry.uri().starts_with("spotify:local:");
            let item = if is_local {
                local(entry.uri())
            } else {
                resolved.get(entry.uri()).cloned().unwrap_or(Value::Null)
            };
            let mut row = json!({"item":item,"is_local":is_local});
            if !reduced {
                if let Some(time) =
                    chrono::DateTime::from_timestamp_millis(entry.attributes.timestamp())
                        .filter(|_| entry.attributes.timestamp() > 0)
                {
                    row["added_at"] = json!(time.to_rfc3339());
                }
                if !entry.attributes.added_by().is_empty() {
                    row["added_by"] = records::value(Owner {
                        id: entry.attributes.added_by(),
                        display_name: entry.attributes.added_by(),
                    });
                }
            }
            row
        })
        .collect();
    Ok(records::value(Page::new(
        rows,
        window,
        list.length().max(0) as usize,
    )))
}

#[cfg(test)]
mod tests;
