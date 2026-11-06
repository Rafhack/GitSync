# Changelog

The release workflow puts the section for a version at the top of that
version's GitHub release notes. Add a section here when you bump the version
in Cargo.toml.

## v0.3.0

### New: sync without ssh (the gitsync transport)

- Machines can now sync over gitsync's own TLS connection instead of ssh: no
  ssh server, accounts or keys to set up.
- Pair two machines once: `gitsync pair` on one prints a 6-digit code, then
  `gitsync pair <code>` on the other. After that, each machine trusts the
  other's key, and a paired machine can only read the repo.
- `gitsync id` shows this machine's key, `gitsync unpair <machine>` forgets one.
- This is now the default (`transport = "gitsync"`). To keep using ssh, set
  `transport = "ssh"` in `.git/gitsync.toml`.
- On Windows, allow incoming TCP for gitsync once (`gitsync service install`
  prints the command).
- Details: DESIGN.md, section 11.

### Other changes

- Releases are now published when a pull request that bumps the version is
  merged into main.
