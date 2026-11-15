use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::config::sort_mode::SongSort;

type SortEntries = BTreeMap<String, BTreeMap<String, SongSort>>;

#[derive(Debug, Default)]
pub struct CollectionSorts {
    backend: String,
    file: Option<PathBuf>,
    entries: SortEntries,
}

impl CollectionSorts {
    pub fn load(backend: String) -> Self {
        match super::paths::state_dir() {
            Ok(dir) => Self::from_file(backend, dir.join("collection-sorts.json")),
            Err(error) => {
                log::warn!(error:?; "Song sort preferences will be kept for this session only");
                Self {
                    backend,
                    ..Self::default()
                }
            }
        }
    }

    fn from_file(backend: String, file: PathBuf) -> Self {
        let entries = Self::read(&file).unwrap_or_else(|error| {
            log::warn!(error:?; "Cannot read song sort preferences");
            SortEntries::new()
        });
        Self {
            backend,
            file: Some(file),
            entries,
        }
    }

    fn read(file: &Path) -> Result<SortEntries> {
        match std::fs::read(file) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SortEntries::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn get(&self, context: &str) -> SongSort {
        self.entries
            .get(&self.backend)
            .and_then(|entries| entries.get(context))
            .copied()
            .unwrap_or_default()
    }

    pub fn set(&mut self, context: &str, sort: SongSort) -> Result<()> {
        // Keep the lock across the read, update and atomic replacement so two
        // running UIs do not overwrite each other's collection preferences.
        let _lock = if let Some(file) = &self.file {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(file.with_extension("lock"))?;
            rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
            self.entries = Self::read(file)?;
            Some(lock)
        } else {
            None
        };
        let entries = self.entries.entry(self.backend.clone()).or_default();
        if sort == SongSort::default() {
            entries.remove(context);
        } else {
            entries.insert(context.to_owned(), sort);
        }
        if entries.is_empty() {
            self.entries.remove(&self.backend);
        }
        let Some(file) = &self.file else {
            return Ok(());
        };
        let temporary = file.with_extension(format!("{}.tmp", rand::random::<u64>()));
        let result = (|| -> Result<()> {
            let mut writer = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut writer, &self.entries)?;
            writer.flush()?;
            drop(writer);
            std::fs::rename(&temporary, file)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("Could not save song sort preferences")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sort_mode::SongSortKey;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn playlists_keep_independent_sorting_across_restarts() {
        let dir = tempfile::tempdir().expect("state directory");
        let file = dir.path().join("collection-sorts.json");
        let mut sorts = CollectionSorts::from_file("server".into(), file.clone());
        let title = SongSort::new(SongSortKey::Title);
        let recent = SongSort::new(SongSortKey::Recent);
        sorts.set("First", title).expect("save first");
        sorts.set("Second", recent).expect("save second");
        let mut restarted = CollectionSorts::from_file("server".into(), file.clone());
        assert_eq!(restarted.get("First"), title);
        assert_eq!(restarted.get("Second"), recent);
        restarted
            .set("First", SongSort::default())
            .expect("restore original order");
        let restarted = CollectionSorts::from_file("server".into(), file.clone());
        assert_eq!(restarted.get("First"), SongSort::default());
        assert_eq!(restarted.get("Second"), recent);
        assert_eq!(
            file.metadata()
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("state files").count(),
            2
        );
    }

    #[test]
    fn identically_named_playlists_on_different_servers_keep_their_own_preferences() {
        let dir = tempfile::tempdir().expect("state directory");
        let file = dir.path().join("collection-sorts.json");
        let title = SongSort::new(SongSortKey::Title);
        let recent = SongSort::new(SongSortKey::Recent);
        CollectionSorts::from_file("first-server".into(), file.clone())
            .set("Mix", title)
            .expect("first server");
        CollectionSorts::from_file("second-server".into(), file.clone())
            .set("Mix", recent)
            .expect("second server");
        assert_eq!(
            CollectionSorts::from_file("first-server".into(), file.clone()).get("Mix"),
            title
        );
        assert_eq!(
            CollectionSorts::from_file("second-server".into(), file).get("Mix"),
            recent
        );
    }
    #[test]
    fn overlapping_ui_sessions_do_not_erase_each_others_sort_choices() {
        for (first_server, second_server) in [("server", "server"), ("first", "second")] {
            let dir = tempfile::tempdir().expect("state directory");
            let file = dir.path().join("collection-sorts.json");
            let mut first = CollectionSorts::from_file(first_server.into(), file.clone());
            let mut second = CollectionSorts::from_file(second_server.into(), file.clone());
            let title = SongSort::new(SongSortKey::Title);
            let recent = SongSort::new(SongSortKey::Recent);
            first
                .set("playlist:First", title)
                .expect("save first choice");
            second
                .set("library:liked", recent)
                .expect("save second choice");
            assert_eq!(
                CollectionSorts::from_file(first_server.into(), file.clone()).get("playlist:First"),
                title
            );
            assert_eq!(
                CollectionSorts::from_file(second_server.into(), file).get("library:liked"),
                recent
            );
        }
    }

    #[test]
    fn simultaneous_preference_writers_preserve_all_collections() {
        let directory = tempfile::tempdir().expect("state directory");
        let file = directory.path().join("collection-sorts.json");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for index in 0..8 {
                let file = file.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let mut sorts = CollectionSorts::from_file("server".into(), file);
                    barrier.wait();
                    sorts
                        .set(
                            &format!("playlist:{index}"),
                            SongSort::new(SongSortKey::Title),
                        )
                        .expect("save preference");
                });
            }
        });
        let restarted = CollectionSorts::from_file("server".into(), file);
        for index in 0..8 {
            assert_eq!(
                restarted.get(&format!("playlist:{index}")),
                SongSort::new(SongSortKey::Title)
            );
        }
    }
}
