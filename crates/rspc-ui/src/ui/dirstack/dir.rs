use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::{Bound, RangeBounds},
};

use log::error;
use ratatui::{text::Span, widgets::ListItem};

use super::{DirStackItem, state::DirState};
use crate::{
    config::{
        sort_mode::{SongSort, SongSortKey},
        theme::properties::{Property, SongProperty},
    },
    ctx::Ctx,
    shared::macros::status_warn,
    ui::{FILTER_PREFIX, dirstack::ScrollingState, input::BufferId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RowIdentity<'a> {
    path: &'a str,
    added: Option<chrono::DateTime<chrono::Utc>>,
    occurrence: usize,
}

#[derive(Debug)]
pub struct Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    pub items: Vec<T>,
    pub loaded: bool,
    pub state: DirState<S>,
    matched_item_count: usize,
    pub filter_buffer_id: BufferId,
    pub filter_active: bool,
    view_sort: SongSort,
    // Backend positions travel with rows when the visible order changes.
    original_order: Vec<usize>,
}

impl<T, S> Default for Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn default() -> Self {
        Self {
            loaded: false,
            items: Vec::default(),
            state: DirState::default(),
            matched_item_count: 0,
            filter_buffer_id: BufferId::new(),
            filter_active: false,
            view_sort: SongSort::default(),
            original_order: Vec::new(),
        }
    }
}

#[allow(dead_code)]
impl<T, S> Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    pub fn new(root: Vec<T>) -> Self {
        let mut result = Self {
            loaded: true,
            ..Self::default()
        };

        if !root.is_empty() {
            result.state.select(Some(0), 0);
            result.state.set_content_len(Some(root.len()));
            result.items = root;
        }

        result
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn new_with_state(items: Vec<T>, state: DirState<S>) -> Self {
        return Self {
            loaded: true,
            items,
            state,
            ..Self::default()
        };
    }

    pub fn source_index(&self, index: usize) -> usize {
        self.original_order.get(index).copied().unwrap_or(index)
    }

    fn row_identities(&self) -> Vec<RowIdentity<'_>> {
        let mut indices = (0..self.items.len()).collect::<Vec<_>>();
        indices.sort_unstable_by_key(|index| self.source_index(*index));
        let mut occurrences = HashMap::new();
        let mut identities = vec![None; self.items.len()];
        for index in indices {
            let item = &self.items[index];
            let path = item.as_path();
            let added = item.as_song().and_then(|song| song.added);
            let occurrence = occurrences.entry((path, added)).or_insert(0);
            identities[index] = Some(RowIdentity {
                path,
                added,
                occurrence: *occurrence,
            });
            *occurrence += 1;
        }
        identities.into_iter().flatten().collect()
    }

    pub fn restore_selection(&mut self, previous: &Self, scrolloff: usize) {
        let previous_ids = previous.row_identities();
        let selected = previous
            .state
            .get_selected()
            .and_then(|index| previous_ids.get(index))
            .copied();
        let marked = previous
            .marked()
            .iter()
            .filter_map(|index| previous_ids.get(*index))
            .copied()
            .collect::<HashSet<_>>();
        let current_ids = self.row_identities();
        let selected_index = current_ids
            .iter()
            .position(|identity| Some(*identity) == selected)
            .or_else(|| previous.state.get_selected())
            .or_else(|| self.state.get_selected());
        let marked_indices = current_ids
            .iter()
            .enumerate()
            .filter_map(|(index, identity)| marked.contains(identity).then_some(index))
            .collect::<BTreeSet<_>>();
        self.state.set_viewport_len(previous.state.viewport_len());
        self.select_idx_opt(selected_index, scrolloff);
        *self.marked_mut() = marked_indices;
    }

    pub fn has_sort(&self) -> bool {
        self.view_sort != SongSort::default()
    }

    pub fn sort_songs(&mut self, sort: SongSort, scrolloff: usize) {
        if self.view_sort == sort {
            return;
        }
        let selected = self.state.get_selected();
        let has_viewport = self.state.viewport_len().is_some_and(|height| height > 0);
        let marked = self.marked().clone();
        let original = if self.original_order.is_empty() {
            (0..self.items.len()).collect()
        } else {
            std::mem::take(&mut self.original_order)
        };
        let options = sort.options();
        let mut indexed = std::mem::take(&mut self.items)
            .into_iter()
            .zip(original)
            .enumerate()
            .collect::<Vec<_>>();
        indexed.sort_by(|(_, (a, original_a)), (_, (b, original_b))| {
            let order = match sort.by {
                SongSortKey::Original => {
                    let order = original_a.cmp(original_b);
                    if sort.reverse { order.reverse() } else { order }
                }
                SongSortKey::Recent => {
                    match (
                        a.as_song().and_then(|song| song.added),
                        b.as_song().and_then(|song| song.added),
                    ) {
                        (Some(a), Some(b)) => {
                            if sort.reverse {
                                b.cmp(&a)
                            } else {
                                a.cmp(&b)
                            }
                        }
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    }
                }
                _ => {
                    let order = match (a.as_song(), b.as_song()) {
                        (Some(a), Some(b)) => a
                            .with_custom_sort(&options)
                            .cmp(&b.with_custom_sort(&options)),
                        _ => crate::shared::cmp::StringCompare::from(&options)
                            .compare(a.as_path(), b.as_path()),
                    };
                    if sort.reverse { order.reverse() } else { order }
                }
            };
            order.then_with(|| original_a.cmp(original_b))
        });
        self.unmark_all();
        let mut new_selected = None;
        for (index, (previous, (item, original))) in indexed.into_iter().enumerate() {
            if Some(previous) == selected {
                new_selected = Some(index);
            }
            if marked.contains(&previous) {
                self.state.mark(index);
            }
            self.items.push(item);
            self.original_order.push(original);
        }
        if has_viewport {
            self.select_idx_opt(new_selected, scrolloff);
        } else {
            self.select_idx_opt((!self.items.is_empty()).then_some(0), scrolloff);
            self.state.set_offset(0);
        }
        self.view_sort = sort;
        if sort == SongSort::default() {
            self.original_order.clear();
        }
    }

    pub fn set_filter_active(&mut self, active: bool) {
        self.filter_active = active;
    }

    pub fn filter(&self, ctx: &Ctx) -> String {
        ctx.input.value(self.filter_buffer_id)
    }

    pub fn filter_text<'a>(&self, available_width: u16, ctx: &Ctx) -> Option<Vec<Span<'a>>> {
        self.filter_active.then(|| {
            ctx.input.as_spans_prefixed(
                self.filter_buffer_id,
                FILTER_PREFIX,
                available_width,
                ctx.config.as_border_style(),
                ctx.input.is_active(self.filter_buffer_id),
            )
        })
    }

    pub fn recalculate_matched_items(&mut self, song_format: &[Property<SongProperty>], ctx: &Ctx) {
        let filter = ctx.input.value(self.filter_buffer_id);
        self.matched_item_count = if self.filter_active {
            self.items
                .iter()
                .filter(|item| item.matches(song_format, ctx, &filter))
                .count()
        } else {
            0
        };
    }

    pub fn to_list_items_range<'a>(
        &self,
        range: impl RangeBounds<usize>,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) -> Vec<ListItem<'a>> {
        let current_item_idx = self.selected_with_idx().map(|(idx, _)| idx);

        let start = match range.start_bound() {
            Bound::Included(&start) => start,
            Bound::Excluded(start) => start + 1,
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(end) => end + 1,
            Bound::Excluded(&end) => end,
            Bound::Unbounded => self.items.len(),
        };

        let filter = ctx.input.value(self.filter_buffer_id);
        let mut already_matched = if self.filter_active {
            self.items
                .iter()
                .take(start)
                .filter(|item| item.matches(song_format, ctx, &filter))
                .count() as u32
        } else {
            0
        };
        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(i, item)| {
                let matches = if self.filter_active {
                    item.matches(song_format, ctx, &filter)
                } else {
                    false
                };
                let is_current = current_item_idx.is_some_and(|idx| i == idx);
                if matches {
                    already_matched = already_matched.saturating_add(1);
                }
                let content = if matches && is_current {
                    Some(format!(" [{already_matched}/{}]", self.matched_item_count))
                } else {
                    None
                };
                item.to_list_item(ctx, self.marked().contains(&i), matches, content)
            })
            .collect()
    }

    pub fn to_list_items<'a>(
        &self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) -> Vec<ListItem<'a>> {
        self.to_list_items_range(.., song_format, ctx)
    }

    pub fn selected(&self) -> Option<&T> {
        if let Some(sel) = self.state.get_selected() {
            self.items.get(sel)
        } else {
            None
        }
    }

    pub fn selected_mut(&mut self) -> Option<&mut T> {
        if let Some(sel) = self.state.get_selected() {
            self.items.get_mut(sel)
        } else {
            None
        }
    }

    pub fn selected_with_idx(&self) -> Option<(usize, &T)> {
        if let Some(sel) = self.state.get_selected() {
            self.items.get(sel).map(|v| (sel, v))
        } else {
            None
        }
    }

    pub fn marked_items(&self) -> impl Iterator<Item = &T> {
        self.state
            .marked
            .iter()
            .filter_map(|idx| self.items.get(*idx))
    }

    pub fn marked(&self) -> &BTreeSet<usize> {
        &self.state.marked
    }

    pub fn marked_mut(&mut self) -> &mut BTreeSet<usize> {
        &mut self.state.marked
    }

    pub fn unmark_all(&mut self) {
        self.state.unmark_all();
    }

    pub fn invert_marked(&mut self) {
        self.state.invert_marked();
    }

    pub fn toggle_mark_selected(&mut self) -> bool {
        if let Some(sel) = self.state.get_selected() {
            self.state.toggle_mark(sel)
        } else {
            false
        }
    }

    pub fn mark_selected(&mut self) -> bool {
        if let Some(sel) = self.state.get_selected() {
            self.state.mark(sel)
        } else {
            false
        }
    }

    pub fn unmark_selected(&mut self) -> bool {
        if let Some(sel) = self.state.get_selected() {
            self.state.unmark(sel)
        } else {
            false
        }
    }

    pub fn remove(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
            if !self.original_order.is_empty() {
                let removed = self.original_order.remove(idx);
                for original in &mut self.original_order {
                    if *original > removed {
                        *original -= 1;
                    }
                }
            }
        }
        self.state.remove(idx);
    }

    pub fn remove_all_marked(&mut self) {
        let marked = self.marked().iter().copied().rev().collect::<Vec<_>>();
        for index in marked {
            self.remove(index);
        }
    }

    pub fn next(&mut self, scrolloff: usize, wrap: bool) {
        self.state.next(scrolloff, wrap);
    }

    pub fn prev(&mut self, scrolloff: usize, wrap: bool) {
        self.state.prev(scrolloff, wrap);
    }

    pub fn select_idx_opt(&mut self, idx: Option<usize>, scrolloff: usize) {
        self.state.select(idx, scrolloff);
    }

    pub fn select_idx(&mut self, idx: usize, scrolloff: usize) {
        self.state.select(Some(idx), scrolloff);
    }

    pub fn next_half_viewport(&mut self, scrolloff: usize) {
        self.state.next_half_viewport(scrolloff);
    }

    pub fn prev_half_viewport(&mut self, scrolloff: usize) {
        self.state.prev_half_viewport(scrolloff);
    }

    pub fn next_viewport(&mut self, scrolloff: usize) {
        self.state.next_viewport(scrolloff);
    }

    pub fn prev_viewport(&mut self, scrolloff: usize) {
        self.state.prev_viewport(scrolloff);
    }

    pub fn scroll_to(&mut self, perc: f64, scrolloff: usize) {
        self.state.scroll_to(perc, scrolloff);
    }

    pub fn scroll_down(&mut self, amount: usize, scrolloff: usize) {
        self.state.scroll_down(amount, scrolloff);
    }

    pub fn scroll_up(&mut self, amount: usize, scrolloff: usize) {
        self.state.scroll_up(amount, scrolloff);
    }

    pub fn last(&mut self) {
        self.state.last();
    }

    pub fn first(&mut self) {
        self.state.first();
    }

    pub fn jump_next_matching(&mut self, song_format: &[Property<SongProperty>], ctx: &Ctx) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }

        let Some(selected) = self.state.get_selected() else {
            error!(state:? = self.state; "No song selected");
            return;
        };

        let length = self.items.len();
        let filter = ctx.input.value(self.filter_buffer_id);
        for i in selected + 1..length + selected {
            let i = i % length;
            if self.items[i].matches(song_format, ctx, &filter) {
                self.state.select(Some(i), ctx.config.scrolloff);
                break;
            }
        }
    }

    pub fn jump_previous_matching(&mut self, song_format: &[Property<SongProperty>], ctx: &Ctx) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }
        let Some(selected) = self.state.get_selected() else {
            error!(state:? = self.state; "No song selected");
            return;
        };

        let length = self.items.len();
        let filter = ctx.input.value(self.filter_buffer_id);
        for i in (0..length).rev() {
            let i = (i + selected) % length;
            if self.items[i].matches(song_format, ctx, &filter) {
                self.state.select(Some(i), ctx.config.scrolloff);
                break;
            }
        }
    }

    pub fn jump_first_matching(&mut self, song_format: &[Property<SongProperty>], ctx: &Ctx) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }

        let filter = ctx.input.value(self.filter_buffer_id);
        self.items
            .iter()
            .enumerate()
            .find(|(_, item)| item.matches(song_format, ctx, &filter))
            .inspect(|(idx, _)| self.state.select(Some(*idx), ctx.config.scrolloff));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {

    use ratatui::widgets::ListState;
    use rstest::rstest;

    use super::{Dir, DirState};
    use crate::{ctx::Ctx, tests::fixtures::ctx, ui::input::BufferId};

    mod playlist_sorting {
        use super::*;
        use crate::{
            config::sort_mode::{SongSort, SongSortKey},
            mpd::commands::Song,
        };
        use std::collections::BTreeSet;

        fn playlist() -> Dir<Song, ListState> {
            let rows = [
                ("Zulu", "C", "B", "same", Some(1), 100),
                ("Alpha", "B", "C", "second", Some(3), 300),
                ("Zulu", "A", "A", "same", Some(2), 200),
                ("Unknown", "D", "D", "unknown", None, 50),
            ];
            let songs = rows
                .into_iter()
                .enumerate()
                .map(
                    |(index, (title, artist, album, uri, added, duration))| Song {
                        id: u32::try_from(index).expect("fixture id"),
                        file: uri.into(),
                        added: added
                            .and_then(|day| chrono::DateTime::from_timestamp(day * 86_400, 0)),
                        duration: Some(std::time::Duration::from_secs(duration)),
                        metadata: std::collections::HashMap::from([
                            ("title".into(), title.into()),
                            ("artist".into(), artist.into()),
                            ("album".into(), album.into()),
                            ("date".into(), "2099".into()),
                        ]),
                        ..Default::default()
                    },
                )
                .collect();
            let mut dir = Dir::new(songs);
            dir.state.set_content_and_viewport_len(4, 4);
            dir
        }

        fn ids(dir: &Dir<Song, ListState>) -> Vec<u32> {
            dir.items.iter().map(|song| song.id).collect()
        }

        #[test]
        fn restoring_sort_before_first_render_starts_at_the_top() {
            let mut dir = Dir::new(playlist().items);
            dir.sort_songs(SongSort::new(SongSortKey::Recent), 2);
            assert_eq!(ids(&dir), [1, 2, 0, 3]);
            assert_eq!(dir.state.get_selected(), Some(0));
            assert_eq!(dir.state.offset(), 0);
        }

        #[test]
        fn recently_added_keeps_missing_dates_last_in_both_directions() {
            let mut dir = playlist();
            let mut sort = SongSort::new(SongSortKey::Recent);
            dir.sort_songs(sort, 0);
            assert_eq!(ids(&dir), [1, 2, 0, 3]);
            sort.reverse = false;
            dir.sort_songs(sort, 0);
            assert_eq!(ids(&dir), [0, 2, 1, 3]);
        }

        #[test]
        fn changing_sort_preserves_marked_and_selected_occurrences_and_original_positions() {
            let mut dir = playlist();
            dir.select_idx(2, 0);
            dir.state.mark(0);
            dir.state.mark(2);
            dir.sort_songs(SongSort::new(SongSortKey::Recent), 0);
            assert_eq!(dir.selected().expect("selected duplicate").id, 2);
            assert_eq!(dir.marked(), &BTreeSet::from([1, 2]));
            for (index, song) in dir.items.iter().enumerate() {
                assert_eq!(
                    dir.source_index(index),
                    usize::try_from(song.id).expect("original position")
                );
            }
            dir.sort_songs(SongSort::default(), 0);
            assert_eq!(ids(&dir), [0, 1, 2, 3]);
            assert_eq!(dir.selected().expect("selected duplicate").id, 2);
            assert_eq!(dir.marked(), &BTreeSet::from([0, 2]));
        }

        #[test]
        fn columns_sort_by_their_metadata_and_restore_the_playlist_order() {
            let mut dir = playlist();
            for (by, expected) in [
                (SongSortKey::Title, [1, 3, 2, 0]),
                (SongSortKey::Artist, [2, 1, 0, 3]),
                (SongSortKey::Album, [2, 0, 1, 3]),
                (SongSortKey::Duration, [3, 0, 2, 1]),
            ] {
                dir.sort_songs(SongSort::new(by), 0);
                assert_eq!(ids(&dir), expected, "{by}");
                dir.sort_songs(SongSort::default(), 0);
                assert_eq!(ids(&dir), [0, 1, 2, 3]);
            }
        }

        #[test]
        fn removing_sorted_marked_rows_keeps_the_remaining_backend_positions_valid() {
            let mut dir = playlist();
            dir.state.mark(0);
            dir.state.mark(2);
            dir.sort_songs(SongSort::new(SongSortKey::Recent), 0);
            dir.remove_all_marked();
            assert_eq!(ids(&dir), [1, 3]);
            assert_eq!([dir.source_index(0), dir.source_index(1)], [0, 1]);
            dir.sort_songs(SongSort::default(), 0);
            assert_eq!(ids(&dir), [1, 3]);
        }

        #[test]
        fn refreshing_a_recently_sorted_playlist_keeps_marks_on_the_same_occurrences() {
            let mut previous = playlist();
            previous.select_idx(2, 0);
            previous.state.mark(0);
            previous.state.mark(2);
            let sort = SongSort::new(SongSortKey::Recent);
            previous.sort_songs(sort, 0);
            let mut refreshed = playlist();
            refreshed.items.push(Song {
                id: 4,
                file: "new".into(),
                added: chrono::DateTime::from_timestamp(4 * 86_400, 0),
                ..Default::default()
            });
            refreshed.state.set_content_len(Some(5));
            refreshed.sort_songs(sort, 0);
            refreshed.restore_selection(&previous, 0);
            assert_eq!(ids(&refreshed), [4, 1, 2, 0, 3]);
            assert_eq!(refreshed.selected().expect("selected occurrence").id, 2);
            let marked = refreshed
                .marked()
                .iter()
                .map(|index| refreshed.items[*index].id)
                .collect::<Vec<_>>();
            assert_eq!(marked, [2, 0]);
        }
    }

    fn create_subject() -> Dir<String, ListState> {
        let mut res = Dir {
            loaded: true,
            items: vec!["a", "b", "c", "d", "f"]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
            state: DirState::default(),
            matched_item_count: 0,
            filter_buffer_id: BufferId::new(),
            filter_active: false,
            ..Default::default()
        };
        res.state
            .set_content_and_viewport_len(res.items.len(), res.items.len());
        res
    }

    mod selected {
        use super::create_subject;

        #[test]
        fn returns_none() {
            let mut subject = create_subject();
            subject.state.select(None, 0);

            let result = subject.selected();

            assert_eq!(result, None);
        }

        #[test]
        fn returns_item() {
            let mut subject = create_subject();
            subject.state.select(Some(2), 0);

            let result = subject.selected();

            assert_eq!(result.unwrap(), "c");
        }
    }

    mod selected_with_idx {
        use super::create_subject;

        #[test]
        fn returns_none() {
            let mut subject = create_subject();
            subject.state.select(None, 0);

            let result = subject.selected_with_idx();

            assert_eq!(result, None);
        }

        #[test]
        fn returns_item() {
            let mut subject = create_subject();
            subject.state.select(Some(2), 0);

            let result = subject.selected_with_idx();

            assert_eq!(result.unwrap(), (2, &"c".to_owned()));
        }
    }

    mod toggle_mark_selected {
        use std::collections::BTreeSet;

        use super::create_subject;

        #[test]
        fn toggles_marks() {
            let mut subject = create_subject();
            subject.state.mark(2);
            subject.state.mark(1);
            subject.state.unmark(3);

            subject.state.select(Some(2), 0);
            subject.toggle_mark_selected();
            subject.state.select(Some(3), 0);
            subject.toggle_mark_selected();

            assert_eq!(subject.marked(), &BTreeSet::from([1, 3]));
        }
    }

    mod mark_selected {
        use std::collections::BTreeSet;

        use super::create_subject;

        #[test]
        fn does_nothing_when_none_selected() {
            let mut subject = create_subject();

            subject.mark_selected();

            assert_eq!(subject.marked(), &BTreeSet::from([]));
        }

        #[test]
        fn marks_selected() {
            let mut subject = create_subject();
            subject.state.mark(2);
            subject.state.select(Some(3), 0);

            subject.mark_selected();

            assert_eq!(subject.marked(), &BTreeSet::from([2, 3]));
        }
    }

    mod unmark_selected {
        use std::collections::BTreeSet;

        use super::create_subject;

        #[test]
        fn does_nothing_when_none_selected() {
            let mut subject = create_subject();
            subject.state.mark(3);

            subject.unmark_selected();

            assert_eq!(subject.marked(), &BTreeSet::from([3]));
        }

        #[test]
        fn unmarks_selected() {
            let mut subject = create_subject();
            subject.state.mark(2);
            subject.state.mark(3);
            subject.state.select(Some(2), 0);

            subject.unmark_selected();

            assert_eq!(subject.marked(), &BTreeSet::from([3]));
        }
    }

    mod remove {
        use std::collections::BTreeSet;

        use crate::ui::dirstack::dir::tests::create_subject;

        #[test]
        fn does_nothing_when_outside_range() {
            let mut subject = create_subject();
            subject.state.mark(2);
            subject.state.mark(3);

            subject.remove(5);

            assert_eq!(subject.marked(), &BTreeSet::from([2, 3]));
        }

        #[test]
        fn removes_item() {
            let mut subject = create_subject();
            subject.state.mark(2);
            subject.state.mark(4);

            subject.remove(2);

            assert_eq!(subject.marked(), &BTreeSet::from([3]));
        }
    }

    mod jump_next_matching {
        use super::*;

        #[rstest]
        fn jumps_by_half_viewport(ctx: Ctx) {
            let mut val: Dir<String, ListState> = Dir {
                items: vec!["aa", "ab", "c", "ad"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
                ..Default::default()
            };
            val.state.set_content_and_viewport_len(val.items.len(), 2);
            val.state.select(Some(0), 0);

            val.set_filter_active(true);
            ctx.input.set_buffer("a".to_string(), val.filter_buffer_id);

            val.jump_next_matching(&[], &ctx);
            assert_eq!(val.state.get_selected(), Some(1));

            val.jump_next_matching(&[], &ctx);
            assert_eq!(val.state.get_selected(), Some(3));
        }
    }

    mod jump_previous_matching {
        use super::*;

        #[rstest]
        fn jumps_by_half_viewport(ctx: Ctx) {
            let mut val: Dir<String, ListState> = Dir {
                items: vec!["aa", "ab", "c", "ad", "padding"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
                ..Default::default()
            };
            val.state.set_content_and_viewport_len(val.items.len(), 2);
            val.state.select(Some(4), 0);

            val.set_filter_active(true);
            ctx.input.set_buffer("a".to_string(), val.filter_buffer_id);

            val.jump_previous_matching(&[], &ctx);
            assert_eq!(val.state.get_selected(), Some(3));

            val.jump_previous_matching(&[], &ctx);
            assert_eq!(val.state.get_selected(), Some(1));
        }
    }
}
