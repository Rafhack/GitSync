# gitsync

Keeps a git repository (history and uncommitted work) in sync across machines on
the same local network, peer to peer, using git over ssh. See [DESIGN.md](DESIGN.md).

## What works today

- `gitsync snapshot` saves your uncommitted work (including new files, not
  ignored ones) as a hidden commit at `refs/gitsync/<machine>/wip`, without
  touching your staging area or files.
- `gitsync watch` does that automatically whenever files change, plus a poll
  every 30 seconds as a safety net.
- `gitsync status` shows this machine's latest snapshot, whether there are
  changes not in a snapshot yet, the settings, and other machines' snapshots.
- Optional settings file `.git/gitsync.toml` (see DESIGN.md, section 7).
  Command line flags override it.
- `gitsync fetch` pulls branches and snapshots from the `[[peers]]` in the
  settings file over ssh, into private refs (your own branches are never
  touched). `gitsync watch` also does this every poll interval, and
  `gitsync status` shows how each peer branch compares with yours.
  A peer's `name` must match the `machine_id` it uses.
- Peers find each other on the LAN by themselves (mDNS, the default
  `discovery = "mdns"`): `gitsync watch` announces this machine and fetches a
  peer as soon as it announces news, so no IP addresses go in the config.
  `gitsync discover` lists the peers it hears. The first time, ssh has to
  learn each peer's key: `gitsync discover` prints the one-time command.
  On Windows, allow incoming UDP 5353 for gitsync (DESIGN.md, section 4).
- Fetched work is applied when it is safe: if you have not changed anything
  since the last sync, the peer's files (and its commits on your branch) are
  taken over; branches that are simply behind are fast-forwarded.
  `gitsync watch` does this automatically; `gitsync apply` does it once.
  See DESIGN.md, section 6.
- When both machines changed, their work is merged if they changed different
  lines (or different files). If they changed the same lines, or one deleted
  a file the other changed, nothing is touched: `gitsync status` and
  `gitsync resolve` list the files, and `gitsync resolve --take <peer>` or
  `gitsync resolve --keep-mine` settles it (only the conflicting lines follow
  the side you pick; all other changes from both sides are kept).

Running as a background service is the next step (DESIGN.md, section 9).

## Try it

1. Install Rust: https://rustup.rs (one installer, works on Windows, macOS, Linux).
2. Build: `cargo build --release` (the program ends up in `target/release/gitsync`).
3. Run the tests: `cargo test`.
4. In any git repo: `gitsync watch` (Ctrl+C to stop), edit a file, then look at
   the snapshots with `git log --stat refs/gitsync/<your-hostname>/wip`.

## Code map

- `src/main.rs`: command line (`snapshot`, `watch`, `status`, `fetch`, `apply`, `resolve`,
  `discover`).
- `src/lib.rs`: lists the modules below (so tests can use them).
- `src/git.rs`: small wrapper that runs the `git` program.
- `src/snapshot.rs`: turns uncommitted work into a snapshot commit.
- `src/watch.rs`: the daemon: file watcher + polling loop, mDNS, and a thread
  fetching and applying peers.
- `src/discovery.rs`: mDNS announcements and finding peers on the LAN.
- `src/fetch.rs`: fetches a peer's branches and snapshots.
- `src/apply.rs`: decides whether a peer's work is newer, applies or merges
  it, and settles conflicts.
- `src/config.rs`: reads and checks `.git/gitsync.toml`.
- `src/status.rs`: gathers and prints `gitsync status`.
- `tests/`: tests that create throwaway repos (`common/` has the shared helpers).
