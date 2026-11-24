//! An optional detached file bound to the author's signed original post.

use super::{BodyMetadata, Forum, MAX_BODY, Post, PostId, Retention, codec, filename, validate};
use crate::forum::record::{ConversationId, Event, EventKind};
use bincode::Options;
use mesh_identity::device::DeviceIdentity;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Serialize, Deserialize)]
pub struct Attachment {
    name: String,
    metadata: BodyMetadata,
}

impl Attachment {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn manifest(&self) -> crate::PostBodyManifest<'_> {
        self.metadata.manifest()
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        if self.name.trim().is_empty()
            || self.name.len() > 255
            || matches!(self.name.as_str(), "." | "..")
            || self
                .name
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':'))
            || self.metadata.bytes == 0
            || !self.metadata.valid()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid attachment manifest",
            ));
        }
        Ok(())
    }

    fn verify_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 != self.metadata.bytes
            || BodyMetadata::from_bytes(bytes) != self.metadata
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attachment does not match signed manifest",
            ));
        }
        Ok(())
    }
}

impl Forum {
    #[allow(
        clippy::too_many_arguments,
        reason = "Preserve the public publishing signature"
    )]
    pub fn publish_with_attachment(
        &mut self,
        identity: &DeviceIdentity,
        board: [u8; 32],
        title: &str,
        body: &str,
        now: u64,
        name: &str,
        bytes: &[u8],
    ) -> io::Result<Post> {
        self.publish_attachment_as(identity, board, (title, body), now, (name, bytes), None)
    }

    pub(super) fn publish_attachment_as(
        &mut self,
        identity: &DeviceIdentity,
        board: [u8; 32],
        (title, body): (&str, &str),
        now: u64,
        (name, bytes): (&str, &[u8]),
        author_name: Option<&str>,
    ) -> io::Result<Post> {
        validate(title, body)?;
        if bytes.len() > MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment exceeds size limit",
            ));
        }
        let attachment = Attachment {
            name: name.to_owned(),
            metadata: BodyMetadata::from_bytes(bytes),
        };
        attachment.verify()?;
        let payload = codec()
            .serialize(&(
                *b"FORUM-V3",
                title,
                BodyMetadata::from_body(body),
                attachment,
            ))
            .map_err(io::Error::other)?;
        let payload = super::author_payload(author_name, payload)?;
        let event = Event::new(
            identity,
            ConversationId::new(board),
            1,
            vec![],
            now,
            0,
            EventKind::Post,
            payload,
        );
        let post = Post::from_signed_event(event, body.to_owned())?;
        if self.is_withdrawn(post.id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "post was withdrawn",
            ));
        }
        self.persist(&attachment_filename(post.id), bytes)?;
        if let Err(error) = self.store(post.clone(), Retention::Owned, now) {
            self.remove_attachment(post.id)?;
            return Err(error);
        }
        Ok(post)
    }

    pub fn save_attachment(&self, id: PostId, bytes: &[u8]) -> io::Result<()> {
        let post = self
            .posts
            .get(&id)
            .and_then(|(post, retention, _)| {
                (!matches!(retention, Retention::Cached)).then_some(post)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "save the post before keeping its attachment",
                )
            })?;
        let hint = post.hint();
        let attachment = hint
            .attachment()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "post has no attachment"))?;
        attachment.verify_bytes(bytes)?;
        self.persist(&attachment_filename(hint.thread_id()), bytes)
    }

    pub fn read_attachment(&self, id: PostId) -> io::Result<Option<Vec<u8>>> {
        let Some(hint) = self.posts.get(&id).map(|(post, _, _)| post.hint()) else {
            return Ok(None);
        };
        let Some(attachment) = hint.attachment() else {
            return Ok(None);
        };
        let path = self.directory.join(attachment_filename(hint.thread_id()));
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() == attachment.manifest().bytes => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid attachment size",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let bytes = fs::read(path)?;
        attachment.verify_bytes(&bytes)?;
        Ok(Some(bytes))
    }

    pub(super) fn remove_attachment(&self, root: PostId) -> io::Result<()> {
        match fs::remove_file(self.directory.join(attachment_filename(root))) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

pub(super) fn attachment_filename(root: PostId) -> String {
    Path::new(&filename(root))
        .with_extension("attachment")
        .to_string_lossy()
        .into_owned()
}
