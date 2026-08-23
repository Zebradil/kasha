# Box is one static Rust binary over a flat binary-cache store

Supersedes ADR-0002 (nix-native box store), ADR-0003 (roots-only manifests), and
ADR-0006 (bash reusable tools). ADR-0001 (eager bidirectional replica) and ADR-0004
(box holds no signing key) survive unchanged. Design record:
`docs/agents/handoff-rewrite.md`.

The box is a single static Rust binary (`kasha`) providing the HTTP cache server,
mirror workers, GC, and the CLI (`emit`, `gc`). What changed and why:

- **Flat binary-cache layout on disk** (`<hash>.narinfo`, `nar/…`, `roots/…`) — the
  same layout as the remote bucket. The filesystem is the database; an in-memory
  index is rebuilt by scanning narinfos at boot. No nix on the box, no harmonia, no
  sqlite. Objects are stored byte-identical as received.
- **Manifest v3 carries the full build-closure path list** plus explicit `branch` and
  `attr` fields. Consumers never substitute `.drv` paths, so the drv-mirroring flow
  (the reason the box needed nix's closure awareness) is structurally gone. This
  removed the live failure mode of generations looping "incomplete, will retry" on
  `.drv` paths never uploaded.
- **Mirror-down is a dumb fetch** of listed paths: remote cache first, upstream
  caches for the rest. Missing paths are recorded gaps, re-probed quietly each
  cycle — steady state, never a failing unit.
- **Push is HTTP PUT** (`nix copy --to http://box` with netrc basic auth or bearer
  token); sshd/ssh-ng removed. Every ingested narinfo signature (push and mirror) is
  verified against trusted public keys.
- **GC**: retention-driven box sweep in-process on a timer; remote sweep runs in CI
  (`kasha gc`) with delete-capable credentials the box never holds. Retention source of
  truth is manifest presence — pruning `roots/<flake>/<gen>.json` *is* the retention
  decision, no separate ledger — and the grouping key is the manifest's explicit
  `(branch, attr)` fields, never a parse of the opaque gen-id. Only the remote sweep
  deletes manifests; the box marks a strict subset, so box-retained ⊆ remote-retained by
  construction. R2 pricing shaped this: DELETE is free, LIST is $4.50/M batched 1000/page,
  GET/HEAD $0.36/M, no egress, storage $0.015/GB-mo — API ops are negligible at any
  realistic scale and storage-GB is the only real cost, so the sweep is simple and
  infrequent rather than clever (no lock, no tombstone, just a 24h grace window on top of
  manifest-published-last ordering).
- **Bash tools and the box NixOS module are gone**; the OCI image with the static
  binary is the only box artifact (ADR-0007's generic env-configured image contract
  stands). The consumer NixOS module stays.

Tripwire from ADR-0006 fired as predicted: mirror state stopped being a flat set of
gen-ids (per-path gap tracking, local-origin tracking, retention policy, signature
verification), so the set-math-in-bash rationale no longer held.
