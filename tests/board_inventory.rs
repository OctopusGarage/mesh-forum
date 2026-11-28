use mesh_connect::PeerSession;
use mesh_forum::forum::record::{ConversationId, Event, EventKind};
use mesh_identity::device::DeviceIdentity;
use mesh_replication::{
    CompleteInventory,
    inventory_session::{InventorySender, receive_inventory, send_inventory},
    pull::{PullLimits, Record, pull_over_session, serve_record_request},
    session::HaveLimits,
};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use x25519_dalek::{PublicKey, StaticSecret};

fn peer_key(secret: [u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(secret)).to_bytes()
}

fn limits() -> HaveLimits {
    HaveLimits {
        ids_per_frame: 1,
        frames: 2,
        total_ids: 2,
    }
}

#[tokio::test]
async fn reader_pulls_and_persists_the_missing_board_post() {
    let board = ConversationId::new([7; 32]);
    let author = DeviceIdentity::generate();
    let first = Event::new(
        &author,
        board,
        1,
        vec![],
        1,
        0,
        EventKind::Post,
        b"First thread".to_vec(),
    );
    let posted = Event::new(
        &author,
        board,
        2,
        vec![first.id],
        2,
        0,
        EventKind::Post,
        b"New thread during partition".to_vec(),
    );
    let original = *first.id.as_bytes();
    let offline_post = *posted.id.as_bytes();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let source_secret = [11; 32];
    let reader_secret = [22; 32];
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            async {
                let stream = TcpStream::connect(address).await.unwrap();
                let mut peer =
                    PeerSession::connect(stream, &reader_secret, &peer_key(source_secret))
                        .await
                        .unwrap();
                let scope = *board.as_bytes();
                let local = CompleteInventory::from_ids(&[original], 2).unwrap();
                send_inventory(
                    &mut peer,
                    scope,
                    InventorySender::Requester,
                    &local,
                    limits(),
                    || Ok(()),
                )
                .await
                .unwrap();
                let remote =
                    receive_inventory(&mut peer, scope, InventorySender::Responder, limits())
                        .await
                        .unwrap();
                let directory = tempfile::tempdir().unwrap();
                let log_path = directory.path().join("board");
                let (mut log, existing) =
                    mesh_storage::record_log::EncryptedRecordLog::<Event>::open(
                        &log_path,
                        "test password",
                        b"FORUM1",
                    )
                    .unwrap();
                assert!(existing.is_empty());
                log.append_durable(&first).unwrap();

                let applied = pull_over_session(
                    &mut peer,
                    *board.as_bytes(),
                    &remote,
                    &local,
                    PullLimits {
                        records_per_batch: 1,
                        bytes_per_batch: 4096,
                    },
                    |records| -> Result<(), Box<dyn std::error::Error>> {
                        let events = records
                            .iter()
                            .map(|record| {
                                let event: Event = bincode::deserialize(&record.body)?;
                                if event.id.as_bytes() != &record.id
                                    || event.conversation_id != board
                                    || !event.verify_integrity()
                                    || !event.verify_signature()
                                    || event.author.ed25519_pub() != &author.public().ed25519_pub
                                {
                                    return Err("invalid or unauthorized board record".into());
                                }
                                Ok(event)
                            })
                            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
                        log.append_batch_atomic(&events)?;
                        Ok(())
                    },
                )
                .await
                .unwrap();
                assert_eq!(applied, 1);
                drop(log);
                let (_, records) = mesh_storage::record_log::EncryptedRecordLog::<Event>::open(
                    &log_path,
                    "test password",
                    b"FORUM1",
                )
                .unwrap();
                assert_eq!(records.as_slice(), &[first.clone(), posted.clone()]);
            },
            async {
                let (stream, _) = listener.accept().await.unwrap();
                let mut peer = PeerSession::accept(stream, &source_secret, |key| {
                    *key == peer_key(reader_secret)
                })
                .await
                .unwrap();
                let scope = *board.as_bytes();
                let have =
                    receive_inventory(&mut peer, scope, InventorySender::Requester, limits())
                        .await
                        .unwrap();
                let expected = CompleteInventory::from_ids(&[original], 2).unwrap();
                assert_eq!(have.fingerprint(), expected.fingerprint());
                let local = CompleteInventory::from_ids(&[original, offline_post], 2).unwrap();
                send_inventory(
                    &mut peer,
                    scope,
                    InventorySender::Responder,
                    &local,
                    limits(),
                    || Ok(()),
                )
                .await
                .unwrap();
                serve_record_request(
                    &mut peer,
                    *board.as_bytes(),
                    PullLimits {
                        records_per_batch: 1,
                        bytes_per_batch: 4096,
                    },
                    async |key, ids, budget| {
                        assert_eq!(*key, peer_key(reader_secret));
                        assert_eq!(ids, &[offline_post]);
                        let body = bincode::serialize(&posted).unwrap();
                        assert!(body.len() <= budget);
                        Ok::<_, std::convert::Infallible>(vec![Record {
                            id: offline_post,
                            body,
                        }])
                    },
                )
                .await
                .unwrap();
            }
        );
    })
    .await
    .unwrap();
}
