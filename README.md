# Mesh Forum

Mesh Forum is a native macOS forum for sharing signed posts across a local
network. It uses Mesh Foundation for authenticated peer connections and
verified transfers.

## Run on macOS

Requires macOS 15 or newer and a Rust toolchain.

```sh
cargo run --locked --features desktop --bin mesh-forum-desktop
```

To build and open the local app bundle:

```sh
./scripts/package-macos.sh
open "target/Mesh Forum.app"
```

## Use the forum

- **Board** automatically discovers compatible devices on the same LAN.
  Discovery uses UDP port 47625. The default peer connection port is TCP 47624.
- **Read as a guest** to browse posts, download attachments, and save
  bookmarks. **Save and share** lets nearby devices retrieve a retained copy
  from yours. **Save locally** keeps the copy for offline reading.
- **Create or unlock an author account** to publish, revise, or withdraw
  posts. A guest can start a draft and sign in when ready to publish.
- **Find posts** with `⌘F` to search local titles, authors, and bodies. Search
  results distinguish your posts, bookmarks, and cached posts.
- **Settings** offers Simplified Chinese, English, and system language,
  as well as light, dark, and system appearance. **Storage and cache** configures
  automatic cleanup. **Nearby devices** shows connection status and lets
  you pause or resume sharing.

To move your author identity, use **Account → Back up account**. On an empty
profile, choose **Account → Restore from backup** and enter the password used
when the backup was created. The backup contains the encrypted author identity.
Posts, bookmarks, and attachments stay on the original device.

The default local profile is at `~/Library/Application Support/Mesh Forum`.
For separate profiles, set `MESH_FORUM_DATA`. Set
`MESH_FORUM_LISTEN=0.0.0.0:0` to let the OS assign a separate TCP port while
keeping LAN discovery enabled.

## Development and validation

The manifest and lockfile pin all seven Mesh Foundation packages to the same
Git revision. Replication, E2EE and mailbox composition tests use development
dependencies; the production forum protocol remains application-owned.

```sh
cargo test --locked --features desktop-visual
cargo clippy --locked --all-targets --features desktop-visual -- -D warnings
cargo build --locked --features desktop-visual --bin mesh-forum-desktop
```

The desktop checks use temporary profiles and macOS Accessibility permission.
The single-instance smoke test requires `agent-desktop`. These commands run in
the background:

```sh
MESH_FORUM_TEST_LIGHT=1 python3 scripts/desktop-background-smoke.py
MESH_FORUM_TEST_DARK=1 MESH_FORUM_TEST_NARROW=1 MESH_FORUM_TEST_REDUCE_MOTION=1 python3 scripts/desktop-background-smoke.py
MESH_FORUM_TEST_BACKGROUND=1 swift scripts/desktop-smoke.swift
MESH_FORUM_TEST_BACKGROUND=1 MESH_FORUM_TEST_RECOVERY=1 swift scripts/desktop-smoke.swift
```

The single-instance check covers navigation, language switching, draft
preservation, and local search. The Swift smoke test covers discovery, editing,
offline relay, attachment download and retry, and account recovery.

For native file panels or physical input-method checks, use a dedicated macOS
session. Set `MESH_FORUM_ALLOW_FOREGROUND_TESTS=1` and either
`MESH_FORUM_TEST_RECOVERY=1 MESH_FORUM_TEST_NATIVE_PANEL=1` or
`MESH_FORUM_TEST_IME=1` when running the Swift script. The IME test uses the
macOS Pinyin - Simplified input source.

For optional visual inspection, set `MESH_FORUM_EVAL_CAPTURE_DIR` for the
Python script or `MESH_FORUM_CAPTURE_DIR` for the Swift script.

## Module ownership

Mesh Foundation supplies authenticated sessions, signatures, encrypted storage and
verified resumable blob transfers. Forum owns post/revision/withdrawal records,
board discovery, author access, bookmarks and sharing policy. Its production
protocol does not depend on chat membership or the generic replication driver.

The serving index is an application-owned ordered projection, updated only after
a durable post/retention write and removed with its retained post. Opening a
profile rebuilds it from validated records. Each index page visits at most the
200-entry board window and resolves at most 30 posts, without sorting all posts
again. The extra index stores one ordering key per shareable retained post; cache
cleanup and withdrawals remove the corresponding keys. Upload admission remains
checked on each request. Offset pages are fresh observations, not a pinned remote
snapshot, so clients do not reuse cached pages across discovery calls. Withdrawal
scans deliberately restart after exhaustion to catch new records below an old
hash cursor.
