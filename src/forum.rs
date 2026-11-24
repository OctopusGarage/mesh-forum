//! Durable local posts and retention policy for public forum content.

pub mod record;

use crate::forum::record::{ConversationId, Event, EventKind};
use bincode::Options;
use mesh_identity::device::DeviceIdentity;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

const MAX_BODY: usize = 1024 * 1024;
const MAX_TITLE: usize = 512;
const MAX_RECORD: u64 = (MAX_BODY + MAX_TITLE + 8192) as u64;

pub type PostId = [u8; 32];

const CHUNK_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct BodyMetadata {
    bytes: u64,
    checksum: [u8; 32],
    hashes: Vec<[u8; 32]>,
}

impl BodyMetadata {
    fn from_body(body: &str) -> Self {
        Self::from_bytes(body.as_bytes())
    }

    fn from_bytes(body: &[u8]) -> Self {
        Self {
            bytes: body.len() as u64,
            checksum: mesh_blob::file_checksum(body),
            hashes: body
                .chunks(CHUNK_BYTES)
                .map(mesh_blob::chunk_hash)
                .collect(),
        }
    }

    fn manifest(&self) -> crate::PostBodyManifest<'_> {
        crate::PostBodyManifest {
            bytes: self.bytes,
            chunk_bytes: CHUNK_BYTES as u32,
            checksum: self.checksum,
            chunk_hashes: &self.hashes,
        }
    }

    fn valid(&self) -> bool {
        self.bytes <= MAX_BODY as u64
            && self.hashes.len() == (self.bytes as usize).div_ceil(CHUNK_BYTES)
    }
}

/// An author-verified index entry, independent of the downloaded body.
#[derive(Clone)]
pub struct PostHint {
    pub id: PostId,
    pub title: String,
    metadata: BodyMetadata,
    original: Option<Event>,
    author_name: Option<String>,
    attachment: Option<Attachment>,
    event: Event,
}

impl PostHint {
    pub fn author(&self) -> [u8; 32] {
        *self.event.author.ed25519_pub()
    }

    pub fn author_name(&self) -> Option<&str> {
        self.author_name.as_deref()
    }

    pub fn manifest(&self) -> crate::PostBodyManifest<'_> {
        self.metadata.manifest()
    }

    pub fn attachment(&self) -> Option<&Attachment> {
        self.attachment.as_ref()
    }

    pub fn thread_id(&self) -> PostId {
        *self.original_event().id.as_bytes()
    }

    pub fn previous_revision(&self) -> Option<PostId> {
        self.event.parents.first().map(|id| *id.as_bytes())
    }

    fn original_event(&self) -> &Event {
        self.original.as_ref().unwrap_or(&self.event)
    }

    fn verify(event: Event) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid signed post index");
        if event.ciphertext.len() > 7168 || !event.verify_integrity() || !event.verify_signature() {
            return Err(invalid());
        }
        let (mut author_name, payload) = if event.ciphertext.starts_with(b"AUTHOR01") {
            let (_, name, payload): ([u8; 8], String, Vec<u8>) = codec()
                .deserialize(&event.ciphertext)
                .map_err(io::Error::other)?;
            if !account::valid_name(&name) {
                return Err(invalid());
            }
            (Some(name), std::borrow::Cow::Owned(payload))
        } else {
            (
                None,
                std::borrow::Cow::Borrowed(event.ciphertext.as_slice()),
            )
        };
        let attached = payload.starts_with(b"FORUM-V3");
        let (domain, title, metadata, original, attachment) = match event.kind {
            EventKind::Post if attached && event.parents.is_empty() && event.seq == 1 => {
                let (domain, title, metadata, attachment): (
                    [u8; 8],
                    String,
                    BodyMetadata,
                    Attachment,
                ) = codec().deserialize(&payload).map_err(io::Error::other)?;
                attachment.verify()?;
                (domain, title, metadata, None, Some(attachment))
            }
            EventKind::Post if event.parents.is_empty() && event.seq == 1 => {
                let (domain, title, metadata): ([u8; 8], String, BodyMetadata) =
                    codec().deserialize(&payload).map_err(io::Error::other)?;
                (domain, title, metadata, None, None)
            }
            EventKind::Revision if event.parents.len() == 1 && event.seq > 1 => {
                let (domain, title, metadata, original): ([u8; 8], String, BodyMetadata, Event) =
                    codec().deserialize(&payload).map_err(io::Error::other)?;
                if original.kind != EventKind::Post
                    || original.conversation_id != event.conversation_id
                    || original.author.ed25519_pub() != event.author.ed25519_pub()
                    || original.lamport > event.lamport
                    || (event.seq == 2) != (event.parents[0] == original.id)
                {
                    return Err(invalid());
                }
                let root = Self::verify(original.clone())?;
                author_name = root.author_name.or(author_name);
                (domain, title, metadata, Some(original), root.attachment)
            }
            _ => return Err(invalid()),
        };
        validate(&title, "")?;
        if (domain != *b"FORUM-V2" && domain != *b"FORUM-V3") || !metadata.valid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid body manifest",
            ));
        }
        Ok(Self {
            id: *event.id.as_bytes(),
            title,
            metadata,
            original,
            author_name,
            attachment,
            event,
        })
    }
}

#[derive(Clone)]
pub struct Post {
    pub id: PostId,
    pub title: String,
    pub body: String,
    hint: PostHint,
}

impl Post {
    pub fn hint(&self) -> &PostHint {
        &self.hint
    }

    pub fn signed_event(&self) -> &Event {
        &self.hint.event
    }

    pub fn from_signed_event(event: Event, body: String) -> io::Result<Self> {
        let hint = PostHint::verify(event)?;
        validate(&hint.title, &body)?;
        if hint.metadata != BodyMetadata::from_body(&body) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "body does not match manifest",
            ));
        }
        Ok(Self {
            id: hint.id,
            title: hint.title.clone(),
            body,
            hint,
        })
    }
}

fn author_payload(name: Option<&str>, payload: Vec<u8>) -> io::Result<Vec<u8>> {
    match name {
        Some(name) => codec()
            .serialize(&(*b"AUTHOR01", name, payload))
            .map_err(io::Error::other),
        None => Ok(payload),
    }
}

fn validate(title: &str, body: &str) -> io::Result<()> {
    if title.trim().is_empty() || title.len() > MAX_TITLE || body.len() > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid post size or title",
        ));
    }
    Ok(())
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new().with_limit(MAX_RECORD)
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Bookmark {
    Local,
    Shared,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum Retention {
    Owned,
    Cached,
    Bookmarked(Bookmark),
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct CacheLimits {
    pub posts: usize,
    pub bytes: u64,
    pub idle_seconds: u64,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            posts: 200,
            bytes: 256 * 1024 * 1024,
            idle_seconds: 14 * 24 * 60 * 60,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredPost {
    event: Event,
    body: String,
    retention: Retention,
    accessed: u64,
}

pub struct Forum {
    directory: PathBuf,
    posts: BTreeMap<PostId, (Post, Retention, u64)>,
    withdrawals: BTreeMap<PostId, Withdrawal>,
    cache_limits: CacheLimits,
}

impl Forum {
    /// Open a data directory owned by this application instance.
    pub fn open(directory: &Path) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let mut posts = BTreeMap::new();
        let mut withdrawals = BTreeMap::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "withdrawal")
            {
                let withdrawal = withdrawal::read_withdrawal(&entry.path())?;
                withdrawals.insert(withdrawal.post_id(), withdrawal);
                continue;
            }
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "post")
            {
                continue;
            }
            if entry.metadata()?.len() > MAX_RECORD {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized local record",
                ));
            }
            let stored: StoredPost = codec()
                .deserialize(&fs::read(entry.path())?)
                .map_err(io::Error::other)?;
            let post = Post::from_signed_event(stored.event, stored.body)?;
            if entry.path() != directory.join(filename(post.id)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local record ID mismatch",
                ));
            }
            posts.insert(post.id, (post, stored.retention, stored.accessed));
        }
        let mut forum = Self {
            directory: directory.to_owned(),
            posts,
            withdrawals,
            cache_limits: CacheLimits::default(),
        };
        let withdrawn: Vec<_> = forum.withdrawals.keys().copied().collect();
        for id in withdrawn {
            forum.remove_thread(id)?;
        }
        Ok(forum)
    }

    /// Open the local store and reap expired or over-budget browsing copies.
    pub fn open_with_cache(directory: &Path, limits: CacheLimits, now: u64) -> io::Result<Self> {
        let mut forum = Self::open(directory)?;
        forum.set_cache_limits(limits, now)?;
        Ok(forum)
    }

    pub fn set_cache_limits(&mut self, limits: CacheLimits, now: u64) -> io::Result<usize> {
        self.cache_limits = limits;
        self.maintain_cache(now)
    }

    pub fn maintain_cache(&mut self, now: u64) -> io::Result<usize> {
        self.evict(now, self.cache_limits)
    }

    pub fn publish(
        &mut self,
        identity: &DeviceIdentity,
        board: [u8; 32],
        title: &str,
        body: &str,
        now: u64,
    ) -> io::Result<Post> {
        self.publish_as(identity, board, title, body, now, None)
    }

    fn publish_as(
        &mut self,
        identity: &DeviceIdentity,
        board: [u8; 32],
        title: &str,
        body: &str,
        now: u64,
        author_name: Option<&str>,
    ) -> io::Result<Post> {
        validate(title, body)?;
        let payload = codec()
            .serialize(&(*b"FORUM-V2", title, BodyMetadata::from_body(body)))
            .map_err(io::Error::other)?;
        let payload = author_payload(author_name, payload)?;
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
        self.store(post.clone(), Retention::Owned, now)?;
        Ok(post)
    }

    pub fn edit(
        &mut self,
        author: &DeviceIdentity,
        previous: PostId,
        title: &str,
        body: &str,
        now: u64,
    ) -> io::Result<Post> {
        self.edit_as(author, previous, title, body, now, None)
    }

    fn edit_as(
        &mut self,
        author: &DeviceIdentity,
        previous: PostId,
        title: &str,
        body: &str,
        now: u64,
        author_name: Option<&str>,
    ) -> io::Result<Post> {
        validate(title, body)?;
        let (post, _, _) = self.posts.get(&previous).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "previous revision not found")
        })?;
        if post.hint.author() != author.public().ed25519_pub {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the author can edit",
            ));
        }
        let prior = post.signed_event();
        let seq = prior
            .seq
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "too many revisions"))?;
        let payload = codec()
            .serialize(&(
                *b"FORUM-V2",
                title,
                BodyMetadata::from_body(body),
                post.hint.original_event(),
            ))
            .map_err(io::Error::other)?;
        let payload = author_payload(author_name, payload)?;
        let event = Event::new(
            author,
            prior.conversation_id,
            seq,
            vec![prior.id],
            now.max(prior.lamport),
            0,
            EventKind::Revision,
            payload,
        );
        let post = Post::from_signed_event(event, body.to_owned())?;
        self.store(post.clone(), Retention::Owned, now)?;
        Ok(post)
    }

    pub fn cache(&mut self, post: Post, now: u64) -> io::Result<()> {
        let post = Post::from_signed_event(post.hint.event, post.body)?;
        if self.posts.contains_key(&post.id) {
            return Ok(());
        }
        self.store(post, Retention::Cached, now)?;
        self.maintain_cache(now)?;
        Ok(())
    }

    pub fn bookmark(&mut self, id: PostId, mode: Bookmark) -> io::Result<()> {
        self.set_retention(id, Retention::Bookmarked(mode))
    }

    pub fn unbookmark(&mut self, id: PostId) -> io::Result<()> {
        self.set_retention(id, Retention::Cached)
    }

    /// Return local candidates for serving. The caller checks board and peer authorization.
    pub fn offer(&self, id: PostId) -> Option<&Post> {
        let (post, retention, _) = self.posts.get(&id)?;
        matches!(
            retention,
            Retention::Owned | Retention::Bookmarked(Bookmark::Shared)
        )
        .then_some(post)
    }

    pub fn read(&mut self, id: PostId, now: u64) -> io::Result<Option<Post>> {
        self.maintain_cache(now)?;
        let Some((post, retention, accessed)) = self.posts.get(&id).cloned() else {
            return Ok(None);
        };
        if matches!(retention, Retention::Cached) && now > accessed {
            self.store(post.clone(), retention, now)?;
        }
        Ok(Some(post))
    }

    pub fn evict(&mut self, now: u64, limits: CacheLimits) -> io::Result<usize> {
        let mut cached: Vec<_> = self
            .posts
            .values()
            .filter_map(|(post, retention, accessed)| {
                matches!(retention, Retention::Cached).then_some((
                    *accessed,
                    post.id,
                    (post.body.len() + post.signed_event().ciphertext.len()) as u64,
                ))
            })
            .collect();
        cached.sort_unstable();
        let mut bytes: u64 = cached.iter().map(|(_, _, size)| size).sum();
        let mut remaining = cached.len();
        let mut removed = 0;
        for (accessed, id, size) in cached {
            if now.saturating_sub(accessed) < limits.idle_seconds
                && remaining <= limits.posts
                && bytes <= limits.bytes
            {
                break;
            }
            fs::remove_file(self.directory.join(filename(id)))?;
            if let Some((post, _, _)) = self.posts.remove(&id) {
                let root = post.hint().thread_id();
                if !self
                    .posts
                    .values()
                    .any(|(post, _, _)| post.hint().thread_id() == root)
                {
                    self.remove_attachment(root)?;
                }
            }
            remaining -= 1;
            bytes -= size;
            removed += 1;
        }
        Ok(removed)
    }

    fn set_retention(&mut self, id: PostId, retention: Retention) -> io::Result<()> {
        let (post, existing, accessed) = self.posts.get(&id).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "post has not been downloaded")
        })?;
        if matches!(existing, Retention::Owned) {
            return Ok(());
        }
        self.store(post, retention, accessed)
    }

    fn store(&mut self, post: Post, retention: Retention, accessed: u64) -> io::Result<()> {
        if self.is_withdrawn(post.hint.thread_id()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "post was withdrawn",
            ));
        }
        let record = StoredPost {
            event: post.signed_event().clone(),
            body: post.body.clone(),
            retention,
            accessed,
        };
        let bytes = codec().serialize(&record).map_err(io::Error::other)?;
        self.persist(&filename(post.id), &bytes)?;
        self.posts.insert(post.id, (post, retention, accessed));
        Ok(())
    }

    fn persist(&self, filename: &str, bytes: &[u8]) -> io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.directory.join(filename))?;
        Ok(())
    }
}

fn filename(id: PostId) -> String {
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{hex}.post")
}

mod network;
pub use network::{fetch_index, fetch_resumable, serve_request, sync_withdrawals};

mod withdrawal;
pub use withdrawal::Withdrawal;

mod staging;
pub use staging::{Staging, StagingLimits};

mod attachment;
pub use attachment::Attachment;

mod node;
pub use node::{Invite, LocalPostSource, LocalSearchHit, Node, PeerStatus, StorageUsage};

mod revision;
pub use revision::{RevisionGroup, RevisionStatus, group_revisions};

mod index_cache;

mod lan;
pub use lan::LAN_DISCOVERY_PORT;

mod account;
