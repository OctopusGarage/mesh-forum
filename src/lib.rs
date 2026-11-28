//! Forum content download using Mesh Foundation.

pub use mesh_blob::session::{SessionChunkSource, serve_request as serve_post_body_request};
use mesh_blob::{BlobLayout, ChunkStore, DirectoryChunkStore, FileError, SparseBlobReceiver};
pub use mesh_blob::{ChunkSource, DownloadError, SparseReceiveError};
use std::{io, num::NonZeroUsize, path::Path};

/// The caller authenticates this metadata against the exact post revision.
pub struct PostBodyManifest<'a> {
    pub bytes: u64,
    pub chunk_bytes: u32,
    pub checksum: [u8; 32],
    pub chunk_hashes: &'a [[u8; 32]],
}

#[derive(Debug)]
pub enum PostBodyDownloadError {
    Manifest(FileError),
    Open(io::Error),
    Resume(SparseReceiveError<io::Error>),
    Download(DownloadError<io::Error>),
}

/// Resume a post body into a private directory dedicated to this revision.
/// The caller publishes the verified chunks in index order after success.
pub async fn download_post_body(
    manifest: PostBodyManifest<'_>,
    staging: &Path,
    sources: &[impl ChunkSource],
    max_bytes: u64,
    max_chunks: u32,
    concurrency: NonZeroUsize,
) -> Result<(), PostBodyDownloadError> {
    download_tracked(
        manifest,
        staging,
        sources,
        max_bytes,
        max_chunks,
        concurrency,
        None,
    )
    .await
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub verified_bytes: u64,
    pub total_bytes: u64,
}

type ProgressSender = tokio::sync::watch::Sender<DownloadProgress>;

pub(crate) fn report_progress(
    progress: Option<&ProgressSender>,
    verified_bytes: u64,
    total_bytes: u64,
) {
    if let Some(progress) = progress {
        progress.send_replace(DownloadProgress {
            verified_bytes,
            total_bytes,
        });
    }
}

async fn download_tracked(
    manifest: PostBodyManifest<'_>,
    staging: &Path,
    sources: &[impl ChunkSource],
    max_bytes: u64,
    max_chunks: u32,
    concurrency: NonZeroUsize,
    progress: Option<&ProgressSender>,
) -> Result<(), PostBodyDownloadError> {
    let layout = BlobLayout::new(manifest.bytes, manifest.chunk_bytes, max_bytes)
        .map_err(PostBodyDownloadError::Manifest)?;
    let mut receiver =
        SparseBlobReceiver::new(layout, manifest.checksum, manifest.chunk_hashes, max_chunks)
            .map_err(PostBodyDownloadError::Manifest)?;
    let mut store = DirectoryChunkStore::open(staging).map_err(PostBodyDownloadError::Open)?;
    receiver
        .restore(&mut store)
        .map_err(PostBodyDownloadError::Resume)?;
    let missing = receiver
        .missing()
        .try_fold(0_u64, |bytes, index| {
            layout
                .chunk_len(index)
                .map(|length| bytes + u64::from(length))
        })
        .map_err(PostBodyDownloadError::Manifest)?;
    let mut store = ReportingStore {
        inner: store,
        progress,
        verified: manifest.bytes - missing,
        total: manifest.bytes,
    };
    report_progress(progress, store.verified, store.total);
    receiver
        .download(&mut store, sources, concurrency)
        .await
        .map_err(PostBodyDownloadError::Download)
}

struct ReportingStore<'a> {
    inner: DirectoryChunkStore,
    progress: Option<&'a ProgressSender>,
    verified: u64,
    total: u64,
}

impl ChunkStore for ReportingStore<'_> {
    type Error = io::Error;

    fn read(&mut self, index: u32, expected_len: u32) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(index, expected_len)
    }

    fn write(&mut self, index: u32, bytes: &[u8]) -> io::Result<()> {
        self.inner.write(index, bytes)?;
        self.verified += bytes.len() as u64;
        report_progress(self.progress, self.verified, self.total);
        Ok(())
    }
}

pub mod forum;

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[tokio::test]
    async fn progress_counts_valid_restored_blocks_but_ignores_corrupt_ones() {
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(staging.path().join("0"), b"ab").unwrap();
        std::fs::write(staging.path().join("1"), b"xx").unwrap();
        let hashes = [mesh_blob::chunk_hash(b"ab"), mesh_blob::chunk_hash(b"cd")];
        let manifest = PostBodyManifest {
            bytes: 4,
            chunk_bytes: 2,
            checksum: mesh_blob::file_checksum(b"abcd"),
            chunk_hashes: &hashes,
        };
        let sources: [SessionChunkSource<tokio::net::TcpStream>; 0] = [];
        let (progress, update) = tokio::sync::watch::channel(DownloadProgress::default());
        assert!(
            download_tracked(
                manifest,
                staging.path(),
                &sources,
                4,
                2,
                NonZeroUsize::MIN,
                Some(&progress),
            )
            .await
            .is_err()
        );
        assert_eq!(
            *update.borrow(),
            DownloadProgress {
                verified_bytes: 2,
                total_bytes: 4
            }
        );
    }
}
