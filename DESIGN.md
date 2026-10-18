# gitsync: design

gitsync keeps a git repository in sync across two or more machines on the same
local network. It syncs both committed history and uncommitted work, with no
central server. Every machine runs the same program and talks to the others
using plain `git` over `ssh`.

Status: draft, 2026-10-17. Sections marked **Default** are choices made where
the design could go several ways; they are easy to revisit.

## 1. Big picture

```
 laptop                                desktop
 ┌────────────────────────┐            ┌────────────────────────┐
 │ working tree           │            │ working tree           │
 │   │ file watcher +     │            │   ▲ apply when safe    │
 │   ▼ polling fallback   │   mDNS     │   │                    │
 │ snapshot → refs/gitsync├──"I have──►│ fetch over ssh ◄───────┤
 │            /laptop/wip │   news"    │ (git fetch laptop ...) │
 └────────────────────────┘            └────────────────────────┘
```

Each machine runs a `gitsync daemon` that loops through four steps:

1. **Watch** the working tree for changes.
2. **Snapshot** uncommitted work into a hidden git commit under its own ref.
3. **Announce** on the LAN that it has something new.
4. **Fetch** from peers that announced something, then **apply** their changes
   locally when it is safe to do so.

**Default: pull-only.** A machine never writes into another machine's
repository. It only fetches *from* peers and changes its own copy. This means
a bug or a crash on one machine cannot corrupt another, and ssh only needs read
access to the peer's repo.

## 2. Capturing uncommitted changes

Git can only move commits around, so uncommitted work is turned into a commit
that lives outside of the normal branches:

```
refs/gitsync/<machine-id>/wip      ← latest snapshot from this machine
```

How a snapshot is made, without touching the user's staging area or files:

1. Copy `.git/index` to a private temporary index (`.git/gitsync-index`). Copying
   keeps git's file-stat cache, so the next step is fast on big repos.
2. Run `git add -A` with `GIT_INDEX_FILE` pointing at the temporary index. This
   picks up modified, deleted and new files, and respects `.gitignore`.
3. `git write-tree` turns that index into a tree object.
4. If the tree and `HEAD` are the same as in the previous snapshot, stop:
   nothing changed.
5. Otherwise `git commit-tree` creates a snapshot commit, and `git update-ref`
   moves `refs/gitsync/<machine-id>/wip` to it (using compare-and-swap, so two
   snapshots racing cannot lose each other).

The snapshot commit's parents are:

- the previous snapshot (so snapshots form their own little history, which
  lets peers tell "newer" from "diverged" without trusting clocks), and
- the current `HEAD` (so fetching the snapshot also brings every commit it is
  based on).

The message carries trailers such as `gitsync-machine: laptop` and
`gitsync-branch: main`.

This piece is built: `gitsync snapshot` and `gitsync watch`.
`gitsync status` shows whether the working tree has changes not in a snapshot yet.

## 3. Watching for changes

- **OS watcher:** the `notify` crate uses inotify on Linux, FSEvents on macOS
  and ReadDirectoryChangesW on Windows. Events are debounced (default 1 s of
  quiet) so saving 20 files in a burst produces one snapshot.
- **Polling fallback:** every N seconds (default 30) a snapshot is attempted
  anyway. This catches what watchers miss: network drives, editors that swap
  files, a watcher that hit the OS limit, and commits made by other tools.
  Polling is cheap because step 4 above skips unchanged trees.
- Events inside `.git/` are ignored, so gitsync's own writes do not trigger
  itself.

## 4. Finding peers on the LAN

**Default: mDNS / DNS-SD** (the same mechanism printers and AirPlay use), with
the `mdns-sd` crate, which is pure Rust and works on all three OSes.

- Each daemon advertises a service `_gitsync._tcp.local.` with TXT records:
  - `machine` = machine id
  - `repo` = a repo id (hash of the repo's first commit, so only peers with the
    same project match)
  - `path` = path to the repo on that machine (used in the ssh URL)
  - `user` = ssh user
  - `gen` = a counter bumped on every new snapshot or ref change
- Peers browse for that service. When a peer's `gen` changes, that is the
  "I have news" signal and triggers a fetch.
- **Fallback:** a static list of peers in the config file, for networks that
  block multicast (some corporate Wi-Fi). Static peers are simply polled.

Security note: mDNS is only used to *find* peers. All data moves over ssh, so a
fake announcement can at most cause a failed ssh connection.

## 5. Shipping changes between machines

Plain `git fetch` over ssh, run as a child process. For a peer `desktop`:

```
git fetch ssh://user@desktop.local/path/to/repo \
    +refs/heads/*:refs/gitsync-peers/desktop/heads/* \
    +refs/gitsync/desktop/*:refs/gitsync/desktop/*
```

- Peer branches land in a private namespace, never directly on the user's
  branches.
- Only the peer's *own* snapshots are fetched, so a peer can never overwrite
  our snapshot or relay a third machine's. This is why a peer's `name` in the
  config must equal that peer's `machine_id`; `gitsync fetch` warns when it
  finds no snapshot under that name.
- ssh runs with `BatchMode=yes` (never prompts) and connection/keepalive
  timeouts, unless the user configured their own `GIT_SSH_COMMAND` or
  `core.sshCommand`. FETCH_HEAD is not written, so the user's own
  `git fetch`/`git pull` state is left alone.
- `gitsync watch` fetches static peers every poll interval on a separate
  thread, so an unreachable peer never delays snapshots.
- **Default: use the system `git` and `ssh` binaries** instead of a git library.
  This reuses the user's existing ssh keys, `~/.ssh/config` and agent with zero
  extra setup, and behaves exactly like the git the user already knows.
  Requirement: every machine runs an ssh server (Windows 10+ ships OpenSSH
  Server as an optional feature; macOS has "Remote Login").

## 6. Applying peer changes and handling conflicts

The rule: **never destroy unsynced local work.** When in doubt, keep both sides
and tell the user.

**Branches (committed history):**

- If the local branch is an ancestor of the peer's branch: fast-forward it. If
  it is the checked-out branch, only do so when the working tree has no
  uncommitted changes newer than the last applied snapshot.
- If the peer's branch is an ancestor of ours: nothing to do (they will fetch
  from us).
- If they diverged: do not merge automatically. Keep the peer's version at
  `refs/gitsync-peers/<peer>/heads/<branch>` and report it in `gitsync status`.
  **Default:** no auto-merge of history, because merge commits the user did not
  ask for are confusing and hard to undo.

**Uncommitted work (snapshots):** each machine tracks the last snapshot it has
"seen" (its own or applied from a peer).

| Local working tree         | Peer snapshot                 | Action                                   |
|----------------------------|-------------------------------|------------------------------------------|
| unchanged since last seen  | newer (descends from it)      | apply peer tree to working tree          |
| changed                    | unchanged                     | nothing; peers will fetch ours           |
| changed                    | changed (diverged)            | 3-way merge, see below                   |

3-way merge uses `git merge-tree --write-tree` with the last common snapshot as
the base. If it merges cleanly, the result is written to the working tree and
snapshotted. If it conflicts, the working tree is left alone, the peer's
snapshot is kept under its ref, and `gitsync status` shows the conflicting
files. `gitsync resolve --take <peer>` or `--keep-mine` settles it.

Applying a snapshot only touches files that differ, and is skipped while a git
operation is in progress (`.git/index.lock`, rebase or merge in progress).

Different checked-out branches on two machines: snapshots are applied only when
both machines are on the same branch. Otherwise they are just stored.

## 7. Configuration

`gitsync.toml` inside `.git/` (per repo, never committed by accident):

```toml
machine_id = "laptop"         # default: hostname
poll_interval_secs = 30
debounce_ms = 1000
discovery = "mdns"            # or "static"
[[peers]]                     # optional static peers
name = "desktop"
url = "ssh://rafael@192.168.1.20/home/rafael/code/myproj"
```

## 8. Crates

| Crate                     | Why                                                    |
|---------------------------|--------------------------------------------------------|
| `clap`                    | Command line parsing (`gitsync watch`, `snapshot` ...) |
| `notify`, `notify-debouncer-full` | Cross-platform file watching with debouncing   |
| `mdns-sd`                 | Peer discovery, pure Rust, no system daemon needed     |
| `serde`, `toml`           | Config file                                            |
| `anyhow`                  | Simple error handling with context messages            |
| `tracing`, `tracing-subscriber` | Logging                                          |
| `gethostname`             | Default machine id                                     |
| `tempfile` (tests)        | Throwaway repos in tests                               |

Not used, on purpose: an async runtime (tokio). The daemon does a few things at
a human pace, so plain threads and channels are simpler to read, which matters
while learning Rust. `git2`/`gix` may replace some `git` calls later for speed.

## 9. Build plan

1. ✅ Snapshot of uncommitted work to `refs/gitsync/<id>/wip` + `watch` loop.
2. ✅ Config file and `gitsync status`.
3. ✅ Fetch from static peers over ssh.
4. Apply peer snapshots and fast-forward branches (with the safety rules).
5. mDNS discovery and `gen` announcements.
6. Conflict handling and `gitsync resolve`.
7. Run as a background service (systemd user unit, launchd agent, Windows task).
