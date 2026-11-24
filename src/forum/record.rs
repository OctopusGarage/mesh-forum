//! The canonical, content-addressed event. `id` is the SHA-256 hash of the
//! event's content (everything except `id` and `sig`); `sig` is the author's
//! Ed25519 signature over `id`. Hash-linked via `parents`, carrying a Lamport
//! clock and a per-author sequence number.

use mesh_identity::device::{DeviceIdentity, PublicIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Historical forum v1 hash domain, frozen to preserve existing persisted IDs.
/// The original SDK supplied this spelling; ownership now belongs to the forum.
const EVENT_DOMAIN: &[u8] = b"mesh-talk-event-v1";

/// 32-byte content hash identifying an event.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId([u8; 32]);

impl EventId {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EventId({}…)", &self.to_hex()[..8])
    }
}

/// 32-byte opaque forum board identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ConversationId([u8; 32]);

impl ConversationId {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// An event author, identified by their Ed25519 public key (self-certifying).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author([u8; 32]);

impl Author {
    pub fn from_ed25519(ed25519_pub: [u8; 32]) -> Self {
        Self(ed25519_pub)
    }
    pub fn ed25519_pub(&self) -> &[u8; 32] {
        &self.0
    }
    /// The stable user-id fingerprint of this author (hex, 32 chars).
    pub fn user_id(&self) -> String {
        PublicIdentity::node_id_from(&self.0)
    }
}

impl std::fmt::Debug for Author {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Author({}…)", &hex::encode(self.0)[..8])
    }
}

/// Forum-owned fixed wire indices. Other application event kinds are rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum EventKind {
    Post = 0,
    Revision = 1,
    Withdrawal = 2,
}

/// A single, content-addressed, signed log event.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub conversation_id: ConversationId,
    pub author: Author,
    pub seq: u64,
    pub parents: Vec<EventId>,
    pub lamport: u64,
    pub wall_clock: u64,
    pub kind: EventKind,
    pub ciphertext: Vec<u8>,
    pub sig: Vec<u8>,
}

/// The hashed content of an event (everything that fixes its identity). Held by
/// reference so we never copy the ciphertext to compute the id.
#[derive(Serialize)]
struct EventContent<'a> {
    conversation_id: &'a ConversationId,
    author: &'a Author,
    seq: u64,
    parents: &'a [EventId],
    lamport: u64,
    wall_clock: u64,
    kind: EventKind,
    ciphertext: &'a [u8],
}

fn hash_content(content: &EventContent) -> EventId {
    let bytes = bincode::serialize(content).expect("event content is serializable");
    let mut hasher = Sha256::new();
    hasher.update(EVENT_DOMAIN);
    hasher.update(&bytes);
    EventId(hasher.finalize().into())
}

/// The domain-separated message the author signs: `EVENT_DOMAIN || id`.
fn signing_input(id: &EventId) -> Vec<u8> {
    let mut input = Vec::with_capacity(EVENT_DOMAIN.len() + 32);
    input.extend_from_slice(EVENT_DOMAIN);
    input.extend_from_slice(id.as_bytes());
    input
}

impl Event {
    /// Build a fresh event: canonicalize parents, compute the content-hash id,
    /// and sign that id with the author's Ed25519 key.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: &DeviceIdentity,
        conversation_id: ConversationId,
        seq: u64,
        mut parents: Vec<EventId>,
        lamport: u64,
        wall_clock: u64,
        kind: EventKind,
        ciphertext: Vec<u8>,
    ) -> Self {
        // Canonical parent order so the same logical event always hashes equal.
        parents.sort();
        parents.dedup();
        let author = Author::from_ed25519(identity.public().ed25519_pub);
        let id = hash_content(&EventContent {
            conversation_id: &conversation_id,
            author: &author,
            seq,
            parents: &parents,
            lamport,
            wall_clock,
            kind,
            ciphertext: &ciphertext,
        });
        let sig = identity.sign(&signing_input(&id)).to_vec();
        Event {
            id,
            conversation_id,
            author,
            seq,
            parents,
            lamport,
            wall_clock,
            kind,
            ciphertext,
            sig,
        }
    }

    /// Recompute the content-hash id from the event's own fields.
    pub fn recompute_id(&self) -> EventId {
        hash_content(&EventContent {
            conversation_id: &self.conversation_id,
            author: &self.author,
            seq: self.seq,
            parents: &self.parents,
            lamport: self.lamport,
            wall_clock: self.wall_clock,
            kind: self.kind,
            ciphertext: &self.ciphertext,
        })
    }

    /// True if `id` matches the hash of the content (tamper-evidence).
    /// Does not check parent canonicalization; see [`Event::is_canonical`].
    pub fn verify_integrity(&self) -> bool {
        self.id == self.recompute_id()
    }

    /// True if `sig` is a valid Ed25519 signature over `id` by `author`.
    pub fn verify_signature(&self) -> bool {
        let Ok(sig): Result<[u8; 64], _> = self.sig.as_slice().try_into() else {
            return false;
        };
        DeviceIdentity::verify(self.author.ed25519_pub(), &signing_input(&self.id), &sig)
    }

    /// True if `parents` is already canonical (sorted, de-duplicated) — the form
    /// [`Event::new`] produces. A validly-signed event with non-canonical parents
    /// hashes differently from its canonical equivalent (a phantom fork), so the
    /// store layer rejects non-canonical events on ingest.
    pub fn is_canonical(&self) -> bool {
        let mut expected = self.parents.clone();
        expected.sort();
        expected.dedup();
        self.parents == expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forum_record_authenticates_and_rejects_foreign_event_kinds() {
        let signer = DeviceIdentity::from_secret_bytes([7; 32], [9; 32]);
        let event = Event::new(
            &signer,
            ConversationId::new([1; 32]),
            1,
            vec![],
            1,
            0,
            EventKind::Post,
            b"forum post".to_vec(),
        );
        assert!(event.verify_integrity() && event.verify_signature());
        let mut changed = event.clone();
        changed.ciphertext.push(0);
        assert!(!changed.verify_integrity());
        changed.id = changed.recompute_id();
        assert!(!changed.verify_signature());
        for unsupported in 3u32..20 {
            assert!(bincode::deserialize::<EventKind>(&unsupported.to_le_bytes()).is_err());
        }
        for (kind, tag) in [
            (EventKind::Post, 0u32),
            (EventKind::Revision, 1),
            (EventKind::Withdrawal, 2),
        ] {
            assert_eq!(bincode::serialize(&kind).unwrap(), tag.to_le_bytes());
        }
    }
}
