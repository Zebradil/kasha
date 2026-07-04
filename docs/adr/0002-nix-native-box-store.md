# Box is a nix-native store, not attic or a raw-object mirror

The box needs to serve, accept pushes, and sync bidirectionally with the remote cache.
We chose a real nix store (`harmonia` + `sshd`/nix-daemon, synced via `nix copy`) over:

- **attic** — offers global dedup, GC, and multi-tenancy we don't need yet, but is
  effectively a required always-on stateful server (own DB) rather than a thin layer
  over a dumb bucket, and its maintenance has been irregular enough that a community
  fork (`celler`) exists. Revisit if dedup/GC/multi-tenancy become load-bearing.
- **raw S3-object byte mirror** (`rclone bisync`) — simpler, but pushes closure
  awareness (and any future GC) onto ad-hoc tooling instead of nix itself.

The remote cache stays a dumb S3-compatible bucket regardless, so it stays swappable.
