use itertools::Itertools;
use serde::{Deserialize, Serialize};

use super::{defaults, theme::properties::SongProperty};
use crate::shared::cmp::StringCompare;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, strum::Display)]
pub enum SongSortKey {
    #[default]
    #[strum(to_string = "original order")]
    Original,
    #[strum(to_string = "date added")]
    Recent,
    #[strum(to_string = "title")]
    Title,
    #[strum(to_string = "artist")]
    Artist,
    #[strum(to_string = "album")]
    Album,
    #[strum(to_string = "duration")]
    Duration,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SongSort {
    pub by: SongSortKey,
    pub reverse: bool,
}

impl SongSort {
    pub fn new(by: SongSortKey) -> Self {
        Self {
            by,
            reverse: by == SongSortKey::Recent,
        }
    }

    pub fn options(self) -> SortOptions {
        use SongProperty::{Album, Artist, Disc, Duration, Title, Track};
        let properties = match self.by {
            SongSortKey::Original | SongSortKey::Recent => Vec::new(),
            SongSortKey::Title => vec![Title, Artist, Album, Track],
            SongSortKey::Artist => vec![Artist, Album, Disc, Track, Title],
            SongSortKey::Album => vec![Album, Disc, Track, Title],
            SongSortKey::Duration => vec![Duration, Title],
        };
        SortOptions {
            mode: SortMode::Format(properties),
            fold_case: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SortMode {
    Format(Vec<SongProperty>),
    ModifiedTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Bools represent different flags"
)]
pub struct SortOptions {
    pub mode: SortMode,
    pub group_by_type: bool,
    pub reverse: bool,
    pub ignore_leading_the: bool,
    pub fold_case: bool,
}

impl Default for SortOptions {
    fn default() -> Self {
        Self {
            mode: SortMode::default(),
            group_by_type: true,
            reverse: false,
            ignore_leading_the: false,
            fold_case: false,
        }
    }
}

impl Default for SortMode {
    fn default() -> Self {
        Self::Format(
            defaults::default_song_sort()
                .into_iter()
                .map(SongProperty::from)
                .collect_vec(),
        )
    }
}

impl From<&SortOptions> for StringCompare {
    fn from(value: &SortOptions) -> Self {
        StringCompare::builder()
            .fold_case(value.fold_case)
            .ignore_leading_the(value.ignore_leading_the)
            .build()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub enum SortModeFile {
    Format {
        #[serde(default = "defaults::bool::<true>")]
        group_by_type: bool,
        #[serde(default = "defaults::bool::<false>")]
        reverse: bool,
    },
    SortFormat {
        #[serde(default = "defaults::bool::<true>")]
        group_by_type: bool,
        #[serde(default = "defaults::bool::<false>")]
        reverse: bool,
    },
    ModifiedTime {
        #[serde(default = "defaults::bool::<true>")]
        group_by_type: bool,
        #[serde(default = "defaults::bool::<false>")]
        reverse: bool,
    },
}

impl Default for SortModeFile {
    fn default() -> Self {
        Self::SortFormat {
            group_by_type: true,
            reverse: false,
        }
    }
}
