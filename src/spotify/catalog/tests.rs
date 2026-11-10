//! Fixtures exercise the real catalog transport and protobuf codecs, without accounts.
use super::*;
use librespot_protocol::{
    entity_extension_data::EntityExtensionData, extended_metadata as extended,
    extension_kind::ExtensionKind, metadata as wire,
};
use protobuf::{EnumOrUnknown, Message, MessageField};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

struct Request {
    path: String,
    body: Vec<u8>,
}
fn playlist_page(
    total: i32,
    revision: u8,
    rows: &[&str],
) -> librespot_protocol::playlist4_external::SelectedListContent {
    use librespot_protocol::playlist4_external::{Item, ListItems, SelectedListContent};
    SelectedListContent {
        length: Some(total),
        revision: Some(vec![revision]),
        contents: MessageField::some(ListItems {
            pos: Some(0),
            truncated: Some(rows.len() < (total as usize)),
            items: rows
                .iter()
                .map(|uri| Item {
                    uri: Some((*uri).into()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn rootlist_metadata_stays_aligned_when_a_page_has_no_decorations() {
    use librespot_protocol::playlist4_external::{ListAttributes, MetaItem};
    let first = playlist_page(2, 1, &["spotify:playlist:first"]);
    let mut second = playlist_page(2, 1, &["spotify:playlist:second"]);
    second
        .contents
        .mut_or_insert_default()
        .meta_items
        .push(MetaItem {
            attributes: MessageField::some(ListAttributes {
                name: Some("Second playlist".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
    let (client, task) = server(vec![
        (200, first.write_to_bytes().unwrap()),
        (200, second.write_to_bytes().unwrap()),
    ])
    .await;
    let result = playlists::user_playlists(&client, &[], &Default::default())
        .await
        .unwrap();
    assert_eq!(result["items"][0]["id"], "first");
    assert_eq!(result["items"][0]["name"], "");
    assert_eq!(result["items"][1]["name"], "Second playlist");
    assert_eq!(task.await.unwrap().len(), 2);
}

#[tokio::test]
async fn adding_to_a_large_playlist_reads_only_its_header() {
    use librespot_protocol::playlist4_external::ListChanges;
    let first = playlist_page(5000, 1, &["fixture-existing"]);
    let (client, task) = server(vec![
        (200, first.write_to_bytes().unwrap()),
        (200, first.write_to_bytes().unwrap()),
    ])
    .await;
    let id = librespot_core::SpotifyId::from_raw(&[8; 16])
        .unwrap()
        .to_base62()
        .unwrap();
    playlists::edit(
        &client,
        &id,
        crate::spotify::PlaylistChange::Add {
            uris: vec![uri("track", 1)],
            position: Some(5000),
        },
    )
    .await
    .unwrap();
    let requests = task.await.unwrap();
    assert!(requests[0].path.contains("length=1 "));
    let change = ListChanges::parse_from_bytes(&requests[1].body).unwrap();
    assert_eq!(change.deltas[0].ops[0].add.from_index(), 5000);
    assert_eq!(change.deltas[0].ops[0].add.items.len(), 1);
}

#[tokio::test]
async fn a_revision_change_between_pages_prevents_an_edit() {
    let first = playlist_page(2, 1, &["fixture-one"]);
    let second = playlist_page(2, 2, &["fixture-two"]);
    let (client, task) = server(vec![
        (200, first.write_to_bytes().unwrap()),
        (200, second.write_to_bytes().unwrap()),
    ])
    .await;
    let id = librespot_core::SpotifyId::from_raw(&[8; 16])
        .unwrap()
        .to_base62()
        .unwrap();
    assert!(
        playlists::edit(&client, &id, crate::spotify::PlaylistChange::Clear)
            .await
            .is_err()
    );
    let requests = task.await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.path.starts_with("GET "))
    );
}
async fn server(replies: Vec<(u16, Vec<u8>)>) -> (Client, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::fixture(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut stream = BufReader::new(stream);
            let mut request_line = String::new();
            stream.read_line(&mut request_line).await.unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((header, value)) = line.trim().split_once(':')
                    && header.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await.unwrap();
            requests.push(Request {
                path: request_line,
                body: bytes,
            });
            stream
                .get_mut()
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.get_mut().write_all(&body).await.unwrap();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(650), listener.accept())
                .await
                .is_err(),
            "unexpected extra catalog request"
        );
        requests
    });
    (client, task)
}
fn album_message() -> wire::Album {
    wire::Album {
        gid: Some(vec![2; 16]),
        name: Some("A saved release".into()),
        date: MessageField::some(wire::Date {
            year: Some(2025),
            ..Default::default()
        }),
        artist: vec![wire::Artist {
            gid: Some(vec![3; 16]),
            name: Some("Album artist".into()),
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn uri(kind: &str, byte: u8) -> String {
    format!(
        "spotify:{kind}:{}",
        librespot_core::SpotifyId::from_raw(&[byte; 16])
            .unwrap()
            .to_base62()
            .unwrap()
    )
}
fn metadata(kind: ExtensionKind, uri: String, message: impl Message) -> Vec<u8> {
    extended::BatchedExtensionResponse {
        extended_metadata: vec![extended::EntityExtensionDataArray {
            extension_kind: EnumOrUnknown::new(kind),
            extension_data: vec![EntityExtensionData {
                entity_uri: uri,
                extension_data: MessageField::some(protobuf::well_known_types::any::Any {
                    value: message.write_to_bytes().unwrap(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}
#[tokio::test]
async fn parallel_metadata_reads_share_a_batch_and_keep_input_order() {
    let song = wire::Track {
        gid: Some(vec![1; 16]),
        name: Some("A song".into()),
        duration: Some(5000),
        album: MessageField::some(album_message()),
        artist: vec![wire::Artist {
            gid: Some(vec![4; 16]),
            name: Some("Track artist".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let id = uri("track", 1);
    let (client, task) = server(vec![(
        200,
        metadata(ExtensionKind::TRACK_V4, id.clone(), song),
    )])
    .await;
    let ids = vec![id.clone(), uri("track", 9), id];
    let (a, b) = tokio::join!(
        entities::many::<Track>(&client, &ids),
        entities::many::<Track>(&client, &ids)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.len(), 3);
    assert!(a[1].is_none());
    assert!(std::sync::Arc::ptr_eq(
        a[0].as_ref().unwrap(),
        a[2].as_ref().unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        a[0].as_ref().unwrap(),
        b[0].as_ref().unwrap()
    ));
    let value = records::value(Song::from(a[0].as_deref().unwrap()));
    assert_eq!(value["artists"][0]["name"], "Track artist");
    assert_eq!(value["album"]["artists"][0]["name"], "Album artist");
    assert!(value["album"].get("rspc_track_uris").is_none());
    let requests = task.await.unwrap();
    assert!(requests[0].path.contains("/extended-metadata/"));
    let decoded = extended::BatchedEntityRequest::parse_from_bytes(&requests[0].body).unwrap();
    assert_eq!(decoded.entity_request.len(), 2);
}
#[tokio::test]
async fn saved_albums_exclude_liked_tracks_and_reuse_the_collection_snapshot() {
    use crate::spotify::protocol::collection::{CollectionItem, PageResponse};
    let album = uri("album", 2);
    let collection = PageResponse {
        items: vec![
            CollectionItem {
                uri: uri("track", 1),
                added_at: 10,
                ..Default::default()
            },
            CollectionItem {
                uri: album.clone(),
                added_at: 20,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (client, task) = server(vec![
        (200, collection.write_to_bytes().unwrap()),
        (
            200,
            metadata(ExtensionKind::ALBUM_V4, album, album_message()),
        ),
    ])
    .await;
    let page = collection::saved_albums(
        &client,
        Window {
            offset: 0,
            limit: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["album"]["name"], "A saved release");
    assert!(page["items"][0]["added_at"].is_string());
    let repeated = collection::saved_albums(
        &client,
        Window {
            offset: 0,
            limit: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(page, repeated);
    assert_eq!(task.await.unwrap().len(), 2);
}
#[tokio::test]
async fn a_repeated_collection_cursor_returns_an_error_instead_of_looping() {
    use crate::spotify::protocol::collection::PageResponse;
    let page = PageResponse {
        next_page_token: "same-cursor".into(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    let (client, task) = server(vec![(200, page.clone()), (200, page)]).await;
    assert!(
        collection::saved_tracks(
            &client,
            Window {
                offset: 0,
                limit: 10
            }
        )
        .await
        .is_err()
    );
    assert_eq!(task.await.unwrap().len(), 2);
}
#[tokio::test]
async fn search_categories_share_one_response_and_keep_owner_identity() {
    let body = serde_json::json!({"data":{"searchV2":{"playlists":{"totalCount":1,"items":[{"data":{"uri":"spotify:playlist:fixture","name":"An owner's mix","ownerV2":{"data":{"username":"fixture-owner","name":"A curator"}}}}]},"artists":{"totalCount":0,"items":[]}}}});
    let (client, task) = server(vec![(200, serde_json::to_vec(&body).unwrap())]).await;
    let playlists = [("q", "one query".into()), ("type", "playlist".into())];
    let artists = [("q", "one query".into()), ("type", "artist".into())];
    let (a, b) = tokio::join!(
        search::search(&client, &playlists),
        search::search(&client, &artists)
    );
    assert_eq!(
        a.unwrap()["playlists"]["items"][0]["owner"]["id"],
        "fixture-owner"
    );
    assert_eq!(b.unwrap()["artists"]["items"].as_array().unwrap().len(), 0);
    let captured = task.await.unwrap();
    let request: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(request["operationName"], "searchDesktop");
}
#[tokio::test]
async fn a_failed_playlist_write_is_sent_once_and_preserves_its_revision() {
    use librespot_protocol::playlist4_external::{ListChanges, ListItems, SelectedListContent};
    let current = SelectedListContent {
        revision: Some(vec![1, 2, 3]),
        length: Some(0),
        contents: MessageField::some(ListItems {
            pos: Some(0),
            truncated: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (client, task) = server(vec![
        (200, current.write_to_bytes().unwrap()),
        (503, Vec::new()),
    ])
    .await;
    let id = librespot_core::SpotifyId::from_raw(&[8; 16])
        .unwrap()
        .to_base62()
        .unwrap();
    assert!(
        playlists::edit(
            &client,
            &id,
            crate::spotify::PlaylistChange::Rename("New name".into())
        )
        .await
        .is_err()
    );
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].path.contains("/changes"));
    let sent = ListChanges::parse_from_bytes(&requests[1].body).unwrap();
    assert_eq!(sent.base_revision(), [1, 2, 3]);
    assert_eq!(
        sent.deltas[0].ops[0]
            .update_list_attributes
            .new_attributes
            .values
            .name(),
        "New name"
    );
}
