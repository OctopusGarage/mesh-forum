use mesh_blob::{chunk_hash, file_checksum};
use mesh_connect::PeerSession;
use mesh_forum::{
    PostBodyManifest, SessionChunkSource, download_post_body, serve_post_body_request,
};
use mesh_identity::device::DeviceIdentity;
use std::{num::NonZeroUsize, time::Duration};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn post_revision_resumes_from_another_authenticated_peer() {
    let staging = tempfile::tempdir().unwrap();
    let hashes = [chunk_hash(b"ab"), chunk_hash(b"cd")];
    let checksum = file_checksum(b"abcd");
    let manifest = || PostBodyManifest {
        bytes: 4,
        chunk_bytes: 2,
        checksum,
        chunk_hashes: &hashes,
    };
    let concurrency = NonZeroUsize::new(2).unwrap();
    let reader = DeviceIdentity::generate();
    let reader_key = reader.public().x25519_pub;
    for (held_index, content) in [(1, b"cd"), (0, b"ab")] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = DeviceIdentity::generate();
        let provider_key = provider.public().x25519_pub;
        let (download, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                async {
                    let socket = TcpStream::connect(address).await.unwrap();
                    let session =
                        PeerSession::connect(socket, &reader.secret_bytes().1, &provider_key)
                            .await
                            .unwrap();
                    let source = SessionChunkSource::discover(
                        checksum,
                        2,
                        2,
                        session,
                        Duration::from_secs(2),
                    )
                    .await
                    .unwrap();
                    download_post_body(manifest(), staging.path(), &[source], 4, 2, concurrency)
                        .await
                },
                async {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut session =
                        PeerSession::accept(socket, &provider.secret_bytes().1, |peer| {
                            *peer == reader_key
                        })
                        .await
                        .unwrap();
                    for _ in 0..2 {
                        serve_post_body_request(
                            &mut session,
                            |peer, object, index, length| {
                                assert_eq!(
                                    (*peer, object, index, length),
                                    (reader_key, checksum, held_index, 2)
                                );
                                Some(content.to_vec())
                            },
                            |peer, object, start, length| {
                                assert_eq!(
                                    (*peer, object, start, length),
                                    (reader_key, checksum, 0, 2)
                                );
                                Some((0..2).map(|index| index == held_index).collect())
                            },
                        )
                        .await
                        .unwrap();
                    }
                }
            )
        })
        .await
        .unwrap();
        assert_eq!(download.is_ok(), held_index == 0);
    }
    assert_eq!(std::fs::read(staging.path().join("0")).unwrap(), b"ab");
    assert_eq!(std::fs::read(staging.path().join("1")).unwrap(), b"cd");
}

#[tokio::test]
async fn post_revision_combines_complementary_sources_in_one_download() {
    let staging = tempfile::tempdir().unwrap();
    let reader = DeviceIdentity::generate();
    let reader_key = reader.public().x25519_pub;
    const BLOCK_BYTES: u32 = 32 * 1024;
    let chunks = std::sync::Arc::new([
        vec![0x11; BLOCK_BYTES as usize],
        vec![0x22; BLOCK_BYTES as usize],
        vec![0x33; BLOCK_BYTES as usize],
        vec![0x44; 87],
    ]);
    let body: Vec<u8> = chunks.iter().flatten().copied().collect();
    let bytes = body.len() as u64;
    let checksum = file_checksum(&body);
    let hashes = std::array::from_fn::<_, 4, _>(|index| chunk_hash(&chunks[index]));
    let mut servers = tokio::task::JoinSet::new();
    let mut sources = Vec::new();

    for lane in 0..2 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = DeviceIdentity::generate();
        let provider_key = provider.public().x25519_pub;
        let served_chunks = std::sync::Arc::clone(&chunks);
        servers.spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut session = PeerSession::accept(socket, &provider.secret_bytes().1, |peer| {
                *peer == reader_key
            })
            .await
            .unwrap();
            for round in 0..3 {
                if lane == 0 && round > 0 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                serve_post_body_request(
                    &mut session,
                    |peer, object, index, length| {
                        assert_eq!((*peer, object), (reader_key, checksum));
                        assert_eq!(index % 2, lane);
                        let chunk = &served_chunks[index as usize];
                        assert_eq!(length as usize, chunk.len());
                        Some(chunk.clone())
                    },
                    |peer, object, start, length| {
                        assert_eq!((*peer, object, start, length), (reader_key, checksum, 0, 4));
                        Some((0..4).map(|index| index % 2 == lane).collect())
                    },
                )
                .await
                .unwrap();
            }
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let session = PeerSession::connect(stream, &reader.secret_bytes().1, &provider_key)
            .await
            .unwrap();
        sources.push(
            SessionChunkSource::discover(checksum, 4, 4, session, Duration::from_secs(2))
                .await
                .unwrap(),
        );
    }

    download_post_body(
        PostBodyManifest {
            bytes,
            chunk_bytes: BLOCK_BYTES,
            checksum,
            chunk_hashes: &hashes,
        },
        staging.path(),
        &sources,
        bytes,
        4,
        NonZeroUsize::new(4).unwrap(),
    )
    .await
    .unwrap();
    while let Some(result) = servers.join_next().await {
        result.unwrap();
    }
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(
            std::fs::read(staging.path().join(index.to_string())).unwrap(),
            *chunk
        );
    }
}
