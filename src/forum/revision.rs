//! Resolve the latest unambiguous version using verified index entries.

use super::{PostHint, PostId};
use std::{cmp::Reverse, collections::BTreeMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionStatus {
    Current,
    MissingPrevious,
    Conflict,
}

pub struct RevisionGroup {
    pub head: PostHint,
    pub status: RevisionStatus,
}

pub fn group_revisions<'a>(hints: impl IntoIterator<Item = &'a PostHint>) -> Vec<RevisionGroup> {
    let mut threads: BTreeMap<PostId, BTreeMap<PostId, &PostHint>> = BTreeMap::new();
    for hint in hints {
        threads
            .entry(hint.thread_id())
            .or_default()
            .insert(hint.id, hint);
    }
    let mut groups: Vec<_> = threads
        .into_iter()
        .filter_map(|(id, revisions)| {
            let mut head = match revisions.get(&id) {
                Some(root) => (*root).clone(),
                None => {
                    PostHint::verify(revisions.values().next()?.original_event().clone()).ok()?
                }
            };
            let mut children: BTreeMap<PostId, Vec<&PostHint>> = BTreeMap::new();
            let mut pending = 0;
            for hint in revisions.values() {
                if let Some(parent) = hint.previous_revision() {
                    children.entry(parent).or_default().push(hint);
                    pending += 1;
                }
            }
            let status = loop {
                let Some(next) = children.get(&head.id) else {
                    break if pending == 0 {
                        RevisionStatus::Current
                    } else {
                        RevisionStatus::MissingPrevious
                    };
                };
                if next.len() != 1 {
                    break RevisionStatus::Conflict;
                }
                let next = next[0];
                if head.event.seq.checked_add(1) != Some(next.event.seq)
                    || next.event.lamport < head.event.lamport
                {
                    break RevisionStatus::MissingPrevious;
                }
                head = next.clone();
                pending -= 1;
            };
            Some(RevisionGroup { head, status })
        })
        .collect();
    groups.sort_unstable_by_key(|group| (Reverse(group.head.event.lamport), group.head.id));
    groups
}
