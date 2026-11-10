use super::*;
use protobuf::MessageField;
#[test]
fn folder_rules_apply_to_decoded_paths_and_descendants() {
    let uris = [
        "spotify:playlist:before",
        "spotify:start-group:one:A%2FB+%26%3D",
        "spotify:playlist:hidden",
        "spotify:start-group:two:Child",
        "spotify:playlist:nested",
        "spotify:end-group:two",
        "spotify:end-group:one",
        "spotify:playlist:after",
    ];
    let mut list = SelectedListContent::new();
    list.contents = MessageField::some(librespot_protocol::playlist4_external::ListItems {
        items: uris
            .iter()
            .map(|uri| Item {
                uri: Some((*uri).into()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    let rules = crate::config::PlaylistExclusions {
        exclude_folders: vec!["A/B+&=".into()],
        ..Default::default()
    };
    assert_eq!(visible(&list, &rules), [0, 7]);
}
#[test]
fn local_track_names_and_duration_are_decoded_without_overflow() {
    let value = local("spotify:local:An+Artist:Some%3AAlbum:A%2BB:18446744073709551615");
    assert_eq!(value["artists"][0]["name"], "An Artist");
    assert_eq!(value["name"], "A+B");
    assert_eq!(value["album"]["name"], "Some:Album");
    assert_eq!(value["duration_ms"], u64::MAX);
}
#[test]
fn undecorated_and_forbidden_playlists_do_not_shift_the_folder_stack() {
    let mut list = SelectedListContent::new();
    list.contents = MessageField::some(librespot_protocol::playlist4_external::ListItems {
        items: vec![
            Item {
                uri: Some("spotify:playlist:one".into()),
                ..Default::default()
            },
            Item {
                uri: Some("spotify:playlist:two".into()),
                ..Default::default()
            },
        ],
        meta_items: vec![librespot_protocol::playlist4_external::MetaItem {
            status_code: Some(403),
            ..Default::default()
        }],
        ..Default::default()
    });
    assert_eq!(visible(&list, &Default::default()), [1]);
}
