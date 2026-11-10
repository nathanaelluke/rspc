//! Build revision-checked playlist transactions. Writes are sent once.
use super::super::super::{PlaylistChange, request};
use super::{Client, Item, ListAttributes, Target, complete, revision, root, summary};
use anyhow::{Context, Result, ensure};
use librespot_core::SpotifyId;
use librespot_protocol::playlist4_external::{self as wire, op::Kind};
use protobuf::MessageField;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn stamped(uri: String, now: i64, public: Option<bool>) -> Item {
    Item {
        uri: Some(uri),
        attributes: MessageField::some(wire::ItemAttributes {
            timestamp: Some(now),
            public,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn insertion(items: Vec<Item>, at: Option<usize>) -> Result<wire::Op> {
    let add = wire::Add {
        items,
        from_index: at.map(i32::try_from).transpose()?,
        add_last: at.is_none().then_some(true),
        ..Default::default()
    };
    Ok(wire::Op {
        kind: Some(Kind::ADD.into()),
        add: MessageField::some(add),
        ..Default::default()
    })
}
fn deletion(at: usize, items: Vec<Item>) -> Result<wire::Op> {
    let rem = wire::Rem {
        from_index: Some(i32::try_from(at)?),
        length: Some(i32::try_from(items.len())?),
        items,
        ..Default::default()
    };
    Ok(wire::Op {
        kind: Some(Kind::REM.into()),
        rem: MessageField::some(rem),
        ..Default::default()
    })
}
fn rename(name: &str, description: Option<&str>) -> wire::Op {
    let values = ListAttributes {
        name: Some(name.to_owned()),
        description: description.map(str::to_owned),
        ..Default::default()
    };
    let update = wire::UpdateListAttributes {
        new_attributes: MessageField::some(wire::ListAttributesPartialState {
            values: MessageField::some(values),
            ..Default::default()
        }),
        ..Default::default()
    };
    wire::Op {
        kind: Some(Kind::UPDATE_LIST_ATTRIBUTES.into()),
        update_list_attributes: MessageField::some(update),
        ..Default::default()
    }
}
fn envelope(base: &[u8], ops: Vec<wire::Op>, user: String, now: i64) -> wire::ListChanges {
    wire::ListChanges {
        base_revision: Some(base.to_vec()),
        deltas: vec![wire::Delta {
            ops,
            info: MessageField::some(wire::ChangeInfo {
                user: Some(user),
                timestamp: Some(now),
                ..Default::default()
            }),
            ..Default::default()
        }],
        want_resulting_revisions: Some(true),
        want_sync_result: Some(true),
        ..Default::default()
    }
}

fn selected(rows: &[Item], entries: Vec<(usize, String)>) -> Result<BTreeMap<usize, Item>> {
    let mut chosen = BTreeMap::new();
    for (position, uri) in entries {
        let item = rows
            .get(position)
            .filter(|item| item.uri() == uri)
            .context("Playlist changed; reopen it before editing")?;
        ensure!(
            chosen.insert(position, item.clone()).is_none(),
            "A playlist position was selected twice"
        );
    }
    Ok(chosen)
}
/// Merge adjacent deletions, highest index first, so positions stay valid.
fn removals(chosen: &BTreeMap<usize, Item>) -> Result<Vec<wire::Op>> {
    let mut groups: Vec<(usize, Vec<Item>)> = Vec::new();
    for (&position, item) in chosen {
        if let Some((start, items)) = groups
            .last_mut()
            .filter(|(start, items)| *start + items.len() == position)
        {
            let _ = start;
            items.push(item.clone());
        } else {
            groups.push((position, vec![item.clone()]));
        }
    }
    groups
        .into_iter()
        .rev()
        .map(|(start, items)| deletion(start, items))
        .collect()
}
fn plan(rows: &[Item], length: usize, change: PlaylistChange, now: i64) -> Result<Vec<wire::Op>> {
    match change {
        PlaylistChange::Rename(name) => Ok(vec![rename(&name, None)]),
        PlaylistChange::Clear => {
            if rows.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![deletion(0, rows.to_vec())?])
            }
        }
        PlaylistChange::Add { uris, position } => {
            ensure!(
                position.is_none_or(|at| at <= length),
                "Playlist destination is out of range"
            );
            let mut items = Vec::new();
            for uri in uris {
                let parsed = librespot_core::SpotifyUri::from_uri(&uri)?;
                ensure!(
                    matches!(
                        parsed,
                        librespot_core::SpotifyUri::Track { .. }
                            | librespot_core::SpotifyUri::Episode { .. }
                    ),
                    "Only tracks and episodes can be added to Spotify playlists"
                );
                items.push(stamped(uri, now, None));
            }
            if items.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![insertion(items, position)?])
            }
        }
        PlaylistChange::Remove { entries } => removals(&selected(rows, entries)?),
        PlaylistChange::Move { entries, to } => {
            let chosen = selected(rows, entries)?;
            ensure!(
                to <= rows.len() - chosen.len(),
                "Playlist destination is out of range"
            );
            let mut operations = removals(&chosen)?;
            if !chosen.is_empty() {
                operations.push(insertion(chosen.into_values().collect(), Some(to))?);
            }
            Ok(operations)
        }
    }
}
async fn send(client: &Client, target: Target<'_>, changes: &wire::ListChanges) -> Result<Value> {
    *client.playlists.root.write().await = None;
    let reply = match target {
        Target::Root => client.rootlist_changes(changes).await,
        Target::Playlist(id) => {
            client
                .playlist_changes(&SpotifyId::from_base62(id)?, changes)
                .await
        }
    }
    .map_err(request::upstream)?;
    Ok(json!({"snapshot_id":revision(reply.revision())}))
}
pub async fn edit(client: &Client, id: &str, change: PlaylistChange) -> Result<()> {
    let _edit = client.playlists.edits.lock().await;
    let target = Target::Playlist(id);
    let current = if matches!(
        &change,
        PlaylistChange::Add { .. } | PlaylistChange::Rename(_)
    ) {
        super::fetch(
            client,
            target,
            crate::spotify::request::Window {
                offset: 0,
                limit: 1,
            },
        )
        .await?
    } else {
        complete(client, target).await?
    };
    let now = chrono::Utc::now().timestamp_millis();
    let operations = plan(
        &current.contents.items,
        current.length().max(0) as usize,
        change,
        now,
    )?;
    if !operations.is_empty() {
        send(
            client,
            Target::Playlist(id),
            &envelope(current.revision(), operations, client.username(), now),
        )
        .await?;
    }
    Ok(())
}
pub async fn add_items(client: &Client, id: &str, body: Option<&Value>) -> Result<Value> {
    let uris = body
        .and_then(|value| value["uris"].as_array())
        .context("Missing playlist items")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("Invalid playlist item")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!uris.is_empty(), "No playlist items were supplied");
    edit(
        client,
        id,
        PlaylistChange::Add {
            uris,
            position: None,
        },
    )
    .await?;
    Ok(Value::Null)
}
pub async fn create(client: &Client, body: Option<&Value>) -> Result<Value> {
    let _edit = client.playlists.edits.lock().await;
    let name = body
        .and_then(|body| body["name"].as_str())
        .unwrap_or("New Playlist");
    let description = body.and_then(|body| body["description"].as_str());
    let public = body
        .and_then(|body| body["public"].as_bool())
        .unwrap_or(true);
    let id = SpotifyId::from_raw(uuid::Uuid::new_v4().as_bytes())?.to_base62()?;
    let now = chrono::Utc::now().timestamp_millis();
    let changes = envelope(
        b"\x00\x00\x00\x00root",
        vec![rename(name, description)],
        client.username(),
        now,
    );
    send(client, Target::Playlist(&id), &changes).await?;
    let uri = format!("spotify:playlist:{id}");
    let current = root(client, true).await?;
    let changes = envelope(
        current.revision(),
        vec![insertion(
            vec![stamped(uri.clone(), now, Some(public))],
            Some(0),
        )?],
        client.username(),
        now,
    );
    send(client, Target::Root, &changes).await?;
    let attributes = ListAttributes {
        name: Some(name.into()),
        description: description.map(str::to_owned),
        ..Default::default()
    };
    Ok(summary(&uri, &attributes, 0, &client.username(), &[]))
}
pub async fn unlist(client: &Client, uri: &str) -> Result<()> {
    let _edit = client.playlists.edits.lock().await;
    let current = root(client, true).await?;
    if let Some(position) = current
        .contents
        .items
        .iter()
        .position(|item| item.uri() == uri)
    {
        let now = chrono::Utc::now().timestamp_millis();
        let operation = deletion(position, vec![current.contents.items[position].clone()])?;
        send(
            client,
            Target::Root,
            &envelope(current.revision(), vec![operation], client.username(), now),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    fn rows() -> Vec<Item> {
        (0..6)
            .map(|n| stamped(format!("fixture-{n}"), n, None))
            .collect()
    }
    #[test]
    fn moves_validate_positions_and_preserve_existing_attributes() {
        let original = rows();
        let operations = plan(
            &original,
            original.len(),
            PlaylistChange::Move {
                entries: vec![(3, "fixture-3".into()), (1, "fixture-1".into())],
                to: 0,
            },
            99,
        )
        .unwrap();
        assert_eq!(operations[0].rem.from_index(), 3);
        assert_eq!(operations[1].rem.from_index(), 1);
        assert_eq!(
            operations[2].add.items,
            [original[1].clone(), original[3].clone()]
        );
        assert!(
            plan(
                &original,
                original.len(),
                PlaylistChange::Remove {
                    entries: vec![(2, "stale".into())]
                },
                99
            )
            .is_err()
        );
        assert!(
            plan(
                &original,
                original.len(),
                PlaylistChange::Move {
                    entries: vec![(1, "fixture-1".into()), (1, "fixture-1".into())],
                    to: 0
                },
                99
            )
            .is_err()
        );
    }
    #[test]
    fn adjacent_deletions_are_merged_without_removing_other_occurrences() {
        let changes = plan(
            &rows(),
            6,
            PlaylistChange::Remove {
                entries: vec![
                    (1, "fixture-1".into()),
                    (2, "fixture-2".into()),
                    (5, "fixture-5".into()),
                ],
            },
            99,
        )
        .unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            (changes[0].rem.from_index(), changes[0].rem.length()),
            (5, 1)
        );
        assert_eq!(
            (changes[1].rem.from_index(), changes[1].rem.length()),
            (1, 2)
        );
    }
    #[test]
    fn the_wire_transaction_keeps_the_base_revision_and_writer() {
        let ops = plan(&rows(), 6, PlaylistChange::Clear, 100).unwrap();
        let change = envelope(&[1, 2, 3], ops, "fixture-user".into(), 100);
        let parsed =
            wire::ListChanges::parse_from_bytes(&change.write_to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.base_revision(), [1, 2, 3]);
        assert_eq!(parsed.deltas[0].info.user(), "fixture-user");
        assert_eq!(parsed.deltas[0].ops[0].rem.length(), 6);
        assert!(parsed.want_sync_result());
    }
}
