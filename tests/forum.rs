use mesh_forum::forum::{Bookmark, CacheLimits, Forum};
use mesh_identity::device::DeviceIdentity;

#[test]
fn bookmarks_survive_cleanup_and_only_shared_copies_are_offered() {
    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let mut first = Forum::open(&directory.path().join("author")).unwrap();
    let mut reader = Forum::open(&directory.path().join("reader")).unwrap();
    let post = first.publish(&author, [1; 32], "Hello", "Body", 1).unwrap();
    let id = post.id;
    assert!(first.offer(id).is_some());
    reader.cache(post, 2).unwrap();
    assert!(reader.offer(id).is_none());
    reader.bookmark(id, Bookmark::Local).unwrap();
    let limits = CacheLimits {
        posts: 0,
        bytes: 0,
        idle_seconds: 0,
    };
    reader.evict(100, limits).unwrap();
    assert!(reader.offer(id).is_none());
    assert_eq!(reader.read(id, 100).unwrap().unwrap().body, "Body");
    reader.bookmark(id, Bookmark::Shared).unwrap();
    drop(reader);
    let mut reader = Forum::open(&directory.path().join("reader")).unwrap();
    assert!(reader.offer(id).is_some());
    reader.unbookmark(id).unwrap();
    reader.evict(101, limits).unwrap();
    assert!(reader.read(id, 101).unwrap().is_none());
}

#[tokio::test]
async fn reader_fetches_a_shared_bookmark_without_the_author() {
    use mesh_connect::PeerSession;
    use mesh_forum::forum::{Staging, StagingLimits, fetch_index, serve_request, sync_withdrawals};
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};

    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let holder = DeviceIdentity::generate();
    let reader = DeviceIdentity::generate();
    let board = [21; 32];
    let body = "a".repeat(32 * 1024) + "Available offline";
    let mut origin = Forum::open(&directory.path().join("author")).unwrap();
    let post = origin
        .publish_with_attachment(&author, board, "Hello", &body, 1, "demo.bin", b"attachment")
        .unwrap();
    let id = post.id;
    let staging_root = directory.path().join("staging");
    let mut staging = Staging::open(&staging_root, StagingLimits::default()).unwrap();
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    let partial = staging_root.join(format!("{hex}.partial"));
    std::fs::create_dir(&partial).unwrap();
    std::fs::write(partial.join("1"), "Available offline").unwrap();
    let mut shared = Forum::open(&directory.path().join("shared")).unwrap();
    shared.cache(post, 1).unwrap();
    shared.bookmark(id, Bookmark::Shared).unwrap();
    shared.save_attachment(id, b"attachment").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut local = Forum::open(&directory.path().join("local")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            async {
                let stream = TcpStream::connect(address).await?;
                let mut session = PeerSession::connect(
                    stream,
                    &reader.secret_bytes().1,
                    &holder.public().x25519_pub,
                )
                .await
                .unwrap();
                let page = fetch_index(&mut session, board, 0).await?;
                assert_eq!(page.len(), 1);
                assert_eq!(page[0].title, "Hello");
                assert_eq!(page[0].author(), author.public().ed25519_pub);
                assert_eq!(page[0].manifest().bytes, body.len() as u64);
                assert!(local.read(id, 4)?.is_none());
                let stream = TcpStream::connect(address).await?;
                let session = PeerSession::connect(
                    stream,
                    &reader.secret_bytes().1,
                    &holder.public().x25519_pub,
                )
                .await
                .unwrap();
                local.cache(staging.fetch(&page[0], vec![session]).await?, 4)?;
                assert!(local.read_attachment(id)?.is_none());
                assert_eq!(page[0].attachment().unwrap().name(), "demo.bin");
                let stream = TcpStream::connect(address).await?;
                let session = PeerSession::connect(
                    stream,
                    &reader.secret_bytes().1,
                    &holder.public().x25519_pub,
                )
                .await
                .unwrap();
                let attachment = staging.fetch_attachment(&page[0], vec![session]).await?;
                assert_eq!(attachment, b"attachment");

                Ok::<_, std::io::Error>(())
            },
            async {
                for _ in 0..3 {
                    let (stream, _) = listener.accept().await?;
                    let mut session =
                        PeerSession::accept(stream, &holder.secret_bytes().1, |key| {
                            *key == reader.public().x25519_pub
                        })
                        .await
                        .unwrap();
                    serve_request(&mut session, &shared, board).await?;
                }
                Ok::<_, std::io::Error>(())
            }
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(local.read(id, 4).unwrap().unwrap().body, body);
    assert!(!partial.exists());
    assert!(local.offer(id).is_none());

    local.bookmark(id, Bookmark::Shared).unwrap();
    shared
        .apply_withdrawal(origin.withdraw(&author, id, 5).unwrap())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            async {
                let stream = TcpStream::connect(address).await?;
                let mut session = PeerSession::connect(
                    stream,
                    &reader.secret_bytes().1,
                    &holder.public().x25519_pub,
                )
                .await
                .unwrap();
                assert!(
                    sync_withdrawals(&mut session, &mut local, board, None)
                        .await?
                        .is_none()
                );
                Ok::<_, std::io::Error>(())
            },
            async {
                let (stream, _) = listener.accept().await?;
                let mut session = PeerSession::accept(stream, &holder.secret_bytes().1, |key| {
                    *key == reader.public().x25519_pub
                })
                .await
                .unwrap();
                serve_request(&mut session, &shared, board).await
            }
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(local.is_withdrawn(id));
    assert!(local.offer(id).is_none());
    assert!(local.read(id, 6).unwrap().is_none());
}

#[test]
fn signed_index_rejects_a_different_body() {
    use mesh_forum::forum::Post;

    let directory = tempfile::tempdir().unwrap();
    let post = Forum::open(directory.path())
        .unwrap()
        .publish(&DeviceIdentity::generate(), [1; 32], "Title", "abcd", 1)
        .unwrap();
    assert!(Post::from_signed_event(post.signed_event().clone(), "abce".into()).is_err());
}

#[test]
fn author_withdrawal_blocks_old_copies_even_if_it_arrives_first() {
    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let stranger = DeviceIdentity::generate();
    let mut origin = Forum::open(&directory.path().join("origin")).unwrap();
    let replica_path = directory.path().join("replica");
    let mut replica = Forum::open(&replica_path).unwrap();
    let post = origin
        .publish(&author, [1; 32], "Title", "Text", 1)
        .unwrap();
    assert!(origin.withdraw(&stranger, post.id, 2).is_err());
    let withdrawal = origin.withdraw(&author, post.id, 2).unwrap();
    assert!(origin.offer(post.id).is_none());
    replica.apply_withdrawal(withdrawal).unwrap();
    drop(replica);
    let mut replica = Forum::open(&replica_path).unwrap();
    assert!(replica.cache(post.clone(), 3).is_err());
    assert!(replica.read(post.id, 3).unwrap().is_none());
}

#[test]
fn configured_cache_limit_is_enforced_on_insert_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let mut origin = Forum::open(&directory.path().join("origin")).unwrap();
    let first = origin.publish(&author, [1; 32], "First", "One", 1).unwrap();
    let second = origin
        .publish(&author, [1; 32], "Second", "Two", 2)
        .unwrap();
    let path = directory.path().join("reader");
    let limits = CacheLimits {
        posts: 1,
        bytes: 1024,
        idle_seconds: 10,
    };
    let mut reader = Forum::open_with_cache(&path, limits, 1).unwrap();
    reader.cache(first.clone(), 1).unwrap();
    reader.cache(second.clone(), 2).unwrap();
    assert!(reader.read(first.id, 2).unwrap().is_none());
    assert!(reader.read(second.id, 2).unwrap().is_some());
    drop(reader);
    let mut reader = Forum::open_with_cache(&path, limits, 12).unwrap();
    assert!(reader.read(second.id, 12).unwrap().is_none());
}

#[test]
fn staging_reclaims_expired_and_over_budget_partials() {
    use mesh_forum::forum::{Staging, StagingLimits};
    use std::{
        fs,
        time::{Duration, SystemTime},
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let idle = Duration::from_secs(60);
    let mut storage = Staging::open(root, StagingLimits { bytes: 4, idle }).unwrap();
    for name in ["a.partial", "b.partial"] {
        let partial = root.join(name);
        fs::create_dir(&partial).unwrap();
        fs::write(partial.join("0"), b"abcd").unwrap();
    }
    let now = SystemTime::now();
    assert_eq!(storage.maintain(now).unwrap(), 4);
    assert_eq!(storage.maintain(now + idle).unwrap(), 0);
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn editing_preserves_saved_revision_and_withdrawal_covers_the_whole_post() {
    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let mut origin = Forum::open(&directory.path().join("origin")).unwrap();
    let mut reader = Forum::open(&directory.path().join("reader")).unwrap();
    let first = origin.publish(&author, [5; 32], "First", "old", 1).unwrap();
    reader.cache(first.clone(), 2).unwrap();
    reader.bookmark(first.id, Bookmark::Shared).unwrap();
    let edited = origin.edit(&author, first.id, "Revised", "new", 3).unwrap();
    assert_eq!(edited.hint().thread_id(), first.id);
    assert_eq!(edited.hint().previous_revision(), Some(first.id));
    assert_eq!(reader.read(first.id, 3).unwrap().unwrap().body, "old");
    reader.cache(edited.clone(), 3).unwrap();
    reader
        .apply_withdrawal(origin.withdraw(&author, edited.id, 4).unwrap())
        .unwrap();
    assert!(reader.offer(first.id).is_none());
    assert!(reader.read(edited.id, 4).unwrap().is_none());
    assert!(reader.cache(edited, 5).is_err());
}

#[test]
fn attachment_is_signed_and_requires_explicit_local_save() {
    let directory = tempfile::tempdir().unwrap();
    let author = DeviceIdentity::generate();
    let mut origin = Forum::open(&directory.path().join("origin")).unwrap();
    let mut reader = Forum::open(&directory.path().join("reader")).unwrap();
    let post = origin
        .publish_with_attachment(&author, [2; 32], "Title", "Body", 1, "data.bin", b"bytes")
        .unwrap();
    assert_eq!(post.hint().attachment().unwrap().name(), "data.bin");
    assert_eq!(origin.read_attachment(post.id).unwrap().unwrap(), b"bytes");
    reader.cache(post.clone(), 2).unwrap();
    reader.bookmark(post.id, Bookmark::Shared).unwrap();
    assert!(reader.read_attachment(post.id).unwrap().is_none());
    assert!(reader.save_attachment(post.id, b"wrong").is_err());
    reader.save_attachment(post.id, b"bytes").unwrap();
    assert_eq!(reader.read_attachment(post.id).unwrap().unwrap(), b"bytes");
    reader
        .apply_withdrawal(origin.withdraw(&author, post.id, 3).unwrap())
        .unwrap();
    assert!(reader.read_attachment(post.id).unwrap().is_none());
}

#[test]
fn revision_groups_keep_the_last_unambiguous_version() {
    use mesh_forum::forum::{RevisionStatus, group_revisions};
    let directory = tempfile::tempdir().unwrap();
    let mut forum = Forum::open(directory.path()).unwrap();
    let author = DeviceIdentity::generate();
    let root = forum.publish(&author, [1; 32], "Title", "root", 1).unwrap();
    let second = forum.edit(&author, root.id, "Title", "second", 2).unwrap();
    let third = forum.edit(&author, second.id, "Title", "third", 3).unwrap();
    let fork = forum.edit(&author, second.id, "Title", "fork", 4).unwrap();

    let complete = group_revisions([third.hint(), second.hint()]);
    assert_eq!(complete[0].head.id, third.id);
    assert_eq!(complete[0].status, RevisionStatus::Current);
    let gap = group_revisions([third.hint()]);
    assert_eq!(gap[0].head.id, root.id);
    assert_eq!(gap[0].status, RevisionStatus::MissingPrevious);
    let conflict = group_revisions([third.hint(), fork.hint(), second.hint()]);
    assert_eq!(conflict.len(), 1);
    assert_eq!(conflict[0].head.id, second.id);
    assert_eq!(conflict[0].status, RevisionStatus::Conflict);
}
