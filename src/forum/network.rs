//! A bounded request over an already authenticated and admitted session.

use super::{Forum, Post, PostHint, PostId, Withdrawal, codec};
use crate::forum::record::Event;
use bincode::Options;
use mesh_connect::PeerSession;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{io, num::NonZeroUsize, path::Path, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};

use super::board_index::{INDEX_LIMIT, PAGE_SIZE};
const PROTOCOL: [u8; 8] = *b"MFORUM07";

#[derive(Serialize, Deserialize)]
enum Request {
    Probe {
        board: [u8; 32],
    },
    Index {
        board: [u8; 32],
        offset: u16,
    },
    Revision {
        board: [u8; 32],
        id: PostId,
    },
    Withdrawals {
        board: [u8; 32],
        after: Option<PostId>,
    },
    Body {
        board: [u8; 32],
        id: PostId,
        attachment: bool,
    },
}

async fn send<S, T>(peer: &mut PeerSession<S>, value: &T) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = codec()
        .serialize(&(PROTOCOL, value))
        .map_err(io::Error::other)?;
    peer.send(&payload).await.map_err(io::Error::other)
}

async fn receive<S, T>(peer: &mut PeerSession<S>) -> io::Result<T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: DeserializeOwned,
{
    let bytes = peer.recv().await.map_err(io::Error::other)?;
    let (protocol, value): ([u8; 8], T) = codec().deserialize(&bytes).map_err(io::Error::other)?;
    if protocol != PROTOCOL {
        return Err(invalid("unexpected forum protocol"));
    }
    Ok(value)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_offset(offset: u16) -> io::Result<usize> {
    let offset = usize::from(offset);
    if offset >= INDEX_LIMIT {
        return Err(invalid("board index offset out of range"));
    }
    Ok(offset)
}

/// Serve one request. The host admits the peer and authorizes its board.
pub async fn serve_request<S>(
    peer: &mut PeerSession<S>,
    forum: &Forum,
    board: [u8; 32],
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_with_policy(peer, forum, board, true).await
}

pub(super) async fn serve_with_policy<S>(
    peer: &mut PeerSession<S>,
    forum: &Forum,
    board: [u8; 32],
    uploads: bool,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match receive(peer).await? {
        Request::Probe { board: requested } if requested == board => send(peer, &uploads).await,
        Request::Index {
            board: requested,
            offset,
        } if requested == board => {
            let offset = check_offset(offset)?;
            if !uploads {
                return send(peer, &0_u8).await;
            }
            let page = forum.index_page(board, offset);
            send(peer, &(page.len() as u8)).await?;
            for event in page {
                send(peer, event).await?;
            }
            Ok(())
        }
        Request::Revision {
            board: requested,
            id,
        } if requested == board => {
            let event = uploads
                .then(|| forum.offer(id))
                .flatten()
                .filter(|post| post.signed_event().conversation_id.as_bytes() == &board)
                .map(Post::signed_event);
            send(peer, &event).await
        }
        Request::Withdrawals {
            board: requested,
            after,
        } if requested == board => {
            use std::ops::Bound::{Excluded, Unbounded};
            let start = after.map_or(Unbounded, Excluded);
            let page: Vec<_> = forum
                .withdrawals
                .range((start, Unbounded))
                .filter_map(|(_, withdrawal)| (withdrawal.board() == board).then_some(withdrawal))
                .take(WITHDRAWAL_PAGE_SIZE)
                .collect();
            send(peer, &page).await
        }
        Request::Body {
            board: requested,
            id,
            attachment,
        } if requested == board => {
            if !uploads {
                return send(peer, &false).await;
            }
            let post = forum
                .offer(id)
                .filter(|post| post.signed_event().conversation_id.as_bytes() == &board);
            if let Some(post) = post {
                if !attachment {
                    send(peer, &true).await?;
                    return serve_body(peer, post.hint().manifest(), post.body.as_bytes()).await;
                }
                if let Some(file) = post.hint().attachment()
                    && let Some(bytes) = forum.read_attachment(id)?
                {
                    send(peer, &true).await?;
                    return serve_body(peer, file.manifest(), &bytes).await;
                }
            }
            send(peer, &false).await?;
            Ok(())
        }
        _ => Err(invalid("request outside authorized board")),
    }
}

pub(super) async fn probe<S>(peer: &mut PeerSession<S>, board: [u8; 32]) -> io::Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(peer, &Request::Probe { board }).await?;
    receive(peer).await
}

pub(super) async fn fetch_revision<S>(
    peer: &mut PeerSession<S>,
    board: [u8; 32],
    id: PostId,
    root: PostId,
) -> io::Result<Option<PostHint>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(peer, &Request::Revision { board, id }).await?;
    let event: Option<Event> = receive(peer).await?;
    let Some(event) = event else {
        return Ok(None);
    };
    if event.conversation_id.as_bytes() != &board || event.id.as_bytes() != &id {
        return Err(invalid("unexpected revision"));
    }
    let hint = PostHint::verify(event)?;
    if hint.thread_id() != root {
        return Err(invalid("revision belongs to another post"));
    }
    Ok(Some(hint))
}

pub async fn fetch_index<S>(
    peer: &mut PeerSession<S>,
    board: [u8; 32],
    offset: u16,
) -> io::Result<Vec<PostHint>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let start = check_offset(offset)?;
    send(peer, &Request::Index { board, offset }).await?;
    let count: u8 = receive(peer).await?;
    if usize::from(count) > PAGE_SIZE.min(INDEX_LIMIT - start) {
        return Err(invalid("oversized board index"));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut page = Vec::with_capacity(count.into());
    for _ in 0..count {
        let event: Event = receive(peer).await?;
        if event.conversation_id.as_bytes() != &board || !ids.insert(event.id) {
            return Err(invalid("invalid board index"));
        }
        page.push(PostHint::verify(event)?);
    }
    Ok(page)
}

async fn serve_body<S>(
    peer: &mut PeerSession<S>,
    manifest: crate::PostBodyManifest<'_>,
    body: &[u8],
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let checksum = manifest.checksum;
    let count = manifest.chunk_hashes.len();
    let chunk_bytes = manifest.chunk_bytes as usize;
    // One availability page and at most one request per chunk.
    for _ in 0..=count {
        let result = crate::serve_post_body_request(
            peer,
            |_, object, index, length| {
                let index = index as usize;
                if object != checksum || index >= count {
                    return None;
                }
                let start = index * chunk_bytes;
                let chunk = &body[start..body.len().min(start + chunk_bytes)];
                (chunk.len() == length as usize).then(|| chunk.to_vec())
            },
            |_, object, start, length| {
                let end = start.checked_add(length)? as usize;
                (object == checksum && end <= count).then(|| vec![true; length as usize])
            },
        )
        .await;
        match result {
            Ok(()) => {}
            Err(mesh_connect::TransportError::Io(error))
                if error.kind() == io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(error) => return Err(io::Error::other(error)),
        }
    }
    Ok(())
}

/// Resume one verified revision using any available authenticated source.
/// The caller owns the private staging directory and must dedicate it to this revision.
pub async fn fetch_resumable<S>(
    hint: &PostHint,
    staging: &Path,
    peers: Vec<PeerSession<S>>,
) -> io::Result<Post>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hint = PostHint::verify(hint.event.clone())?;
    let body = fetch_blob(&hint, staging, peers, false, None).await?;
    let body = String::from_utf8(body).map_err(io::Error::other)?;
    Post::from_signed_event(hint.event, body)
}

pub(super) async fn fetch_blob<S>(
    hint: &PostHint,
    staging: &Path,
    peers: Vec<PeerSession<S>>,
    attachment: bool,
    progress: Option<&crate::ProgressSender>,
) -> io::Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let manifest = if attachment {
        hint.attachment()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "post has no attachment"))?
            .manifest()
    } else {
        hint.manifest()
    };
    let count = manifest.chunk_hashes.len() as u32;
    let bytes = manifest.bytes;
    crate::report_progress(progress, 0, bytes);
    if count == 0 {
        return Ok(Vec::new());
    }
    let deadline = Duration::from_secs(5);
    let mut sources = Vec::new();
    let mut first_error = None;
    for mut peer in peers {
        let request = Request::Body {
            board: *hint.event.conversation_id.as_bytes(),
            id: hint.id,
            attachment,
        };
        let source = tokio::time::timeout(deadline, async {
            send(&mut peer, &request).await?;
            if !receive::<_, bool>(&mut peer).await? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "source no longer offers this post",
                ));
            }
            crate::SessionChunkSource::discover(manifest.checksum, count, count, peer, deadline)
                .await
                .map_err(io::Error::other)
        })
        .await
        .map_err(io::Error::other)
        .and_then(|result| result);
        match source {
            Ok(source) => sources.push(source),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    std::fs::create_dir_all(staging)?;
    crate::download_tracked(
        manifest,
        staging,
        &sources,
        super::MAX_BODY as u64,
        count,
        NonZeroUsize::new(4).unwrap(),
        progress,
    )
    .await
    .map_err(|error| {
        first_error.unwrap_or_else(|| io::Error::other(format!("post download failed: {error:?}")))
    })?;
    let mut body = Vec::with_capacity(bytes as usize);
    for index in 0..count {
        body.extend(std::fs::read(staging.join(index.to_string()))?);
    }
    Ok(body)
}

const WITHDRAWAL_PAGE_SIZE: usize = 12;

/// Sync a bounded page of durable withdrawals, including for posts not seen yet.
/// Start each complete rescan at None to catch later records sorting before the cursor.
pub async fn sync_withdrawals<S>(
    peer: &mut PeerSession<S>,
    forum: &mut Forum,
    board: [u8; 32],
    after: Option<PostId>,
) -> io::Result<Option<PostId>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (records, next) = fetch_withdrawals(peer, board, after).await?;
    for withdrawal in records {
        forum.apply_withdrawal(withdrawal)?;
    }
    Ok(next)
}

pub(super) async fn fetch_withdrawals<S>(
    peer: &mut PeerSession<S>,
    board: [u8; 32],
    after: Option<PostId>,
) -> io::Result<(Vec<Withdrawal>, Option<PostId>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(peer, &Request::Withdrawals { board, after }).await?;
    let records: Vec<Withdrawal> = receive(peer).await?;
    if records.len() > WITHDRAWAL_PAGE_SIZE {
        return Err(invalid("oversized withdrawal page"));
    }
    let mut cursor = after;
    for withdrawal in &records {
        if withdrawal.board() != board || cursor.is_some_and(|id| id >= withdrawal.post_id()) {
            return Err(invalid("invalid withdrawal page order or board"));
        }
        withdrawal.verify()?;
        cursor = Some(withdrawal.post_id());
    }
    let next = if records.len() == WITHDRAWAL_PAGE_SIZE {
        cursor
    } else {
        None
    };
    Ok((records, next))
}
