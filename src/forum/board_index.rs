//! Ordered serving projection. Never persists or authorizes a peer.
//! Mutations follow durable post writes; queries visit only the requested window.
use super::{Event, Forum, Post, PostId, Retention};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

pub(super) const PAGE_SIZE: usize = 30;
pub(super) const INDEX_LIMIT: usize = 200;
type Order = (Reverse<u64>, PostId);

#[derive(Default)]
pub(super) struct BoardIndex(BTreeMap<[u8; 32], BTreeSet<Order>>);

impl BoardIndex {
    pub(super) fn update(&mut self, post: &Post, retention: Retention) {
        if retention.offers_content() {
            self.0
                .entry(*post.signed_event().conversation_id.as_bytes())
                .or_default()
                .insert((Reverse(post.signed_event().lamport), post.id));
        } else {
            self.remove(post);
        }
    }

    pub(super) fn remove(&mut self, post: &Post) {
        let board = post.signed_event().conversation_id.as_bytes();
        if let Some(entries) = self.0.get_mut(board) {
            entries.remove(&(Reverse(post.signed_event().lamport), post.id));
            if entries.is_empty() {
                self.0.remove(board);
            }
        }
    }
}

impl Forum {
    pub(super) fn index_page(&self, board: [u8; 32], offset: usize) -> Vec<&Event> {
        self.board_index
            .0
            .get(&board)
            .into_iter()
            .flatten()
            .take(INDEX_LIMIT)
            .skip(offset)
            .take(PAGE_SIZE)
            .filter_map(|(_, id)| self.offer(*id).map(Post::signed_event))
            .collect()
    }
}
