# Handoff: kasha v2 rewrite — single Rust binary

Design agreed in a grilling session (2026-08-20). Not yet built. This doc is the spec to implement.
It supersedes the bash/harmonia POC architecture (ADR-0002 nix-native store, ADR-0003 roots-only
manifests, ADR-0006 bash tools) and absorbs `docs/agents/handoff-gc.md` (GC policy carried over,
mark step simplified). ADR-0001 (eager bidirectional replica) and ADR-0004 (box holds no signing
key) survive unchanged. Write fresh ADRs for the superseding decisions once built.

## Why rewrite (evidence, don't re-litigate)

- Load-bearing pains: bash/wrapper maintenance and harmonia/nix-store being wrong for what is
  really a dumb mirror. Disk growth (no GC) and the stuck-generation retry loop were acute but
  fixable in place — they did not justify the rewrite alone.
- Live failure at decision time: ~13 generations looping "incomplete, will retry" every 5 min
  because manifests reference `.drv` paths never uploaded to R2. The v2 design removes the drv
  flow entirely, so this failure class is structurally gone, not patched.
- Key insight: consumers never substitute `.drv` from a cache — `nixos-rebuild` evaluates locally
  and regenerates drvs for free. The drv mirroring existed only so the box could expand build
  closures without nix eval. Manifest-carried closure lists replace it.

## Locked decisions

1. **One static Rust binary**, server + CLI subcommands. tvix `nix-compat` crate covers
   narinfo/signature parsing. Big-bang cutover: wipe the box, redeploy; downtime tolerable.
2. **Box store = flat binary-cache layout on disk** (`<storehash>.narinfo`, `nar/<filehash>.nar.*`,
   `roots/…`), same layout as the R2 bucket. Filesystem is the database: in-memory index built by
   scanning narinfos at boot. No nix on the box, no sqlite. Objects stored byte-identical as
   received; no re-compression.
3. **Manifest v3**: full build-closure store-path list plus `flake`, `gen`, `timestamp`, and
   explicit `branch` + `attr` fields (never parse the gen-id). CI enumerates the closure at emit
   time (eval already done; path lists are cheap even when NARs don't fit CI disk). Roots-only
   `{outPath, drvPath}` (v2) is dead; `.drv` uploads are unnecessary.
4. **Mirror-down = dumb fetch** of listed paths: try the remote cache first, upstream caches
   (cache.nixos.org) for the rest. Box holds the full build closure (the LAN-speed product);
   R2 stores only what writers push. Missing paths are expected steady state, not errors:
   fetch-what-exists, record per-path misses, re-probe quietly each cycle, report
   "synced, N gaps" — one status line, never a failing unit or retry-forever loop.
5. **Push = HTTP PUT** (`nix copy --to http://box`), bearer-token auth. sshd/ssh-ng removed.
   Box verifies every ingested narinfo signature (push and mirror) against trusted public keys;
   box holds no signing key (ADR-0004 invariant).
6. **Mirror-up = everything R2 lacks** from local-origin generations (matches current CI
   behavior). Local-origin generations are those whose manifest arrived via the authenticated
   push API, tracked in state files beside the store. Guard: a locally-pushed gen is never
   GC-eligible until its manifest is confirmed present in R2.
7. **GC policy carried over verbatim from `handoff-gc.md`**: retain if `age < M` OR `< N` newer
   gens in the `(branch, attr)` group; defaults main `N=5, M=4wk`, non-main `N=1, M=1wk`; box
   marks newest `3` (main) / `1` (non-main) and deletes no manifests; 24h grace window skips
   young objects; manifest-published-last ordering. **Simplified mark step**: mark set = union of
   retained manifests' closure lists — no transitive narinfo walk, no drv handling. Live nar keys
   still come from retained narinfos' `URL` fields.
8. **Remote sweep runs in CI** (scheduled GitHub Actions) using the same binary
   (`kasha gc --remote` or similar), with delete-capable credentials the box never holds.
   Box GC runs in-process on a timer.
9. **Cutover in the same R2 bucket**: v3 manifests under a new prefix (or version field). No
   migration code — the first remote sweep marks only from v3 manifests, so v2 manifests,
   orphaned `.drv` objects, and the broken generations are swept as ordinary garbage.
10. **Serve = local-only substituter**: 404 on miss, consumer's static substituter list falls
    back to remote/upstream. No pull-through. Reads unauthenticated on LAN; writes need the token.
11. **Observability**: structured logs + one `/status` JSON endpoint (per-flake last sync, gap
    count, store size, pending mirror-up). No Prometheus /metrics in v1.
12. **Deployment**: OCI image with the static binary is the only box artifact; the box NixOS
    module is dropped. The consumer NixOS module (substituter list + connect-timeout) stays.
    Repo stays `kasha`, rewrite on a branch; flake kept for devshell/CI checks. Integration
    test = a real nix client (`nix copy` push, substitute pull) against the server in a
    VM/container check.

## Deliberately deferred (revisit on evidence, in rough priority)

- **Re-compression** — explore options later (owner interest): store currently keeps bytes
  as-received; candidates include recompressing xz→zstd for LAN throughput, or normalizing
  compression at mirror-up. Needs measurement before any design.
- **Mirror-up filtering** — push everything for now; smarter logic later (e.g. skip paths
  upstream already serves, via signature check or upstream HEAD probe). If sig-filtering is
  ever adopted, the CLI push must sign only paths lacking a trusted sig, not the whole closure.
- Pull-through serving on miss.
- Prometheus /metrics.
- Selection shim / discovery backends (`docs/cache-roadmap.md`).

## Suggested build order

1. Core types + store: narinfo parse/verify, flat store scan/read/write, in-memory index.
2. HTTP server: binary cache GET endpoints + PUT ingest with sig verification + `/status`.
3. Manifest v3 emit (CLI) + mirror-down worker with the gap policy.
4. Mirror-up worker + local-origin tracking + not-yet-mirrored GC guard.
5. GC: retention selector + box sweep + remote sweep subcommand; CI workflow for the remote sweep.
6. OCI image, integration check, cutover: wipe box volume, repoint CI to v3 manifest emission,
   first remote sweep cleans v2 leftovers.

## Suggested skills

- `grilling` — re-grill any new fork before coding.
- `ponytail` — this design deliberately avoided sqlite, drv parsing, pull-through, re-compression,
  and mirror-up filtering. Don't reintroduce without evidence.
- `tdd` — retention selector, narinfo verification, and gap policy are branch-heavy; build
  test-first with fixture inputs.
- `caveman-commit` — repo commit style.

No secrets in this doc. Real bucket/endpoint/token values live in deployment config, not here.
