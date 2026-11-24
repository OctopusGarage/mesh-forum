use mesh_forum::forum::{Bookmark, LocalPostSource, Node};

#[tokio::test]
async fn three_running_nodes_publish_bookmark_and_serve_after_author_stops() {
    let root = tempfile::tempdir().unwrap();
    let board = [7; 32];
    let address = "127.0.0.1:0".parse().unwrap();
    let a = Node::start(&root.path().join("a"), address, board)
        .await
        .unwrap();
    let b = Node::start(&root.path().join("b"), address, board)
        .await
        .unwrap();
    let c = Node::start(&root.path().join("c"), address, board)
        .await
        .unwrap();
    a.add_peer(b.invite()).await.unwrap();
    b.add_peer(a.invite()).await.unwrap();
    b.add_peer(c.invite()).await.unwrap();
    c.add_peer(b.invite()).await.unwrap();
    let post = a.publish("Hello", "Written by A").await.unwrap();
    let owned = a.search_local_posts("  HELLO ").await;
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].hint.id, post.id);
    assert_eq!(owned[0].source, LocalPostSource::Owned);
    let page = b.discover(0).await.unwrap();
    assert_eq!(page[0].id, post.id);
    assert!(b.search_local_posts("Hello").await.is_empty());
    assert_eq!(
        b.download(&page[0], Some(Bookmark::Shared))
            .await
            .unwrap()
            .body,
        "Written by A"
    );
    let bookmarks = b.search_local_posts("written by a").await;
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].source, LocalPostSource::Bookmarked);
    assert_eq!(bookmarks[0].snippet, "Written by A");
    let former_address = a.invite();
    assert_eq!(
        former_address
            .to_string()
            .parse::<mesh_forum::forum::Invite>()
            .unwrap(),
        former_address
    );
    drop(a);
    let page = c.discover(0).await.unwrap();
    assert_eq!(page[0].author(), post.hint().author());
    let (progress, updates) = tokio::sync::watch::channel(Default::default());
    assert_eq!(
        c.download_with_progress(&page[0], None, Some(&progress))
            .await
            .unwrap()
            .body,
        "Written by A"
    );
    assert_eq!(updates.borrow().verified_bytes, "Written by A".len() as u64);
    assert_eq!(updates.borrow().total_bytes, "Written by A".len() as u64);
    assert_eq!(c.read(post.id).await.unwrap().unwrap().body, "Written by A");
    let cache = c.search_local_posts("Hello").await;
    assert_eq!(cache.len(), 1);
    assert_eq!(cache[0].source, LocalPostSource::Cached);
    assert_eq!(c.clear_cache().await.unwrap(), 1);
    assert!(c.search_local_posts("Hello").await.is_empty());
    assert_eq!(c.download(&page[0], None).await.unwrap().id, post.id);

    assert!(
        c.discover(0)
            .await
            .unwrap()
            .iter()
            .any(|hint| hint.id == post.id)
    );
    b.bookmark(post.id, Some(Bookmark::Local)).await.unwrap();
    assert!(c.discover(0).await.unwrap().is_empty());

    b.bookmark(post.id, Some(Bookmark::Shared)).await.unwrap();
    let restarted = Node::start(&root.path().join("a"), address, board)
        .await
        .unwrap();
    assert_eq!(restarted.invite().public_key, former_address.public_key);
    b.add_peer(restarted.invite()).await.unwrap();
    restarted.withdraw(post.id).await.unwrap();
    b.sync().await.unwrap();
    c.sync().await.unwrap();
    assert!(c.read(post.id).await.unwrap().is_none());
    assert!(c.search_local_posts("Hello").await.is_empty());
}

#[tokio::test]
async fn cache_preferences_survive_restart_and_cleanup_preserves_published_posts() {
    use mesh_forum::forum::CacheLimits;
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let node = Node::start(root.path(), address, [1; 32]).await.unwrap();
    let post = node.publish("Owned", "Keep").await.unwrap();
    let limits = CacheLimits {
        posts: 5,
        bytes: 512,
        idle_seconds: 60,
    };
    node.configure_cache(limits).await.unwrap();
    node.clear_cache().await.unwrap();
    assert!(node.read(post.id).await.unwrap().is_some());
    drop(node);
    let node = Node::start(root.path(), address, [1; 32]).await.unwrap();
    let storage = node.storage().await.unwrap();
    assert_eq!(storage.limits.posts, 5);
    assert_eq!(storage.limits.bytes, 512);
    assert_eq!(storage.limits.idle_seconds, 60);
    assert_eq!(storage.owned_posts, 1);
    assert_eq!(storage.cached_posts, 0);
}

#[tokio::test]
async fn verified_index_survives_restart_without_downloading_body() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [3; 32];
    let a = Node::start(&root.path().join("a"), address, board)
        .await
        .unwrap();
    let reader_path = root.path().join("reader");
    let b = Node::start(&reader_path, address, board).await.unwrap();
    a.add_peer(b.invite()).await.unwrap();
    b.add_peer(a.invite()).await.unwrap();
    let published = a.publish("Offline index", "Not downloaded").await.unwrap();
    assert_eq!(b.discover(0).await.unwrap()[0].id, published.id);
    drop(a);
    drop(b);
    let b = Node::start(&reader_path, address, board).await.unwrap();
    let cached = b.cached_index().await;
    assert_eq!(cached[0].id, published.id);
    assert_eq!(cached[0].author(), published.hint().author());
    assert!(b.read(published.id).await.unwrap().is_none());
}

#[tokio::test]
async fn discovery_pages_the_merged_board_across_sources() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [4; 32];
    let a = Node::start(&root.path().join("a"), address, board)
        .await
        .unwrap();
    let b = Node::start(&root.path().join("b"), address, board)
        .await
        .unwrap();
    let reader = Node::start(&root.path().join("reader"), address, board)
        .await
        .unwrap();
    for source in [&a, &b] {
        source.add_peer(reader.invite()).await.unwrap();
        reader.add_peer(source.invite()).await.unwrap();
    }
    for index in 0..16 {
        a.publish(&format!("A {index}"), "A").await.unwrap();
        b.publish(&format!("B {index}"), "B").await.unwrap();
    }
    let first = reader.discover(0).await.unwrap();
    let second = reader.discover(30).await.unwrap();
    assert_eq!(first.len(), 30);
    assert_eq!(second.len(), 2);
    assert!(
        second
            .iter()
            .all(|entry| first.iter().all(|first| first.id != entry.id))
    );
    assert_eq!(reader.cached_index().await.len(), 32);
}

#[tokio::test]
async fn paused_uploads_are_reachable_but_do_not_offer_content() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [5; 32];
    let origin_path = root.path().join("origin");
    let a = Node::start(&origin_path, address, board).await.unwrap();
    let b = Node::start(&root.path().join("reader"), address, board)
        .await
        .unwrap();
    a.add_peer(b.invite()).await.unwrap();
    b.add_peer(a.invite()).await.unwrap();
    let post = a.publish("Visible", "Body").await.unwrap();
    let hint = b.discover(0).await.unwrap().remove(0);
    a.set_uploads_paused(true).await.unwrap();
    assert_eq!(b.peer_status().await[0].sharing, Some(false));
    assert!(b.discover(0).await.unwrap().is_empty());
    assert!(b.download(&hint, None).await.is_err());
    assert!(a.read(post.id).await.unwrap().is_some());
    drop(a);
    let a = Node::start(&origin_path, address, board).await.unwrap();
    assert!(a.uploads_paused());
    b.add_peer(a.invite()).await.unwrap();
    a.set_uploads_paused(false).await.unwrap();
    assert_eq!(b.peer_status().await[0].sharing, Some(true));
    assert_eq!(b.download(&hint, None).await.unwrap().body, "Body");
}

#[tokio::test]
async fn lan_discovery_connects_and_downloads_without_manual_admission() {
    let root = tempfile::tempdir().unwrap();
    let port = std::net::UdpSocket::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let address = "0.0.0.0:0".parse().unwrap();
    let board = [17; 32];
    let mut a = Node::start(&root.path().join("a"), address, board)
        .await
        .unwrap();
    let mut b = Node::start(&root.path().join("b"), address, board)
        .await
        .unwrap();
    a.enable_lan_discovery(port).unwrap();
    b.enable_lan_discovery(port).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let a_seen = b
                .nearby_peers()
                .await
                .into_iter()
                .find(|peer| peer.public_key == a.invite().public_key);
            let b_seen = a
                .nearby_peers()
                .await
                .into_iter()
                .find(|peer| peer.public_key == b.invite().public_key);
            if a_seen.is_some() && b_seen.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert!(!root.path().join("a/peers").exists());
    assert!(!root.path().join("b/peers").exists());
    assert_eq!(a.peer_status().await[0].sharing, Some(true));
    let post = a.publish("LAN", "Found over UDP").await.unwrap();
    let entries = b.discover(0).await.unwrap();
    assert_eq!(entries[0].id, post.id);
    assert_eq!(
        b.download(&entries[0], None).await.unwrap().body,
        "Found over UDP"
    );
}

#[tokio::test]
async fn reconnect_fills_revision_gap_beyond_first_index_page() {
    use mesh_forum::forum::{RevisionStatus, group_revisions};
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [18; 32];
    let writer = Node::start(&root.path().join("writer"), address, board)
        .await
        .unwrap();
    let reader_path = root.path().join("reader");
    let reader = Node::start(&reader_path, address, board).await.unwrap();
    writer.add_peer(reader.invite()).await.unwrap();
    reader.add_peer(writer.invite()).await.unwrap();
    let original = writer.publish("Thread", "original").await.unwrap();
    reader.discover(0).await.unwrap();
    drop(reader);
    let second = writer
        .edit(original.id, "Thread", "intermediate")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    for index in 0..32 {
        writer
            .publish(&format!("Unrelated {index}"), "body")
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let latest = writer.edit(second.id, "Thread", "latest").await.unwrap();
    let reader = Node::start(&reader_path, address, board).await.unwrap();
    let page = reader.discover(0).await.unwrap();
    let hints = reader.cached_index().await;
    assert!(!page.iter().any(|hint| hint.id == second.id));
    let group = group_revisions(&hints)
        .into_iter()
        .find(|group| group.head.thread_id() == original.id)
        .unwrap();
    assert_eq!(group.status, RevisionStatus::Current);
    assert_eq!(group.head.id, latest.id);
}

#[tokio::test]
async fn guest_cannot_publish_and_account_unlock_restores_authority() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [19; 32];
    let guest = Node::start_guest(root.path(), address, board)
        .await
        .unwrap();
    assert!(guest.publish("Guest", "Forbidden").await.is_err());
    guest
        .create_account("Lin", "correct password")
        .await
        .unwrap();
    let post = guest.publish("Post", "Original").await.unwrap();
    guest.logout().await;
    assert!(guest.edit(post.id, "Post", "Forbidden").await.is_err());
    assert!(guest.login("wrong password").await.is_err());
    drop(guest);

    let guest = Node::start_guest(root.path(), address, board)
        .await
        .unwrap();
    guest.login("correct password").await.unwrap();
    let edit = guest.edit(post.id, "Post", "Revised").await.unwrap();
    assert_eq!(edit.hint().thread_id(), post.id);
    assert_eq!(edit.hint().author(), post.hint().author());
    assert_eq!(edit.hint().author_name(), Some("Lin"));
    assert_ne!(edit.hint().author(), guest.author_key());
    let reader = Node::start_guest(&root.path().join("reader"), address, board)
        .await
        .unwrap();
    reader.add_peer(guest.invite()).await.unwrap();
    guest.add_peer(reader.invite()).await.unwrap();
    let hint = reader
        .discover(0)
        .await
        .unwrap()
        .into_iter()
        .find(|hint| hint.id == edit.id)
        .unwrap();
    assert_eq!(hint.author_name(), Some("Lin"));
    assert_eq!(
        reader
            .download(&hint, Some(Bookmark::Shared))
            .await
            .unwrap()
            .body,
        "Revised"
    );
    assert!(reader.publish("Guest", "Forbidden").await.is_err());
    reader
        .create_account("Lin", "different password")
        .await
        .unwrap();
    assert!(
        reader
            .edit(edit.id, "Impersonated", "Forged")
            .await
            .is_err()
    );
    assert!(reader.withdraw(edit.id).await.is_err());
}

#[tokio::test]
async fn encrypted_account_backup_restores_authority_on_another_device() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [21; 32];
    let a = Node::start_guest(&root.path().join("a"), address, board)
        .await
        .unwrap();
    let b = Node::start_guest(&root.path().join("b"), address, board)
        .await
        .unwrap();
    let backup = root.path().join("account.mesh-account");
    a.create_account("Lin", "correct password").await.unwrap();
    a.export_account(&backup).await.unwrap();
    assert!(a.export_account(&backup).await.is_err());
    assert!(b.restore_account(&backup, "wrong password").await.is_err());
    assert!(!b.has_account());
    let original = a.publish("Original", "Original body").await.unwrap();
    a.add_peer(b.invite()).await.unwrap();
    b.add_peer(a.invite()).await.unwrap();
    let original_hint = b.discover(0).await.unwrap().remove(0);
    b.download(&original_hint, Some(Bookmark::Local))
        .await
        .unwrap();
    b.restore_account(&backup, "correct password")
        .await
        .unwrap();
    assert_ne!(a.invite().public_key, b.invite().public_key);
    assert_eq!(a.active_author().await, b.active_author().await);
    let edit = b
        .edit(original.id, "Edited", "Restored body")
        .await
        .unwrap();
    assert_eq!(edit.hint().thread_id(), original.id);
    assert_eq!(edit.hint().author(), original.hint().author());
    assert_eq!(a.discover(0).await.unwrap()[0].author_name(), Some("Lin"));
}

#[tokio::test]
async fn enrolling_prototype_author_keeps_old_post_id_and_removes_plaintext_key() {
    let root = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let board = [22; 32];
    let old = Node::start(root.path(), address, board).await.unwrap();
    let original = old.publish("Old thread", "Old body").await.unwrap();
    let old_edit = old
        .edit(original.id, "Old edit", "Old edit body")
        .await
        .unwrap();
    drop(old);

    let node = Node::start_guest(root.path(), address, board)
        .await
        .unwrap();
    assert_ne!(node.author_key(), original.hint().author());
    assert!(node.edit(old_edit.id, "Guest", "Forbidden").await.is_err());
    node.create_account("Lin", "correct password")
        .await
        .unwrap();
    assert!(!root.path().join("identity").exists());
    let updated = node
        .edit(old_edit.id, "Migrated", "New body")
        .await
        .unwrap();
    assert_eq!(updated.hint().thread_id(), original.id);
    assert_eq!(updated.hint().author(), original.hint().author());
    assert_eq!(updated.hint().author_name(), Some("Lin"));

    let guest_root = tempfile::tempdir().unwrap();
    let guest = Node::start_guest(guest_root.path(), address, board)
        .await
        .unwrap();
    node.add_peer(guest.invite()).await.unwrap();
    guest.add_peer(node.invite()).await.unwrap();
    let page = guest.discover(0).await.unwrap();
    assert_eq!(page.len(), 1);
    let cached = guest.cached_index().await;
    let groups = mesh_forum::forum::group_revisions(&cached);
    assert_eq!(groups[0].head.id, updated.id);
}
