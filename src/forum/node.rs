//! Desktop-independent runtime for a manually connected forum node.

use super::board_index::{INDEX_LIMIT, PAGE_SIZE};
use super::index_cache::{self, IndexCache};
use super::network::{fetch_revision, fetch_withdrawals, probe, serve_with_policy};
use super::{
    Bookmark, CacheLimits, Forum, Post, PostHint, PostId, Retention, Staging, StagingLimits, codec,
    fetch_index,
};
use bincode::Options;
use mesh_connect::PeerSession;
use mesh_identity::device::DeviceIdentity;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, Semaphore},
    task::JoinHandle,
    time::{interval, timeout},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalPostSource {
    Owned,
    Bookmarked,
    Cached,
}

#[derive(Clone)]
pub struct LocalSearchHit {
    pub hint: PostHint,
    pub source: LocalPostSource,
    pub snippet: String,
}

const PEER_LIMIT: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    pub address: SocketAddr,
    pub public_key: [u8; 32],
}

impl std::fmt::Display for Invite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}@", self.address)?;
        for byte in self.public_key {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Invite {
    type Err = io::Error;

    fn from_str(value: &str) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an address and a 64-character public key",
            )
        };
        let (address, key) = value.trim().split_once('@').ok_or_else(invalid)?;
        let address = address.parse().map_err(|_| invalid())?;
        let mut public_key = [0; 32];
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        for (index, byte) in public_key.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&key[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self {
            address,
            public_key,
        })
    }
}

struct State {
    identity: DeviceIdentity,
    author: RwLock<Option<super::account::Unlocked>>,
    legacy_author: bool,
    root: std::path::PathBuf,
    board: [u8; 32],
    forum: RwLock<Forum>,
    staging: Mutex<Staging>,
    peers: RwLock<Vec<Invite>>,
    nearby: super::lan::Candidates,
    index: Mutex<IndexCache>,
    paused: tokio::sync::watch::Sender<bool>,
    pause_write: Mutex<()>,
    sync_cursors: Mutex<std::collections::BTreeMap<[u8; 32], PostId>>,
}

impl State {
    async fn active_peers(&self) -> Vec<Invite> {
        let mut peers: std::collections::BTreeMap<_, _> = self
            .peers
            .read()
            .await
            .iter()
            .map(|invite| (invite.public_key, *invite))
            .collect();
        for invite in super::lan::active(&self.nearby).await {
            peers.insert(invite.public_key, invite);
        }
        peers.into_values().collect()
    }
}

pub struct PeerStatus {
    pub invite: Invite,
    /// None means the authenticated forum probe could not reach the peer.
    pub sharing: Option<bool>,
}

pub struct StorageUsage {
    pub limits: CacheLimits,
    pub owned_posts: usize,
    pub owned_bytes: u64,
    pub bookmarked_posts: usize,
    pub bookmarked_bytes: u64,
    pub cached_posts: usize,
    pub cached_bytes: u64,
    pub partial_bytes: Option<u64>,
}

pub struct Node {
    state: Arc<State>,
    address: SocketAddr,
    listener: JoinHandle<()>,
    maintenance: JoinHandle<()>,
    lan: Option<JoinHandle<()>>,
}

impl Node {
    pub async fn start(root: &Path, listen: SocketAddr, board: [u8; 32]) -> io::Result<Self> {
        Self::start_inner(root, listen, board, true).await
    }

    pub async fn start_guest(root: &Path, listen: SocketAddr, board: [u8; 32]) -> io::Result<Self> {
        Self::start_inner(root, listen, board, false).await
    }

    async fn start_inner(
        root: &Path,
        listen: SocketAddr,
        board: [u8; 32],
        legacy_author: bool,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(listen).await?;
        let address = listener.local_addr()?;
        let limits = match read_metadata(root, "cache-settings", 64)? {
            Some(bytes) => codec().deserialize(&bytes).map_err(io::Error::other)?,
            None => CacheLimits::default(),
        };
        let forum = Forum::open_with_cache(root, limits, now())?;
        let identity_file = if legacy_author {
            "identity"
        } else {
            "connection-identity"
        };
        let identity = match read_metadata(root, identity_file, 64)? {
            Some(bytes) => {
                let (signing, transport): ([u8; 32], [u8; 32]) =
                    codec().deserialize(&bytes).map_err(io::Error::other)?;
                DeviceIdentity::from_secret_bytes(signing, transport)
            }
            None => {
                let identity = DeviceIdentity::generate();
                let bytes = codec()
                    .serialize(&identity.secret_bytes())
                    .map_err(io::Error::other)?;
                forum.persist(identity_file, &bytes)?;
                identity
            }
        };
        let peers = match read_metadata(root, "peers", 8192)? {
            Some(bytes) => codec().deserialize(&bytes).map_err(io::Error::other)?,
            None => Vec::new(),
        };
        if peers.len() > PEER_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many stored peers",
            ));
        }
        let paused = match read_metadata(root, "uploads-paused", 1)? {
            Some(bytes) => codec().deserialize(&bytes).map_err(io::Error::other)?,
            None => false,
        };
        let state = Arc::new(State {
            identity,
            author: RwLock::new(None),
            legacy_author,
            root: root.to_owned(),
            board,
            forum: RwLock::new(forum),
            staging: Mutex::new(Staging::open(
                &root.join("downloads"),
                StagingLimits::default(),
            )?),
            peers: RwLock::new(peers),
            nearby: Default::default(),
            index: Mutex::new(IndexCache::open(root, board)?),
            sync_cursors: Mutex::default(),
            paused: tokio::sync::watch::channel(paused).0,
            pause_write: Mutex::default(),
        });
        let listener = tokio::spawn(listen_for_requests(listener, Arc::clone(&state)));
        let maintenance = tokio::spawn(maintain(Arc::clone(&state)));
        Ok(Self {
            state,
            address,
            listener,
            maintenance,
            lan: None,
        })
    }

    pub fn enable_lan_discovery(&mut self, port: u16) -> io::Result<()> {
        if self.address.is_ipv4() && self.address.ip().is_unspecified() && port != 0 {
            if self.lan.is_none() {
                self.lan = Some(super::lan::start(
                    &self.state.identity,
                    self.state.board,
                    self.address.port(),
                    port,
                    Arc::clone(&self.state.nearby),
                )?);
            }
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LAN discovery needs an IPv4 wildcard listener and nonzero UDP port",
        ))
    }

    pub async fn nearby_peers(&self) -> Vec<Invite> {
        super::lan::active(&self.state.nearby).await
    }

    pub fn invite(&self) -> Invite {
        let mut address = self.address;
        if address.ip().is_unspecified() {
            let local = mesh_connect::net::ipv4_interface_addrs()
                .into_iter()
                .find(|address| !address.is_loopback())
                .unwrap_or(std::net::Ipv4Addr::LOCALHOST);
            address.set_ip(local.into());
        }
        Invite {
            address,
            public_key: self.state.identity.public().x25519_pub,
        }
    }

    pub fn uploads_paused(&self) -> bool {
        *self.state.paused.borrow()
    }

    pub async fn set_uploads_paused(&self, paused: bool) -> io::Result<()> {
        let _write = self.state.pause_write.lock().await;
        if self.uploads_paused() != paused {
            self.state
                .forum
                .read()
                .await
                .persist("uploads-paused", &[u8::from(paused)])?;
            self.state.paused.send_replace(paused);
        }
        Ok(())
    }

    pub async fn peer_status(&self) -> Vec<PeerStatus> {
        let mut tasks = tokio::task::JoinSet::new();
        for invite in self.state.active_peers().await {
            let state = Arc::clone(&self.state);
            tasks.spawn(async move {
                let sharing = timeout(Duration::from_secs(3), async {
                    let mut session = dial(&state, invite).await?;
                    probe(&mut session, state.board).await
                })
                .await
                .ok()
                .and_then(Result::ok);
                PeerStatus { invite, sharing }
            });
        }
        let mut statuses = Vec::new();
        while let Some(Ok(status)) = tasks.join_next().await {
            statuses.push(status);
        }
        statuses.sort_unstable_by_key(|status| status.invite.address);
        statuses
    }

    pub async fn add_peer(&self, invite: Invite) -> io::Result<()> {
        if invite.public_key == self.invite().public_key
            || invite.address.port() == 0
            || invite.address.ip().is_unspecified()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid peer invite",
            ));
        }
        let mut peers = self.state.peers.write().await;
        let mut next = peers.clone();
        next.retain(|peer| peer.public_key != invite.public_key);
        if next.len() == PEER_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "peer limit reached",
            ));
        }
        next.push(invite);
        let bytes = codec().serialize(&next).map_err(io::Error::other)?;
        self.state.forum.read().await.persist("peers", &bytes)?;
        *peers = next;
        Ok(())
    }

    pub fn has_account(&self) -> bool {
        super::account::exists(&self.state.root)
    }

    pub async fn account_name(&self) -> Option<String> {
        self.state
            .author
            .read()
            .await
            .as_ref()
            .map(|author| author.name.clone())
    }

    pub async fn create_account(&self, name: &str, password: &str) -> io::Result<()> {
        self.unlock_account(Some(name.to_owned()), password).await
    }

    pub async fn login(&self, password: &str) -> io::Result<()> {
        self.unlock_account(None, password).await
    }

    async fn unlock_account(&self, name: Option<String>, password: &str) -> io::Result<()> {
        let mut author = self.state.author.write().await;
        let root = self.state.root.clone();
        let transport = self.state.identity.secret_bytes().1;
        let password = zeroize::Zeroizing::new(password.to_owned());
        let prototype = if self.state.legacy_author {
            None
        } else {
            read_metadata(&root, "identity", 64)?
                .map(|bytes| {
                    let bytes = zeroize::Zeroizing::new(bytes);
                    codec()
                        .deserialize::<([u8; 32], [u8; 32])>(&bytes)
                        .map(zeroize::Zeroizing::new)
                        .map_err(io::Error::other)
                })
                .transpose()?
        };
        let adopt = if let Some(prototype) = &prototype {
            let former = DeviceIdentity::from_secret_bytes(prototype.0, prototype.1);
            let key = former.public().ed25519_pub;
            self.state
                .forum
                .read()
                .await
                .posts
                .values()
                .any(|(post, retention, _)| {
                    matches!(retention, Retention::Owned) && post.hint().author() == key
                })
        } else {
            false
        };
        let opened = tokio::task::spawn_blocking(move || {
            let opened = match name {
                Some(name) => {
                    let legacy_seed = prototype
                        .as_ref()
                        .filter(|_| adopt)
                        .map(|keys| zeroize::Zeroizing::new(keys.0));
                    super::account::create(&root, &name, &password, transport, legacy_seed)?
                }
                None => super::account::open(&root, &password, transport)?,
            };
            if let Some(prototype) = prototype
                && opened.signer.public().ed25519_pub
                    == DeviceIdentity::from_secret_bytes(prototype.0, prototype.1)
                        .public()
                        .ed25519_pub
            {
                match fs::remove_file(root.join("identity")) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok::<_, io::Error>(opened)
        })
        .await
        .map_err(io::Error::other)??;
        *author = Some(opened);
        Ok(())
    }

    pub async fn export_account(&self, destination: &Path) -> io::Result<()> {
        let author = self.state.author.read().await;
        if author.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "请先登录再备份账号",
            ));
        }
        let root = self.state.root.clone();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || super::account::export(&root, &destination))
            .await
            .map_err(io::Error::other)?
    }

    pub async fn restore_account(&self, source: &Path, password: &str) -> io::Result<()> {
        let mut author = self.state.author.write().await;
        if author.is_some() || self.has_account() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "设备上已有账号",
            ));
        }
        let root = self.state.root.clone();
        let source = source.to_owned();
        let transport = self.state.identity.secret_bytes().1;
        let password = zeroize::Zeroizing::new(password.to_owned());
        let opened = tokio::task::spawn_blocking(move || {
            super::account::restore(&root, &source, &password, transport)
        })
        .await
        .map_err(io::Error::other)??;
        *author = Some(opened);
        Ok(())
    }

    pub async fn logout(&self) {
        self.state.author.write().await.take();
    }

    fn signer<'a>(
        &'a self,
        author: &'a Option<super::account::Unlocked>,
    ) -> io::Result<&'a DeviceIdentity> {
        author
            .as_ref()
            .map(|author| &author.signer)
            .or(self.state.legacy_author.then_some(&self.state.identity))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "请先登录再发布或修改帖子")
            })
    }

    pub async fn publish(&self, title: &str, body: &str) -> io::Result<Post> {
        let author = self.state.author.read().await;
        let signer = self.signer(&author)?;
        self.state.forum.write().await.publish_as(
            signer,
            self.state.board,
            title,
            body,
            now(),
            author.as_ref().map(|author| author.name.as_str()),
        )
    }

    pub async fn active_author(&self) -> Option<[u8; 32]> {
        self.state
            .author
            .read()
            .await
            .as_ref()
            .map(|author| author.signer.public().ed25519_pub)
    }

    pub async fn local_posts(&self, bookmarks_only: bool) -> Vec<PostHint> {
        let forum = self.state.forum.read().await;
        let mut posts: Vec<_> = forum
            .posts
            .values()
            .filter(|(post, retention, _)| {
                post.signed_event().conversation_id.as_bytes() == &self.state.board
                    && if bookmarks_only {
                        matches!(retention, Retention::Bookmarked(_))
                    } else {
                        matches!(retention, Retention::Owned)
                    }
            })
            .map(|(post, _, _)| post.hint().clone())
            .collect();
        posts.sort_unstable_by_key(|hint| (std::cmp::Reverse(hint.event.lamport), hint.id));
        posts
    }

    pub async fn search_local_posts(&self, query: &str) -> Vec<LocalSearchHit> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let forum = self.state.forum.read().await;
        let mut posts: Vec<_> = forum
            .posts
            .values()
            .filter(|(post, _, _)| {
                post.signed_event().conversation_id.as_bytes() == &self.state.board
            })
            .collect();
        posts.sort_unstable_by_key(|(post, _, _)| {
            (std::cmp::Reverse(post.hint().event.lamport), post.id)
        });
        posts
            .into_iter()
            .filter_map(|(post, retention, _)| {
                let author = post.hint().author_name().unwrap_or_default();
                let matches_meta = post.title.to_lowercase().contains(&query)
                    || author.to_lowercase().contains(&query);
                let matching_line = post
                    .body
                    .lines()
                    .find(|line| line.to_lowercase().contains(&query));
                if !matches_meta
                    && matching_line.is_none()
                    && !post.body.to_lowercase().contains(&query)
                {
                    return None;
                }
                let source = match retention {
                    Retention::Owned => LocalPostSource::Owned,
                    Retention::Bookmarked(_) => LocalPostSource::Bookmarked,
                    Retention::Cached => LocalPostSource::Cached,
                };
                Some(LocalSearchHit {
                    hint: post.hint().clone(),
                    source,
                    snippet: matching_line
                        .unwrap_or(post.body.lines().next().unwrap_or_default())
                        .chars()
                        .take(120)
                        .collect(),
                })
            })
            .take(50)
            .collect()
    }

    pub fn author_key(&self) -> [u8; 32] {
        self.state.identity.public().ed25519_pub
    }

    pub async fn configure_cache(&self, limits: CacheLimits) -> io::Result<usize> {
        let bytes = codec().serialize(&limits).map_err(io::Error::other)?;
        let mut forum = self.state.forum.write().await;
        forum.persist("cache-settings", &bytes)?;
        forum.set_cache_limits(limits, now())
    }

    pub async fn clear_cache(&self) -> io::Result<usize> {
        self.state.forum.write().await.evict(
            now(),
            CacheLimits {
                posts: 0,
                bytes: 0,
                idle_seconds: 0,
            },
        )
    }

    pub async fn storage(&self) -> io::Result<StorageUsage> {
        let forum = self.state.forum.read().await;
        let mut usage = StorageUsage {
            limits: forum.cache_limits,
            owned_posts: 0,
            owned_bytes: 0,
            bookmarked_posts: 0,
            bookmarked_bytes: 0,
            cached_posts: 0,
            cached_bytes: 0,
            partial_bytes: None,
        };
        let mut attachments = std::collections::BTreeMap::new();
        for (post, retention, _) in forum.posts.values() {
            let size = fs::metadata(forum.directory.join(super::filename(post.id)))?.len();
            match retention {
                Retention::Owned => {
                    usage.owned_posts += 1;
                    usage.owned_bytes += size;
                }
                Retention::Bookmarked(_) => {
                    usage.bookmarked_posts += 1;
                    usage.bookmarked_bytes += size;
                }
                Retention::Cached => {
                    usage.cached_posts += 1;
                    usage.cached_bytes += size;
                }
            }
            if post.hint().attachment().is_some() {
                let rank = match retention {
                    Retention::Owned => 2_u8,
                    Retention::Bookmarked(_) => 1,
                    Retention::Cached => 0,
                };
                attachments
                    .entry(post.hint().thread_id())
                    .and_modify(|current: &mut u8| *current = (*current).max(rank))
                    .or_insert(rank);
            }
        }
        for (id, rank) in attachments {
            let path = forum
                .directory
                .join(super::attachment::attachment_filename(id));
            let bytes = match fs::metadata(path) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match rank {
                2 => usage.owned_bytes += bytes,
                1 => usage.bookmarked_bytes += bytes,
                _ => usage.cached_bytes += bytes,
            }
        }
        drop(forum);
        if let Ok(staging) = self.state.staging.try_lock() {
            usage.partial_bytes = Some(staging.usage()?);
        }
        Ok(usage)
    }

    pub async fn has_local_post(&self, id: PostId) -> bool {
        self.state.forum.read().await.posts.contains_key(&id)
    }

    pub async fn read(&self, id: PostId) -> io::Result<Option<Post>> {
        self.state.forum.write().await.read(id, now())
    }

    pub async fn bookmark_mode(&self, id: PostId) -> Option<Bookmark> {
        let forum = self.state.forum.read().await;
        match forum.posts.get(&id) {
            Some((_, Retention::Bookmarked(mode), _)) => Some(*mode),
            _ => None,
        }
    }

    pub async fn bookmark(&self, id: PostId, mode: Option<Bookmark>) -> io::Result<()> {
        let mut forum = self.state.forum.write().await;
        match mode {
            Some(mode) => forum.bookmark(id, mode),
            None => forum.unbookmark(id),
        }
    }

    pub async fn withdraw(&self, id: PostId) -> io::Result<()> {
        let author = self.state.author.read().await;
        let signer = self.signer(&author)?;
        self.state.forum.write().await.withdraw(signer, id, now())?;
        Ok(())
    }

    pub async fn edit(&self, id: PostId, title: &str, body: &str) -> io::Result<Post> {
        let author = self.state.author.read().await;
        let signer = self.signer(&author)?;
        self.state.forum.write().await.edit_as(
            signer,
            id,
            title,
            body,
            now(),
            author.as_ref().map(|author| author.name.as_str()),
        )
    }

    /// Discover a bounded page across configured peers. Unreachable peers are skipped.
    pub async fn discover(&self, offset: u16) -> io::Result<Vec<PostHint>> {
        if usize::from(offset) >= INDEX_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "board index offset out of range",
            ));
        }
        let peers = self.state.active_peers().await;
        let known: Arc<std::collections::BTreeMap<_, _>> = Arc::new(
            self.cached_index()
                .await
                .into_iter()
                .map(|hint| (hint.id, hint))
                .collect(),
        );
        let mut history = Vec::new();
        let mut page = std::collections::BTreeMap::new();
        let mut reached = peers.is_empty();
        let mut first_error = None;
        let mut tasks = tokio::task::JoinSet::new();
        for peer in peers {
            let state = Arc::clone(&self.state);
            let known = Arc::clone(&known);
            tasks.spawn(async move {
                let entries = timeout(REQUEST_TIMEOUT, async {
                    let mut entries = Vec::new();
                    for start in (0..=(usize::from(offset) + PAGE_SIZE - 1).min(INDEX_LIMIT - 1))
                        .step_by(PAGE_SIZE)
                    {
                        let mut session = dial(&state, peer).await?;
                        let page = fetch_index(&mut session, state.board, start as u16).await?;
                        let last = page.len() < PAGE_SIZE;
                        entries.extend(page);
                        if last {
                            break;
                        }
                    }
                    Ok::<_, io::Error>(entries)
                })
                .await
                .map_err(io::Error::other)??;
                let history = repair_history(&state, peer, &entries, &known).await;
                Ok::<_, io::Error>((entries, history))
            });
        }
        while let Some(result) = tasks.join_next().await {
            match result.map_err(io::Error::other)? {
                Ok((entries, repaired)) => {
                    history.extend(repaired);
                    reached = true;
                    for entry in entries {
                        page.insert(entry.id, entry);
                    }
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if !reached {
            return Err(first_error.unwrap_or_else(|| io::Error::other("no reachable source")));
        }
        let mut index = self.state.index.lock().await;
        let forum = self.state.forum.read().await;
        let mut page: Vec<_> = page
            .into_values()
            .filter(|entry| !forum.is_withdrawn(entry.thread_id()))
            .collect();
        if !self.state.legacy_author {
            let public_roots: std::collections::BTreeSet<_> = page
                .iter()
                .filter(|entry| entry.author_name().is_some())
                .map(PostHint::thread_id)
                .collect();
            history.retain(|entry| public_roots.contains(&entry.thread_id()));
            history.extend(
                page.iter()
                    .filter(|entry| public_roots.contains(&entry.thread_id()))
                    .cloned(),
            );
            page.retain(|entry| entry.author_name().is_some());
        } else {
            history.extend(page.iter().cloned());
        }
        page.sort_unstable_by_key(|entry| (std::cmp::Reverse(entry.event.lamport), entry.id));
        let mut next = index.clone();
        next.merge(&history, |id| !forum.is_withdrawn(id));
        if !index.same_revisions(&next) {
            forum.persist(index_cache::NAME, &next.encode()?)?;
            *index = next;
        }
        Ok(page
            .into_iter()
            .take(INDEX_LIMIT)
            .skip(usize::from(offset))
            .take(PAGE_SIZE)
            .collect())
    }

    /// Last verified metadata, including entries whose sources are now unreachable.
    pub async fn cached_index(&self) -> Vec<PostHint> {
        let index = self.state.index.lock().await;
        let forum = self.state.forum.read().await;
        index
            .entries()
            .iter()
            .filter(|hint| !forum.is_withdrawn(hint.thread_id()))
            .cloned()
            .collect()
    }

    pub async fn download(&self, hint: &PostHint, bookmark: Option<Bookmark>) -> io::Result<Post> {
        self.download_with_progress(hint, bookmark, None).await
    }

    pub async fn download_with_progress(
        &self,
        hint: &PostHint,
        bookmark: Option<Bookmark>,
        progress: Option<&crate::ProgressSender>,
    ) -> io::Result<Post> {
        if hint.event.conversation_id.as_bytes() != &self.state.board {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "post is outside the active board",
            ));
        }
        if !self.state.legacy_author && hint.author_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "帖子缺少已签名的作者账号",
            ));
        }
        if let Some(post) = self.read(hint.id).await? {
            if let Some(mode) = bookmark {
                self.bookmark(hint.id, Some(mode)).await?;
            }
            let bytes = post.body.len() as u64;
            crate::report_progress(progress, bytes, bytes);
            return Ok(post);
        }
        let peers = self.connect_peers().await;
        let post = self
            .state
            .staging
            .lock()
            .await
            .fetch_with_progress(hint, peers, progress)
            .await?;
        let mut forum = self.state.forum.write().await;
        match bookmark {
            Some(mode) => forum.store(post.clone(), Retention::Bookmarked(mode), now())?,
            None => forum.cache(post.clone(), now())?,
        }
        Ok(post)
    }

    async fn connect_peers(&self) -> Vec<PeerSession<TcpStream>> {
        let mut tasks = tokio::task::JoinSet::new();
        for invite in self.state.active_peers().await {
            let state = Arc::clone(&self.state);
            tasks.spawn(async move { timeout(REQUEST_TIMEOUT, dial(&state, invite)).await });
        }
        let mut sessions = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(Ok(Ok(session))) = result {
                sessions.push(session);
            }
        }
        sessions
    }

    pub async fn sync(&self) -> io::Result<()> {
        sync_round(&self.state).await
    }

    pub async fn publish_attachment(
        &self,
        title: &str,
        body: &str,
        name: &str,
        bytes: &[u8],
    ) -> io::Result<Post> {
        let author = self.state.author.read().await;
        let signer = self.signer(&author)?;
        self.state.forum.write().await.publish_attachment_as(
            signer,
            self.state.board,
            (title, body),
            now(),
            (name, bytes),
            author.as_ref().map(|author| author.name.as_str()),
        )
    }

    pub async fn download_attachment(&self, hint: &PostHint, keep: bool) -> io::Result<Vec<u8>> {
        self.download_attachment_with_progress(hint, keep, None)
            .await
    }

    pub async fn download_attachment_with_progress(
        &self,
        hint: &PostHint,
        keep: bool,
        progress: Option<&crate::ProgressSender>,
    ) -> io::Result<Vec<u8>> {
        if hint.event.conversation_id.as_bytes() != &self.state.board {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "post is outside the active board",
            ));
        }
        let stored = self.state.forum.read().await.read_attachment(hint.id)?;
        let bytes = match stored {
            Some(bytes) => bytes,
            None => {
                let peers = self.connect_peers().await;
                self.state
                    .staging
                    .lock()
                    .await
                    .fetch_attachment_with_progress(hint, peers, progress)
                    .await?
            }
        };
        let forum = self.state.forum.read().await;
        if forum.is_withdrawn(hint.thread_id()) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "post was withdrawn",
            ));
        }
        if keep {
            forum.save_attachment(hint.id, &bytes)?;
        }
        crate::report_progress(progress, bytes.len() as u64, bytes.len() as u64);
        Ok(bytes)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.listener.abort();
        self.maintenance.abort();
        if let Some(lan) = &self.lan {
            lan.abort();
        }
    }
}

async fn repair_history(
    state: &State,
    peer: Invite,
    entries: &[PostHint],
    known: &std::collections::BTreeMap<PostId, PostHint>,
) -> Vec<PostHint> {
    let mut seen: std::collections::BTreeSet<_> = entries.iter().map(|hint| hint.id).collect();
    let mut pending = entries
        .iter()
        .filter_map(|hint| {
            hint.previous_revision()
                .map(|previous| (hint.thread_id(), previous))
        })
        .collect::<std::collections::VecDeque<_>>();
    let mut history = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut requests = 0;
    while let Some((root, id)) = pending.pop_front() {
        if root == id || !seen.insert(id) {
            continue;
        }
        if let Some(known) = known.get(&id) {
            if known.thread_id() == root
                && let Some(previous) = known.previous_revision()
            {
                pending.push_back((root, previous));
            }
            continue;
        }
        if requests == 24 {
            break;
        }
        requests += 1;
        let request = tokio::time::timeout_at(deadline, async {
            let mut session = dial(state, peer).await?;
            fetch_revision(&mut session, state.board, id, root).await
        })
        .await;
        match request {
            Ok(Ok(Some(revision))) => {
                if let Some(previous) = revision.previous_revision() {
                    pending.push_back((root, previous));
                }
                history.push(revision);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    history
}

async fn dial(state: &State, invite: Invite) -> io::Result<PeerSession<TcpStream>> {
    let stream = TcpStream::connect(invite.address).await?;
    PeerSession::connect(stream, &state.identity.secret_bytes().1, &invite.public_key)
        .await
        .map_err(io::Error::other)
}

async fn listen_for_requests(listener: TcpListener, state: Arc<State>) {
    let capacity = Arc::new(Semaphore::new(16));
    let mut requests = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((stream, _)) = result else { break };
                let Ok(permit) = capacity.clone().try_acquire_owned() else { continue };
                let state = Arc::clone(&state);
                requests.spawn(async move {
                    let _permit = permit;
                    let _ = timeout(REQUEST_TIMEOUT, async {
                        let peers = state.active_peers().await;
                        let mut session = PeerSession::accept(stream, &state.identity.secret_bytes().1, |key| {
                            peers.iter().any(|peer| &peer.public_key == key)
                        }).await.map_err(io::Error::other)?;
                        let forum = state.forum.read().await;
                        let mut pause = state.paused.subscribe();
                        let uploads = !*pause.borrow_and_update();
                        tokio::select! {
                            biased;
                            _ = pause.changed() => Ok(()),
                            result = serve_with_policy(&mut session, &forum, state.board, uploads) => result,
                        }
                    }).await;
                });
            }
            _ = requests.join_next(), if !requests.is_empty() => {}
        }
    }
}

async fn sync_round(state: &State) -> io::Result<()> {
    let peers = state.active_peers().await;
    let mut cursors = state.sync_cursors.lock().await;
    let mut tasks = tokio::task::JoinSet::new();
    for invite in peers {
        let after = cursors.get(&invite.public_key).copied();
        let board = state.board;
        let secret = state.identity.secret_bytes().1;
        tasks.spawn(async move {
            let records = timeout(REQUEST_TIMEOUT, async {
                let stream = TcpStream::connect(invite.address).await?;
                let mut session = PeerSession::connect(stream, &secret, &invite.public_key)
                    .await
                    .map_err(io::Error::other)?;
                fetch_withdrawals(&mut session, board, after).await
            })
            .await;
            (invite.public_key, records)
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let (key, Ok(Ok((records, next)))) = result.map_err(io::Error::other)? {
            let mut forum = state.forum.write().await;
            for withdrawal in records {
                forum.apply_withdrawal(withdrawal)?;
            }
            match next {
                Some(cursor) => {
                    cursors.insert(key, cursor);
                }
                None => {
                    cursors.remove(&key);
                }
            }
        }
    }
    Ok(())
}

async fn maintain(state: Arc<State>) {
    let mut tick = interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(error) = state.forum.write().await.maintain_cache(now()) {
            eprintln!("Forum cache maintenance failed: {error}");
        }
        if let Ok(mut staging) = state.staging.try_lock()
            && let Err(error) = staging.maintain(SystemTime::now())
        {
            eprintln!("Forum download staging maintenance failed: {error}");
        }
        if let Err(error) = sync_round(&state).await {
            eprintln!("Forum withdrawal sync failed: {error}");
        }
    }
}

fn read_metadata(root: &Path, name: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= max => fs::read(path).map(Some),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid profile metadata",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
