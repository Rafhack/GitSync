# gitsync

Keeps a git repository (history and uncommitted work) in sync across machines on
the same local network, peer to peer, using git over ssh. See [DESIGN.md](DESIGN.md).

## What works today

- `gitsync snapshot` saves your uncommitted work (including new files, not
  ignored ones) as a hidden commit at `refs/gitsync/<machine>/wip`, without
  touching your staging area or files.
- `gitsync watch` does that automatically whenever files change, plus a poll
  every 30 seconds as a safety net.

Syncing to other machines is the next step (DESIGN.md, section 9).

## Try it

1. Install Rust: https://rustup.rs (one installer, works on Windows, macOS, Linux).
2. Build: `cargo build --release` (the program ends up in `target/release/gitsync`).
3. Run the tests: `cargo test`.
4. In any git repo: `gitsync watch` (Ctrl+C to stop), edit a file, then look at
   the snapshots with `git log --stat refs/gitsync/<your-hostname>/wip`.

## Code map

- `src/main.rs`: command line (`snapshot`, `watch`).
- `src/git.rs`: small wrapper that runs the `git` program.
- `src/snapshot.rs`: turns uncommitted work into a snapshot commit.
- `src/watch.rs`: file watcher + polling loop.
- `tests/snapshot.rs`: tests that create throwaway repos.
