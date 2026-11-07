use crate::model::TrackInfo;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug)]
pub struct MissingQueueId(pub u32);
impl std::fmt::Display for MissingQueueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no such queue ID: {}", self.0)
    }
}
impl std::error::Error for MissingQueueId {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: u32,
    pub track: TrackInfo,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Queue {
    pub entries: Vec<Entry>,
    pub next_id: u32,
    pub version: u32,
    pub current: Option<u32>,
    #[serde(default)]
    pub consume: bool,
    #[serde(default)]
    pub consume_oneshot: bool,
}
impl Queue {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let mut queue: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        let mut ids = std::collections::HashSet::new();
        ensure!(
            queue.entries.iter().all(|e| ids.insert(e.id)),
            "duplicate IDs in persisted queue"
        );
        queue.next_id = queue.next_id.max(ids.iter().copied().max().unwrap_or(0));
        ensure!(
            queue.current.is_none_or(|id| ids.contains(&id)),
            "current queue ID is missing"
        );
        Ok(queue)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::paths::write_private_file_atomic(path, &serde_json::to_vec(self)?)
    }
    pub fn changed(&mut self) {
        self.version = self.version.wrapping_add(1) & 0x7fff_ffff;
    }
    pub fn index(&self, id: u32) -> Result<usize> {
        self.entries
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| anyhow::Error::new(MissingQueueId(id)))
    }
    pub fn cursor(&self) -> Option<usize> {
        self.current.and_then(|id| self.index(id).ok())
    }
    pub fn consume_finished(&mut self, id: Option<u32>) -> bool {
        if !self.consume {
            return false;
        }
        let Some(id) = id else {
            return false;
        };
        let old_len = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.current == Some(id) {
            self.current = None;
        }
        if self.consume_oneshot {
            self.consume = false;
            self.consume_oneshot = false;
        }
        if old_len != self.entries.len() {
            self.changed();
            return true;
        }
        false
    }
    pub fn position(&self, text: &str) -> Result<usize> {
        let current = self.cursor().unwrap_or(0);
        let value = if let Some(n) = text.strip_prefix('+') {
            current.saturating_add(1).saturating_add(n.parse()?)
        } else if let Some(n) = text.strip_prefix('-') {
            current.saturating_sub(n.parse()?)
        } else {
            text.parse()?
        };
        Ok(value.min(self.entries.len()))
    }
    pub fn add(&mut self, tracks: Vec<TrackInfo>, position: usize) -> Result<Vec<u32>> {
        ensure!(
            position <= self.entries.len(),
            "queue position out of range"
        );
        ensure!(
            tracks.iter().all(|track| track.uri.is_some()),
            "track has no playable URI"
        );
        let count = u32::try_from(tracks.len()).context("queue ID space exhausted")?;
        let next_id = self
            .next_id
            .checked_add(count)
            .context("queue ID space exhausted")?;
        let ids: Vec<_> = (self.next_id..next_id).map(|id| id + 1).collect();
        let inserted = tracks
            .into_iter()
            .zip(ids.iter().copied())
            .map(|(track, id)| Entry { id, track });
        self.entries.splice(position..position, inserted);
        self.next_id = next_id;
        self.changed();
        Ok(ids)
    }
    pub fn move_range(&mut self, start: usize, end: usize, destination: usize) -> Result<()> {
        ensure!(
            start < end && end <= self.entries.len(),
            "queue range out of bounds"
        );
        let moving: Vec<_> = self.entries.drain(start..end).collect();
        let position = destination.min(self.entries.len());
        self.entries.splice(position..position, moving);
        self.changed();
        Ok(())
    }
    pub fn delete(&mut self, start: usize, end: usize) -> Result<()> {
        ensure!(
            start < end && end <= self.entries.len(),
            "queue range out of bounds"
        );
        self.entries.drain(start..end);
        self.changed();
        if self.current.is_some_and(|id| self.index(id).is_err()) {
            self.current = self
                .entries
                .get(start.min(self.entries.len().saturating_sub(1)))
                .map(|e| e.id);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn track() -> TrackInfo {
        super::super::catalog::demo_tracks().remove(0)
    }
    #[test]
    fn rejected_batches_do_not_change_the_queue_or_consume_ids() {
        let mut queue = Queue::default();
        queue.add(vec![track()], 0).unwrap();
        let before = serde_json::to_vec(&queue).unwrap();
        let mut invalid = track();
        invalid.uri = None;
        assert!(queue.add(vec![track(), invalid], 1).is_err());
        assert_eq!(serde_json::to_vec(&queue).unwrap(), before);

        queue.next_id = u32::MAX - 1;
        let before = serde_json::to_vec(&queue).unwrap();
        assert!(queue.add(vec![track(), track()], 1).is_err());
        assert_eq!(serde_json::to_vec(&queue).unwrap(), before);
    }
    #[test]
    fn duplicate_tracks_keep_distinct_stable_ids_after_moves() {
        let mut q = Queue::default();
        let ids = q.add(vec![track(), track(), track()], 0).unwrap();
        q.current = Some(ids[1]);
        q.move_range(1, 2, 0).unwrap();
        assert_eq!(q.cursor(), Some(0));
        q.delete(1, 2).unwrap();
        assert_eq!(
            q.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![ids[1], ids[2]]
        );
        assert!(q.next_id > 0);
    }
    #[test]
    fn queue_roundtrips_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.add(vec![track()], 0).unwrap();
        q.save(&path).unwrap();
        assert_eq!(Queue::load(&path).unwrap().entries[0].id, q.entries[0].id);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
