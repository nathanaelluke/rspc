//! rspc's optional paged artist index. Native MPD keeps the tag-based browser.
use super::{FETCH_ALBUM, FETCH_ARTIST_PAGE, TagBrowserPane};
use crate::mpd::commands::{
    artist_albums::{ArtistAlbum, ArtistAlbumPage},
    lsinfo::LsInfoEntry,
};
use crate::{
    MpdQueryResult,
    config::artists::{AlbumDisplayMode, AlbumSortMode},
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    ui::{
        dir_or_song::DirOrSong,
        dirstack::{DirStackItem, Path},
    },
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

#[derive(Debug, Default)]
pub(super) struct ArtistPages {
    albums: Vec<ArtistAlbum>,
    offsets: HashSet<usize>,
    next: Option<usize>,
}

impl TagBrowserPane {
    pub(super) fn visible_artist(&self) -> Option<&str> {
        self.stack
            .path()
            .as_slice()
            .first()
            .map(String::as_str)
            .or_else(|| self.stack.current().selected().map(DirStackItem::as_path))
    }

    pub(super) fn fetch_artist_page(&self, artist: &str, ctx: &Ctx) {
        let offset = match self.artist_pages.get(artist) {
            Some(pages) => match pages.next {
                Some(offset) => offset,
                None => return,
            },
            None => 0,
        };
        let artist = artist.to_owned();
        ctx.query()
            .id(FETCH_ARTIST_PAGE)
            .replace_id(FETCH_ARTIST_PAGE)
            .target(self.target_pane.clone())
            .query(move |client| {
                let page = client.artist_album_page(&artist, offset)?;
                Ok(MpdQueryResult::ArtistAlbums {
                    artist,
                    offset,
                    page,
                })
            });
    }

    pub(super) fn process_artist_page(
        &mut self,
        artist: String,
        offset: usize,
        page: ArtistAlbumPage,
        ctx: &Ctx,
    ) {
        let pages = self.artist_pages.entry(artist.clone()).or_default();
        if !pages.offsets.insert(offset) {
            return;
        }
        pages.next = page.next.filter(|next| *next > offset);
        let mut uris: HashSet<_> = pages.albums.iter().map(|album| album.uri.clone()).collect();
        pages.albums.extend(
            page.albums
                .into_iter()
                .filter(|album| uris.insert(album.uri.clone())),
        );
        let use_date = ctx
            .config
            .artists
            .album_date_tags
            .contains(&crate::config::artists::AlbumDateTag::Date);
        let mut albums: Vec<_> = pages.albums.iter().collect();
        albums.sort_by(|a, b| {
            let dates = if use_date {
                a.date.cmp(&b.date)
            } else {
                Ordering::Equal
            };
            match ctx.config.artists.album_sort_by {
                AlbumSortMode::Name => a.name.cmp(&b.name).then(dates),
                AlbumSortMode::Date => dates.then(a.name.cmp(&b.name)),
            }
        });
        let path = Path::from(artist);
        let mut names = Vec::new();
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for album in albums {
            let name = match ctx.config.artists.album_display_mode {
                AlbumDisplayMode::NameOnly => album.name.clone(),
                AlbumDisplayMode::SplitByDate => format!(
                    "({}) {}",
                    if !use_date || album.date.is_empty() {
                        "<no date>"
                    } else {
                        &album.date
                    },
                    album.name
                ),
            };
            if !groups.contains_key(&name) {
                names.push(name.clone());
            }
            groups.entry(name).or_default().push(album.uri.clone());
        }
        for (name, sources) in groups {
            let album_path = path.join(name);
            if self
                .album_sources
                .get(&album_path)
                .is_some_and(|old| old != &sources)
            {
                // A later page can add another release to an existing group.
                self.loaded_albums.remove(&album_path);
            }
            self.album_sources.insert(album_path, sources);
        }
        // Keep the directory, filter buffer, scroll position and marks when a page arrives.
        if self.stack.get(&path).is_none() {
            self.stack.insert(path.clone(), Vec::new());
        }
        let dir = self
            .stack
            .get_mut(&path)
            .expect("inserted artist directory");
        let selected = dir.selected().map(|item| item.as_path().to_owned());
        let marked: HashSet<_> = dir
            .state
            .marked
            .iter()
            .filter_map(|&i| dir.items.get(i))
            .map(|item| item.as_path().to_owned())
            .collect();
        dir.items = names.into_iter().map(DirOrSong::name_only).collect();
        dir.state.set_content_len(Some(dir.items.len()));
        dir.state.marked = dir
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| marked.contains(item.as_path()).then_some(i))
            .collect();
        let selected = selected
            .and_then(|name| dir.items.iter().position(|item| item.as_path() == name))
            .or_else(|| (!dir.items.is_empty()).then_some(0));
        dir.state.select(selected, ctx.config.scrolloff);
        dir.recalculate_matched_items(&ctx.config.theme.browser_song_format.0, ctx);
    }

    pub(super) fn fetch_paged_data(&self, selected: &DirOrSong, ctx: &Ctx) {
        match self.stack.path().as_slice() {
            [] => self.fetch_artist_page(selected.as_path(), ctx),
            [artist] => {
                let path = self.stack.path().join(selected.as_path());
                if self.loaded_albums.contains(&path) {
                    self.fetch_artist_page(artist, ctx);
                    return;
                }
                let Some(sources) = self.album_sources.get(&path).cloned() else {
                    return;
                };
                ctx.query()
                    .id(FETCH_ALBUM)
                    .replace_id(FETCH_ALBUM)
                    .target(self.target_pane.clone())
                    .query(move |client| {
                        let mut data = Vec::new();
                        for source in sources {
                            data.extend(client.lsinfo(Some(&source))?.0.into_iter().filter_map(
                                |item| match item {
                                    LsInfoEntry::File(song) => Some(song),
                                    _ => None,
                                },
                            ));
                        }
                        Ok(MpdQueryResult::SongsList {
                            data,
                            path: Some(path),
                        })
                    });
                self.fetch_artist_page(artist, ctx);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::browser::BrowserPane;
    use crate::{config::tabs::PaneType, mpd::mpd_client::Tag, ui::dirstack::DirStack};
    use crate::{shared::mpd_client_ext::Enqueue, tests::fixtures::ctx};
    use itertools::Itertools;
    use rstest::rstest;
    use std::sync::Arc;

    fn album(uri: &str, name: &str, date: &str) -> ArtistAlbum {
        ArtistAlbum {
            uri: uri.into(),
            name: name.into(),
            date: date.into(),
        }
    }

    #[rstest]
    fn pages_preserve_selection_marks_and_unvisited_releases(ctx: Ctx) {
        let mut pane = TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx);
        pane.paged_artists = true;
        pane.stack = DirStack::new(vec![DirOrSong::name_only("Composer".into())]);
        pane.process_artist_page(
            "Composer".into(),
            0,
            ArtistAlbumPage {
                albums: vec![
                    album("release-b", "B", "2002"),
                    album("release-c", "C", "2003"),
                ],
                next: Some(100),
            },
            &ctx,
        );
        pane.stack.enter();
        pane.stack.current_mut().select_idx(1, 0);
        pane.stack.current_mut().state.mark(0);
        pane.process_artist_page(
            "Composer".into(),
            100,
            ArtistAlbumPage {
                albums: vec![album("release-a", "A", "2001")],
                next: None,
            },
            &ctx,
        );
        assert_eq!(
            pane.stack
                .current()
                .selected()
                .expect("selected album")
                .as_path(),
            "(2003) C"
        );
        assert_eq!(
            pane.stack
                .current()
                .state
                .marked
                .iter()
                .copied()
                .collect_vec(),
            vec![1]
        );
        assert_eq!(
            pane.stack.len(),
            2,
            "indexing must not create song directories"
        );
        let (queued, _) = pane.enqueue(pane.stack.current().items.iter());
        assert_eq!(queued.len(), 3);
        assert!(matches!(&queued[0], Enqueue::File { path } if path == "release-a"));
        pane.stack.leave();
        let (queued, _) = pane.enqueue(pane.stack.current().items.iter());
        assert!(matches!(&queued[0], Enqueue::Find { filter } if filter[0].2 == "Composer"));
    }

    #[rstest]
    fn name_only_merges_sources_without_fetching_tracks(mut ctx: Ctx) {
        Arc::make_mut(&mut ctx.config).artists.album_display_mode = AlbumDisplayMode::NameOnly;
        let mut pane = TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx);
        pane.process_artist_page(
            "Composer".into(),
            0,
            ArtistAlbumPage {
                albums: vec![
                    album("spotify", "Album", "2000"),
                    album("local", "Album", "2001"),
                ],
                next: None,
            },
            &ctx,
        );
        let path = Path::from("Composer").join("Album");
        assert_eq!(pane.album_sources[&path], ["spotify", "local"]);
        assert!(pane.stack.get(&path).is_none());
        // Replayed responses do not duplicate releases.
        pane.process_artist_page("Composer".into(), 0, ArtistAlbumPage::default(), &ctx);
        assert_eq!(pane.album_sources[&path].len(), 2);
    }

    #[rstest]
    fn only_artists_use_optional_paging(mut ctx: Ctx) {
        ctx.supported_commands.insert("rspc_artist_albums".into());
        assert!(TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx).paged_artists);
        assert!(
            !TagBrowserPane::new(Tag::AlbumArtist, PaneType::AlbumArtists, None, &ctx)
                .paged_artists
        );
        ctx.supported_commands.clear();
        assert!(!TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, &ctx).paged_artists);
    }
}
