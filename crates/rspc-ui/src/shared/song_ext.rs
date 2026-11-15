use std::ops::Range;

use crate::mpd::commands::Song;

pub trait SongsExt {
    fn to_album_ranges(self) -> impl Iterator<Item = Range<usize>>;
}

impl SongsExt for &[Song] {
    fn to_album_ranges(self) -> impl Iterator<Item = Range<usize>> {
        let mut i = 0;
        std::iter::from_fn(move || {
            if i >= self.len() {
                return None;
            }
            let mut j = i + 1;
            while j < self.len() && self[i].same_album(&self[j]) {
                j += 1;
            }
            let range = i..j;
            i = j;
            Some(range)
        })
    }
}
