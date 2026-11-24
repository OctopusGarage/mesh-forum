//! Durable, author-signed withdrawals for public posts.

use super::{Forum, PostHint, PostId, codec, filename};
use crate::forum::record::{Event, EventKind};
use bincode::Options;
use mesh_identity::device::DeviceIdentity;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

const MARKER: &[u8] = b"mesh-forum/withdraw/v1";
pub(super) const MAX_WITHDRAWAL_BYTES: u64 = 4608;

#[derive(Clone, Serialize, Deserialize)]
pub struct Withdrawal {
    original: Event,
    signed: Event,
}

impl Withdrawal {
    pub fn post_id(&self) -> PostId {
        *self.original.id.as_bytes()
    }

    pub fn board(&self) -> [u8; 32] {
        *self.original.conversation_id.as_bytes()
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        if self.original.kind != EventKind::Post {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "withdrawal must target the original post",
            ));
        }
        PostHint::verify(self.original.clone())?;
        let signed = &self.signed;
        if signed.kind != EventKind::Withdrawal
            || signed.author.ed25519_pub() != self.original.author.ed25519_pub()
            || signed.conversation_id != self.original.conversation_id
            || signed.parents.as_slice() != [self.original.id]
            || signed.ciphertext != MARKER
            || !signed.verify_integrity()
            || !signed.verify_signature()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid withdrawal",
            ));
        }
        Ok(())
    }
}

impl Forum {
    pub fn withdraw(
        &mut self,
        author: &DeviceIdentity,
        id: PostId,
        now: u64,
    ) -> io::Result<Withdrawal> {
        let (post, _, _) = self
            .posts
            .get(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "post not found"))?;
        let original = post.hint().original_event().clone();
        let signed = Event::new(
            author,
            original.conversation_id,
            1,
            vec![original.id],
            now,
            0,
            EventKind::Withdrawal,
            MARKER.to_vec(),
        );
        let withdrawal = Withdrawal { original, signed };
        self.apply_withdrawal(withdrawal.clone())?;
        Ok(withdrawal)
    }

    pub fn apply_withdrawal(&mut self, withdrawal: Withdrawal) -> io::Result<()> {
        withdrawal.verify()?;
        let id = withdrawal.post_id();
        if !self.withdrawals.contains_key(&id) {
            let bytes = codec()
                .with_limit(MAX_WITHDRAWAL_BYTES)
                .serialize(&withdrawal)
                .map_err(io::Error::other)?;
            self.persist(&withdrawal_filename(id), &bytes)?;
            self.withdrawals.insert(id, withdrawal);
        }
        self.remove_thread(id)
    }

    pub fn is_withdrawn(&self, id: PostId) -> bool {
        self.withdrawals.contains_key(&id)
    }

    pub(super) fn remove_thread(&mut self, id: PostId) -> io::Result<()> {
        let revisions: Vec<_> = self
            .posts
            .values()
            .filter_map(|(post, _, _)| (post.hint().thread_id() == id).then_some(post.id))
            .collect();
        for revision in &revisions {
            self.posts.remove(revision);
        }
        let mut result = self.remove_withdrawn_body(id);
        for revision in revisions.into_iter().filter(|revision| *revision != id) {
            if let Err(error) = self.remove_withdrawn_body(revision) {
                result = Err(error);
            }
        }
        result.and(self.remove_attachment(id))
    }

    pub(super) fn remove_withdrawn_body(&self, id: PostId) -> io::Result<()> {
        match fs::remove_file(self.directory.join(filename(id))) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

pub(super) fn read_withdrawal(path: &Path) -> io::Result<Withdrawal> {
    if fs::metadata(path)?.len() > MAX_WITHDRAWAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized withdrawal",
        ));
    }
    let withdrawal: Withdrawal = codec()
        .with_limit(MAX_WITHDRAWAL_BYTES)
        .deserialize(&fs::read(path)?)
        .map_err(io::Error::other)?;
    withdrawal.verify()?;
    if path.file_name().and_then(|name| name.to_str())
        != Some(withdrawal_filename(withdrawal.post_id()).as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "withdrawal ID mismatch",
        ));
    }
    Ok(withdrawal)
}

fn withdrawal_filename(id: PostId) -> String {
    Path::new(&filename(id))
        .with_extension("withdrawal")
        .to_string_lossy()
        .into_owned()
}
