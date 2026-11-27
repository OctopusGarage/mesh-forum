//! Bounded metadata cache for offline discovery. It never authorizes serving.

use super::{PostHint, PostId, codec};
use crate::forum::record::Event;
use bincode::Options;
use std::{cmp::Reverse, collections::BTreeMap, fs, io, path::Path};

pub(super) const NAME: &str = "board-index";
use super::board_index::INDEX_LIMIT as MAX_ENTRIES;
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Default)]
pub(super) struct IndexCache {
    entries: Vec<PostHint>,
}

impl IndexCache {
    pub(super) fn open(root: &Path, board: [u8; 32]) -> io::Result<Self> {
        let path = root.join(NAME);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_BYTES => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid cached board index",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        }
        let records: Vec<Event> = codec()
            .with_limit(MAX_BYTES)
            .deserialize(&fs::read(path)?)
            .map_err(io::Error::other)?;
        if records.len() > MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cached board index exceeds limit",
            ));
        }
        let entries = records
            .into_iter()
            .filter(|event| event.conversation_id.as_bytes() == &board)
            .map(PostHint::verify)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { entries })
    }

    pub(super) fn merge(&mut self, page: &[PostHint], keep: impl Fn(PostId) -> bool) {
        let unique: BTreeMap<_, _> = self
            .entries
            .drain(..)
            .chain(page.iter().cloned())
            .filter(|hint| keep(hint.thread_id()))
            .map(|hint| (hint.id, hint))
            .collect();
        self.entries = unique.into_values().collect();
        self.entries
            .sort_unstable_by_key(|hint| (Reverse(hint.event.lamport), hint.id));
        self.entries.truncate(MAX_ENTRIES);
    }

    pub(super) fn entries(&self) -> &[PostHint] {
        &self.entries
    }

    pub(super) fn same_revisions(&self, other: &Self) -> bool {
        self.entries
            .iter()
            .map(|hint| hint.id)
            .eq(other.entries.iter().map(|hint| hint.id))
    }

    pub(super) fn encode(&self) -> io::Result<Vec<u8>> {
        let records: Vec<_> = self.entries.iter().map(|hint| &hint.event).collect();
        codec()
            .with_limit(MAX_BYTES)
            .serialize(&records)
            .map_err(io::Error::other)
    }
}
