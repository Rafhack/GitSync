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
- Events inside `.git/` are ignored, so gitsync's own writes (objects, its
  refs, its private index) do not trigger itself. The exceptions are the
  user's own branch moves: `HEAD`, `refs/heads/` and `packed-refs`. A commit
  only touches `.git/`, and peers should hear about it at once, not at the
  next poll.

## 4. Finding peers on the LAN

**Default: mDNS / DNS-SD** (the same mechanism printers and AirPlay use), with
the `mdns-sd` crate, which is pure Rust and works on all three OSes.

- Each `gitsync watch` advertises a service `_gitsync._tcp.local.` per repo
  (instance `<machine>-<repo id>`, port = the ssh port) with TXT records:
  - `machine` = machine id
  - `repo` = repo id: the repo's first commit, so only peers with the same
    project match
  - `user`, `path` = how to reach the repo over ssh. On Windows the path is
    in Git Bash form (`/c/Users/...`), since peers reach it through Git Bash
    (section 5). Both can be overridden in the config (`[advertise]`).
  - `gen` = a fingerprint of our branches and snapshot ref. It changes on every
    new snapshot, commit or branch update, and equal state gives an equal
    value, so a restart does not make peers fetch for nothing.
- `watch` re-announces whenever `gen` changes (after snapshots and applies).
  Peers browse for the service; a changed `gen` is the "I have news" signal
  and triggers a fetch right away. A peer that announces itself is otherwise
  only fetched every 5 minutes, as a safety net for lost UDP packets. On
  Ctrl+C, `watch` withdraws its announcement so peers forget it at once.
- After every local snapshot attempt, peers whose work is waiting (both
  changed, staged changes, ...) are re-checked against what was already
  fetched. Resolving on our side (dropping an edit, unstaging) then takes
  effect within seconds, even if the peer has nothing new to announce.
- The ssh URL is built from the announcement's `user` and `path` and the
  address mDNS resolved (IPv4 preferred, never loopback or link-local), not
  from anything the TXT record claims about addresses. A peer heard with only
  IPv6 is held back 1.5 s for its IPv4 address, and switches to IPv4 as soon
  as one arrives later.
- **IPv6 fallback:** mDNS sends each address family in its own packet, and
  some Wi-Fi networks lose the IPv4 one (measured: a Windows laptop on
  battery got only the IPv6 answer from Linux in 10 of 10 tries, while the
  other direction always got IPv4). Waiting longer does not help: the packet
  never arrives. The IPv6 address works for ssh just as well (the host key is
  checked by alias, not by address), so gitsync uses it and labels it
  "(IPv6)" in `discover` and the `watch` log.
- **Fallback / mix:** peers listed in the config are always used too, for
  networks that block multicast (some corporate Wi-Fi). Their URL wins over an
  announced one; they are polled every poll interval unless they announce.
  `discovery = "static"` turns mDNS off entirely.
- `gitsync discover` lists the peers it hears for a few seconds; `gitsync
  fetch` also listens for 2 seconds before fetching.

**Security:** mDNS is only used to *find* peers; all data moves over ssh.
Announcements are untrusted: machine names must be valid ref components and
not ours, user names are limited to `[A-Za-z0-9._-]` (no leading `-`), paths
must be absolute without control characters. For discovered peers, ssh checks
the host key under a fixed name, `HostKeyAlias=gitsync-<peer>`, instead of
the IP address. So a peer whose IP changes (DHCP) needs no new trust, and a
fake announcement pointing somewhere else fails host key verification. The
first time, the user accepts the key by hand (`gitsync discover` prints the
exact `ssh -o HostKeyAlias=...` command). This is skipped if the user set
their own `GIT_SSH_COMMAND` or `core.sshCommand`.

**Windows:** the Windows firewall lets `gitsync.exe` send announcements but not
receive them on a *Public* network until an inbound rule allows UDP 5353
(the built-in mDNS rule only covers Windows' own DNS service).

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
  it is the checked-out branch, this normally happens as part of applying the
  peer's snapshot (below); otherwise only when nothing at all is uncommitted.
- Branches that exist only on the peer are not created locally; they stay
  under `refs/gitsync-peers/<peer>/heads/`.
- If the peer's branch is an ancestor of ours: nothing to do (they will fetch
  from us).
- If they diverged: do not merge automatically. Keep the peer's version at
  `refs/gitsync-peers/<peer>/heads/<branch>` and report it in `gitsync status`.
  **Default:** no auto-merge of history, because merge commits the user did not
  ask for are confusing and hard to undo.

**Uncommitted work (snapshots):** a snapshot holds a *state*: the commit HEAD
pointed to plus the whole working tree. To decide whether a peer's state is
newer than ours, gitsync compares snapshot histories, which needs no clocks:
each machine snapshots right after applying a peer's state, so its snapshot
chain records every state it has taken over.

| Situation                                            | Action                              |
|------------------------------------------------------|-------------------------------------|
| our state equals the peer's                          | in sync; remember it as the base    |
| the peer's state is in **our** history               | ours is newer; peers will fetch it  |
| our state is in the **peer's** history               | peer is newer: apply its state      |
| neither, but our state equals the last base          | peer is newer: apply its state      |
| neither, and the peer's state equals the last base   | ours is newer                       |
| neither                                              | both changed: diverged, see below   |

The checks run top to bottom, so when both readings are possible local work
wins. The base (`refs/gitsync-state/<peer>/base`, a namespace peers never
fetch) is the last state both machines had; before the first sync, a clean
checkout of our HEAD counts as the base. A state the user typed but that was
not snapshotted yet still counts as a local change.

Applying moves the working tree from our state to theirs with
`git read-tree -m -u` on a private index: only files that differ are written,
and git refuses to overwrite a file the user changed since the decision, or
an untracked file in the way. If the peer's state is based on a newer commit
of the same branch, the branch is fast-forwarded too (`update-ref` with
compare-and-swap, so the reflog shows `gitsync: apply <peer>`) and the
staging area is pointed at the new commit. gitsync snapshots before and after
applying, so every state stays recoverable from the snapshot history.

Applying waits (and says why in `gitsync status`) while a git operation is in
progress (`index.lock`, merge, rebase, cherry-pick, revert, bisect) or while
something is staged, so gitsync never rewrites the user's staging area.

**Diverged (step 6, not built yet):** 3-way merge uses
`git merge-tree --write-tree` with the last common state as the base. If it merges cleanly, the result is written to the working tree and
snapshotted. If it conflicts, the working tree is left alone, the peer's
snapshot is kept under its ref, and `gitsync status` shows the conflicting
files. `gitsync resolve --take <peer>` or `--keep-mine` settles it.

Different checked-out branches on two machines: snapshots are applied only when
both machines are on the same branch. Otherwise they are just stored.

## 7. Configuration

`gitsync.toml` inside `.git/` (per repo, never committed by accident):

```toml
machine_id = "laptop"         # default: hostname
poll_interval_secs = 30
debounce_ms = 1000
discovery = "mdns"            # or "static"

[advertise]                   # optional: how peers reach this machine
user = "rafael"               # default: current user
path = "/home/rafael/code/myproj"  # default: the repo folder
port = 22

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
| `ctrlc`                   | Withdraw the mDNS announcement on Ctrl+C               |
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
4. ✅ Apply peer snapshots and fast-forward branches (with the safety rules).
5. ✅ mDNS discovery and `gen` announcements.
6. Conflict handling and `gitsync resolve`.
7. Run as a background service (systemd user unit, launchd agent, Windows task).
