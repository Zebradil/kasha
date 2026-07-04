# Binary cache roadmap (deferred from MVP)

See `CONTEXT.md` for terminology and `docs/adr/000{1-5}-*.md` for the decisions behind
the net-local box design. This doc tracks what was deliberately left out of the MVP.

## Client selection shim

Replace the static `[box, remote cache]` substituter list + `connect-timeout` with a
localhost proxy substituter that probes box reachability and routes instantly —
removing the off-network timeout tax entirely. Config-mutating the generated
`nix.conf` in place doesn't fit NixOS well, so the shim should be a small always-running
proxy, not a substituter-list rewriter.

Needs **pluggable discovery backends**:
- **static-endpoint** — configured box URL + reachability probe. Works anywhere,
  including inside a k8s CNI overlay.
- **mDNS** — zero-config, for bare-metal/LAN-host deployments. Cannot cross a k3s CNI
  overlay (link-local TTL=1 multicast doesn't bridge to the LAN); relevant once the box
  relocates to a bare-metal host.

## Garbage collection

No GC in the MVP — box storage is cheap for now and R2 lifecycle rules can be a stopgap.
Design for its own session, but the shape is already clear:

- Mark-sweep from **retained root manifests** (the same manifests that drive discovery).
- **Two-tier retention**: box = hot set (last ~1–3 generations per flake, aggressive);
  remote cache = cold archive (last N generations or ~90 days, lenient).
- Box's retained set must stay a subset of the remote cache's retained set, so box GC
  can never orphan a path a client might still need.
- Guard: never GC a locally-pushed root until it's confirmed mirrored up to the remote
  cache (data-loss risk otherwise — it may be the only copy).
- Guard: remote-cache GC needs a grace window so it never deletes an object whose
  manifest/narinfo hasn't fully landed yet.
- Remote-cache list/delete operations have uneven cost — model this; batching or
  keeping objects longer may be cheaper than frequent scans. Retention values should be
  tunable, not hardcoded.

## Full client-side routing engine

Beyond box-vs-remote-cache selection, a richer client-side router could own multiple
substituters, explicit priority/policy rules, and metrics — a genuinely interesting
direction, but strictly a superset of the selection shim above. Revisit once the shim
exists.

## Dedup / attic-style features

Not adopting `attic` (see ADR 0002) for now. If box storage or bandwidth becomes a real
constraint, revisit content-defined chunking / global dedup then — on evidence, not
speculation.

## Public multi-tenant employee cache

Side idea: offer the org's spare storage/throwaway machines as a shared public cache
employees can also use for personal projects. Needs a team discussion (cost, support,
abuse boundaries) before any design work.
