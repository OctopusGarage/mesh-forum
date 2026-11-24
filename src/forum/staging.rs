//! Storage policy for verified partial downloads.

use super::{Post, PostHint, network::fetch_blob};
use mesh_connect::PeerSession;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Clone, Copy)]
pub struct StagingLimits {
    pub bytes: u64,
    pub idle: Duration,
}

impl Default for StagingLimits {
    fn default() -> Self {
        Self {
            bytes: 64 * 1024 * 1024,
            idle: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Owns the partial-download directory for one running application instance.
/// Mutable access serializes downloads and cleanup within the instance.
pub struct Staging {
    root: PathBuf,
    limits: StagingLimits,
}

struct Partial {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

impl Staging {
    pub fn open(root: &Path, limits: StagingLimits) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let mut store = Self {
            root: root.to_owned(),
            limits,
        };
        store.maintain(SystemTime::now())?;
        Ok(store)
    }

    pub(super) fn usage(&self) -> io::Result<u64> {
        let mut used = 0_u64;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "partial")
            {
                used = used.saturating_add(partial(&entry.path())?.bytes);
            }
        }
        Ok(used)
    }

    pub fn maintain(&mut self, now: SystemTime) -> io::Result<u64> {
        self.reap(now, None, 0)
    }

    /// Keep partial blocks after failure. Remove staging after body verification.
    pub async fn fetch<S>(
        &mut self,
        hint: &PostHint,
        peers: Vec<PeerSession<S>>,
    ) -> io::Result<Post>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.fetch_with_progress(hint, peers, None).await
    }

    pub(super) async fn fetch_with_progress<S>(
        &mut self,
        hint: &PostHint,
        peers: Vec<PeerSession<S>>,
        progress: Option<&crate::ProgressSender>,
    ) -> io::Result<Post>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let hint = PostHint::verify(hint.event.clone())?;
        let body = self.fetch_file(&hint, peers, false, progress).await?;
        Post::from_signed_event(
            hint.event,
            String::from_utf8(body).map_err(io::Error::other)?,
        )
    }

    /// Download a detached file only when explicitly requested.
    pub async fn fetch_attachment<S>(
        &mut self,
        hint: &PostHint,
        peers: Vec<PeerSession<S>>,
    ) -> io::Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.fetch_attachment_with_progress(hint, peers, None).await
    }

    pub(super) async fn fetch_attachment_with_progress<S>(
        &mut self,
        hint: &PostHint,
        peers: Vec<PeerSession<S>>,
        progress: Option<&crate::ProgressSender>,
    ) -> io::Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let hint = PostHint::verify(hint.event.clone())?;
        self.fetch_file(&hint, peers, true, progress).await
    }

    async fn fetch_file<S>(
        &mut self,
        hint: &PostHint,
        peers: Vec<PeerSession<S>>,
        attachment: bool,
        progress: Option<&crate::ProgressSender>,
    ) -> io::Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let bytes = if attachment {
            hint.attachment()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "post has no attachment"))?
                .manifest()
                .bytes
        } else {
            hint.manifest().bytes
        };
        if bytes > self.limits.bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "post exceeds download quota",
            ));
        }
        let path = self
            .root
            .join(super::filename(hint.id))
            .with_extension(if attachment {
                "attachment.partial"
            } else {
                "partial"
            });
        let now = SystemTime::now();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid staging path",
                ));
            }
            Ok(_) => {
                let existing = partial(&path)?;
                if existing.bytes > bytes
                    || now.duration_since(existing.modified).unwrap_or_default() >= self.limits.idle
                {
                    fs::remove_dir_all(&path)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.reap(now, Some(&path), bytes)?;
        let post = fetch_blob(hint, &path, peers, attachment, progress).await?;
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        }
        Ok(post)
    }

    fn reap(&self, now: SystemTime, reserved: Option<&Path>, reserve: u64) -> io::Result<u64> {
        let mut entries = Vec::new();
        let mut used = 0_u64;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir()
                || path
                    .extension()
                    .is_none_or(|extension| extension != "partial")
                || reserved == Some(path.as_path())
            {
                continue;
            }
            let entry = partial(&path)?;
            used = used.saturating_add(entry.bytes);
            entries.push(entry);
        }
        entries.sort_unstable_by(|a, b| (a.modified, &a.path).cmp(&(b.modified, &b.path)));
        for entry in entries {
            if now.duration_since(entry.modified).unwrap_or_default() >= self.limits.idle
                || used > self.limits.bytes.saturating_sub(reserve)
            {
                fs::remove_dir_all(entry.path)?;
                used = used.saturating_sub(entry.bytes);
            }
        }
        Ok(used)
    }
}

fn partial(path: &Path) -> io::Result<Partial> {
    let mut modified = fs::symlink_metadata(path)?.modified()?;
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid partial download entry",
            ));
        }
        let metadata = entry.metadata()?;
        modified = modified.max(metadata.modified()?);
        bytes = bytes.saturating_add(metadata.len());
    }
    Ok(Partial {
        path: path.to_owned(),
        modified,
        bytes,
    })
}
