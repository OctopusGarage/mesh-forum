use mesh_blob::{
    BlobLayout, BlobVerifier, FileKey, file_checksum, generate_file_nonce, open_chunk_indexed,
    seal_chunk_indexed,
};
use mesh_connect::{PeerSession, net::bind_dual_stack_listener};
use mesh_e2ee::ratchet::{Header, init_alice, init_bob};
use mesh_identity::{account::Account, device::DeviceIdentity};
use mesh_mailbox::{
    Record, ScopeId,
    capacity::CapacityLimits,
    rpc::{Frame, encode_frame, make_get_request, read_delta_response, read_get_response},
    service::{dispatch, make_probe, probe_scope, read_probe_response},
    spool::{Deposit, Spool},
};
use mesh_replication::Inventory;
use mesh_storage::{EncryptionKey, NONCE_SIZE, decrypt_data, encrypt_data};
use std::{num::NonZeroU64, time::Duration};
use tokio::net::TcpStream;

#[tokio::test]
async fn authorized_reader_recovers_a_private_post_from_an_opaque_relay_copy() {
    let author = DeviceIdentity::generate();
    let reader = DeviceIdentity::generate();
    let relay = DeviceIdentity::generate();
    let account = Account::generate();
    let cert = account.certify(&author.public().ed25519_pub);
    assert!(cert.verify());

    // Fixed test-only shared roots and ratchet keys. A deployed application has
    // to establish and persist them through its authenticated key lifecycle.
    let mut reader_chain = init_bob(&[17; 32], [19; 32]);
    let mut author_chain = init_alice(&[17; 32], &reader_chain.ratchet_public());
    let (header, body) = author_chain.ratchet_encrypt(b"private thread").unwrap();
    let file_key = FileKey::generate();
    let file_nonce = generate_file_nonce();
    let attachment = [
        seal_chunk_indexed(&file_key, &file_nonce, 0, b"attach").unwrap(),
        seal_chunk_indexed(&file_key, &file_nonce, 1, b"ment").unwrap(),
    ];
    let payload = bincode::serialize(&(header.encode(), body, attachment)).unwrap();
    // The forum owns its signed payload schema; the SDK stores opaque bytes.
    let scope = ScopeId::new([3; 32]);
    let mut signed = b"mesh-forum-private-post-v1\0".to_vec();
    signed.extend_from_slice(scope.as_bytes());
    signed.extend_from_slice(&payload);
    let signature = author.sign(&signed).to_vec();
    // Test-only out-of-band recipient key. The host owns envelope encryption;
    // neither this secret nor the author's signed schema is exposed to the relay.
    let outer_key = EncryptionKey::from_derived_key([43; 32]);
    let inner = bincode::serialize(&(author.public().ed25519_pub, payload, signature)).unwrap();
    let (ciphertext, nonce) = encrypt_data(&inner, &outer_key).unwrap();
    let mut damaged = ciphertext.clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert!(decrypt_data(&damaged, &nonce, &outer_key).is_err());
    let outer = Record::new(scope, bincode::serialize(&(nonce, ciphertext)).unwrap());
    assert!(!outer.body.windows(inner.len()).any(|bytes| bytes == inner));
    assert!(
        !outer
            .body
            .windows(32)
            .any(|bytes| bytes == author.public().ed25519_pub)
    );
    let post_id = *outer.id.as_bytes();
    assert!(outer.verify_integrity());

    let storage_dir = tempfile::tempdir().unwrap();
    let path = storage_dir.path().join("mailbox");
    let password = "integration-test-only";
    let limits = CapacityLimits::new(1, 0, 65_536, 0).unwrap();
    let max_age = NonZeroU64::new(60).unwrap();
    {
        let mut spool = Spool::open(&path, password, scope, limits, max_age).unwrap();
        assert_eq!(spool.deposit(outer.clone(), 1).unwrap(), Deposit::Stored);
    }
    let spool = Spool::open(&path, password, scope, limits, max_age).unwrap();
    assert!(spool.durable_have(&outer.id, 2));

    let listener = bind_dual_stack_listener(0).await.unwrap();
    let address = (
        std::net::Ipv4Addr::LOCALHOST,
        listener.local_addr().unwrap().port(),
    );
    let reader_secret = reader.secret_bytes().1;
    let relay_secret = relay.secret_bytes().1;
    let reader_key = reader.public().x25519_pub;
    let relay_key = relay.public().x25519_pub;
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            async {
                let mut peer = PeerSession::connect(
                    TcpStream::connect(address).await.unwrap(),
                    &reader_secret,
                    &relay_key,
                )
                .await
                .unwrap();
                negotiate_mailbox(&mut peer, scope).await;
                peer.send(
                    &encode_frame(&Frame::Delta {
                        scope,
                        prior: None,
                        limit: mesh_mailbox::rpc::PAGE_IDS as u16,
                        max_bytes: mesh_mailbox::rpc::BODY_BUDGET as u32,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
                let page = read_delta_response(&peer.recv().await.unwrap(), scope).unwrap();
                assert!(page.complete && !page.reset);
                assert_eq!(page.ids.len(), 1);
                drop(peer);

                // Resume body retrieval on a freshly authenticated socket.
                let mut peer = PeerSession::connect(
                    TcpStream::connect(address).await.unwrap(),
                    &reader_secret,
                    &relay_key,
                )
                .await
                .unwrap();
                negotiate_mailbox(&mut peer, scope).await;
                peer.send(&make_get_request(scope, &page.cursor, &page.ids).unwrap())
                    .await
                    .unwrap();
                let mut events =
                    read_get_response(&peer.recv().await.unwrap(), scope, &page.ids).unwrap();
                let outer = events.pop().unwrap();
                assert_eq!(outer.id.as_bytes(), &post_id);
                assert!(outer.verify_integrity());
                let (nonce, ciphertext): ([u8; NONCE_SIZE], Vec<u8>) =
                    bincode::deserialize(&outer.body).unwrap();
                let opened = decrypt_data(&ciphertext, &nonce, &outer_key).unwrap();
                let (signer, payload, signature): ([u8; 32], Vec<u8>, Vec<u8>) =
                    bincode::deserialize(&opened).unwrap();
                assert_eq!(signer, cert.device_ed25519_pub);
                let mut signed = b"mesh-forum-private-post-v1\0".to_vec();
                signed.extend_from_slice(scope.as_bytes());
                signed.extend_from_slice(&payload);
                assert!(DeviceIdentity::verify(
                    &signer,
                    &signed,
                    &signature.try_into().unwrap()
                ));
                let mut remote = Inventory::new(1, 1);
                remote.add_page(&[*outer.id.as_bytes()]).unwrap();
                let empty = Inventory::new(1, 1).finish().unwrap();
                assert_eq!(
                    remote
                        .finish()
                        .unwrap()
                        .missing_from(&empty)
                        .collect::<Vec<_>>(),
                    [post_id]
                );

                let (header, body, attachment): (Vec<u8>, Vec<u8>, [Vec<u8>; 2]) =
                    bincode::deserialize(&payload).unwrap();
                assert_eq!(
                    reader_chain
                        .ratchet_decrypt(&Header::decode(&header).unwrap(), &body)
                        .unwrap(),
                    b"private thread"
                );
                let layout = BlobLayout::new(10, 6, 10).unwrap();
                let mut verified = BlobVerifier::for_layout(layout, file_checksum(b"attachment"));
                let mut bytes = Vec::new();
                for (index, ciphertext) in attachment.iter().enumerate() {
                    let index = u32::try_from(index).unwrap();
                    let chunk =
                        open_chunk_indexed(&file_key, &file_nonce, index, ciphertext).unwrap();
                    verified.push(index, &chunk).unwrap();
                    bytes.extend_from_slice(&chunk);
                }
                verified.finish().unwrap();
                assert_eq!(bytes, b"attachment");
            },
            async {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut peer =
                        PeerSession::accept(stream, &relay_secret, |key| *key == reader_key)
                            .await
                            .unwrap();
                    let probe = peer.recv().await.unwrap();
                    assert_eq!(probe_scope(&probe), Some(scope));
                    peer.send(&encode_frame(&Frame::Supported { scope }).unwrap())
                        .await
                        .unwrap();
                    let request = peer.recv().await.unwrap();
                    let reply = dispatch(&spool.view(2), Some(scope), &request).unwrap();
                    peer.send(&reply.bytes).await.unwrap();
                }
            }
        );
    })
    .await
    .unwrap();
}

async fn negotiate_mailbox(peer: &mut PeerSession<TcpStream>, scope: ScopeId) {
    peer.send(&make_probe(scope).unwrap()).await.unwrap();
    assert!(read_probe_response(&peer.recv().await.unwrap(), scope).unwrap());
}
