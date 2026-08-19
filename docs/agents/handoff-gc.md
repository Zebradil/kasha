# Handoff: garbage collection for kasha + remote R2

Design agreed in a grilling session. Not yet built. This doc is the spec to implement.
It refines `docs/cache-roadmap.md` §"Garbage collection" and `docs/adr/0003-root-manifest-indexing.md`
(root manifests double as GC roots) — read both first. Consider promoting this to an ADR once built.

## Context the implementer needs

- R2 bucket layout = standard nix S3 binary cache: `<storehash>.narinfo`, `nar/<filehash>.nar.<ext>`,
  `.drv` paths stored as their own narinfo+nar, plus kasha's `roots/<flake>/<gen>.json`.
- Manifest shape today: `scripts/emit-root-manifest.sh` (version 2, `roots: [{outPath, drvPath}]`, roots only,
  never closures). gen-id today is the opaque string `<branch>-<shortsha>-<attr>`
  (e.g. `main-8a741bd-checks.x86_64-linux.zebradil-tuxedo-home-build`).
- Mirror workers: `scripts/mirror-down.sh`, `scripts/mirror-up.sh`. Both already publish the manifest LAST
  (after NARs land) and tolerate incomplete trees. GC relies on manifest-last ordering.
- Box never holds cross-system top-level outputs; only recipe (`.drv`) + partial input closures. See ADR-0003.

## Key facts that shaped the design (don't re-derive)

- **R2 pricing:** DELETE is **free**. LIST = Class A ($4.50/M, batched 1000/page). GET/HEAD = Class B ($0.36/M).
  No egress. Storage $0.015/GB-mo. => API ops are negligible at any realistic scale; **storage-GB is the only
  real cost**. This kills the roadmap's "list/delete uneven cost" worry — design for simple + infrequent, not clever.
- Manifest does NOT list closures. Remote mark set must be reconstructed by reading narinfos transitively,
  and MUST tolerate dangling references (partial CI builds leave gaps).
- `.narinfo` object key is deterministic from store hash (`<hash>.narinfo`); `nar/*` keys are content-hashed and
  are NOT derivable — you must read a retained narinfo's `URL` field to learn its live nar key.

## Decisions (locked)

1. **Retention source of truth = manifest presence.** Pruning `roots/<flake>/<gen>.json` IS the retention decision.
   Both GCs mark-sweep from surviving manifests. No separate ledger.
2. **Grouping key = (branch, attr).** Emit `branch` + `attr` as **explicit manifest fields** — do NOT parse the
   opaque gen-id (landmine on scheme change). Old manifests lacking fields: fall back to parse, or treat as main/keep.
3. **Policy:** retain a gen if `age < M` OR `fewer than N newer gens in its (branch,attr) group`.
   Defaults (tunable): main `N=5, M=4wk`; non-main `N=1, M=1wk`. Non-main uses age only (no git liveness query).
4. **Two-tier via two numbers over one manifest set:** only remote retention deletes manifests. Box GC deletes no
   manifests — it marks only the newest `3` (main) / `1` (non-main) per group. => box retained ⊆ remote retained,
   automatically. No starving the remote.
5. **Box GC** = generate gcroot symlinks (on present `outPath` + `drvPath`) → `nix-collect-garbage`. Nix does the
   closure math. **Guard:** never let a locally-pushed gen leave roots until mirror-up `.seen`/remote `roots/`
   confirms it mirrored up (box may hold the only copy — data loss otherwise).
6. **Remote mark set = read closures transitively from R2 narinfos** (Class B, dedup via visited set, tolerate
   dangling). Chose this over box-local `nix-store --requisites` — reads are cheap, avoids coupling remote GC to box
   store contents + a hybrid fallback branch. (Box-local is a future optimization only if read volume ever bites.)
7. **Remote sweep runs in CI** (scheduled GitHub Actions), NOT on the box. Global destructive op belongs in the
   controlled ephemeral runner; keeps box creds read/write-only (no bucket-wide delete on the LAN box); survives box
   downtime.
8. **Race safety:** manifest-published-last (already true) + GC reads `roots/` LAST (after listing the whole bucket)
   + 24h grace window (skip any object `LastModified < 24h`). Deletes free + sweep idempotent => no lock, no
   two-phase tombstone.
9. **Derivations need no special handling** — `.drv` is a narinfo+nar in the closure; retained via `drvPath` root,
   orphans swept like any garbage. Same pass covers NARs and derivations.

## Remote sweep algorithm

1. LIST whole bucket first (Class A, batched). Partition keys: `<h>.narinfo`, `nar/*`, `roots/*`.
2. THEN LIST `roots/`, apply N/M retention per (branch,attr) → surviving manifests.
3. Mark set = union of retained closures, read transitively from R2 narinfos' `References`, dedup, tolerate dangling.
4. Retain `.narinfo` iff `h ∈ mark` (deterministic, zero reads). Read each retained narinfo's `URL` → live nar keys.
5. Delete non-marked `.narinfo` + non-live `nar/*`, bulk DeleteObjects (free). Skip anything `LastModified < 24h`.

## Suggested build order

1. Add `branch` + `attr` explicit fields to `scripts/emit-root-manifest.sh` (independent).
2. Box GC script + systemd timer (daily) in `modules/box.nix` + mirror-up-confirmed guard (independent of 1).
3. Remote sweep script + scheduled CI workflow (depends on 1 for the grouping fields).

Follow repo conventions: bash tools are env-in/stdout-out with test seams (ADR-0006), VM/fixture tests under
`tests/`, `nix flake check` must stay green. Match existing script style (`mirror-up.sh` is the closest template
for the S3-URL-to-aws-flags parsing and the list/diff/state pattern).

## Suggested skills

- `grilling` — if any new fork opens during implementation, re-grill before coding.
- `ponytail` — enforce laziest-that-works; this design deliberately avoided a ledger, tombstone, and box-local
  closure path. Don't reintroduce them without evidence.
- `tdd` — mark/sweep and retention math are branch-heavy; build the remote sweep and retention selector test-first
  with fixture inputs (mirror scripts already use `KASHA_*_FILE` test seams — mirror that).
- `verify` — exercise box GC + remote sweep end-to-end (dry-run mode first) before committing.
- `caveman-commit` — repo commit style (Conventional Commits, terse).

No secrets in this doc. Real bucket/endpoint values live in deployment config, not here.
