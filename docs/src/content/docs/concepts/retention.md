---
title: Retention and GC
description: How kasha decides which generations to keep, and how the box and remote sweeps delete the rest.
---

Retention works on generations, not on individual store paths. The presence of a manifest *is* the decision to keep a
generation: there is no separate ledger. Garbage collection then deletes every object no retained manifest reaches.

## The retention rule

Generations are grouped by `(flake, branch, attr)`. Within a group, a generation is kept when it is among the `N`
newest (by manifest `timestamp`), or younger than `M`. The branch `main` gets its own `N` and `M`; every other branch
shares the other tier.

A group goes **stale** when its newest generation trails the newest generation of the same flake and tier by `S` or
more. A stale group keeps only what the `M` rule keeps. This retires groups the flake has moved past: a renamed or
removed attr, an old release tag, a one-off branch name. Staleness is measured against the flake, not the clock, so a
flake that stops publishing keeps its newest generations; and against the tier, so pull-request churn never makes
`main` stale.

| Sweep | `main` | other branches | Stale after | Where it runs |
| --- | --- | --- | --- | --- |
| Remote | `N=5`, `M=4 weeks` | `N=1`, `M=1 week` | `S=4 weeks` | `kasha gc`, from CI, with delete-capable credentials |
| Box | `N=3`, count only | `N=1`, count only | never | inside `kasha serve`, every `KASHA_GC_INTERVAL`; on demand with `kasha sweep` |

The remote defaults can be overridden per run with `kasha gc` flags, and `kasha ls` shows each generation's verdict
under them; the box policy is fixed. Because the box keeps the
newest 3 or 1 of what the remote keeps, and never more, a box never retains a generation the remote cache has dropped.

## Marking and sweeping

Both sweeps are mark-and-sweep:

1. Pick the retained manifests.
2. Mark every store path in their closures. The mark set is the union of the listed paths; the sweep never walks
   narinfo references.
3. Mark the NARs the marked narinfos point to.
4. Delete every unmarked narinfo and NAR, except objects younger than the grace window (24 hours). The box sweep also
   keeps the NAR of every narinfo it keeps, however old the NAR is, and under a size cap it keeps every unmarked
   narinfo: [eviction](#unlisted-paths-and-the-size-cap) decides for those.

The remote sweep also deletes the manifests retention did not keep, and anything under `roots/` that is not a valid v3
manifest. The box sweep deletes no manifests: mirror-down drops a local manifest once it disappears from the remote, so
remote retention reaches the box through sync.

## Safety without locks

Writers publish a manifest after every path it lists, and the grace window skips any object younger than 24 hours. A
sweep running during a push therefore leaves the push's objects alone until the manifest lands and marks them. Re-running
a sweep is harmless: it deletes what the previous run missed.

Every narinfo written to the box makes the NAR it names young, and inside `kasha serve` each NAR removal is decided
under the same lock as that write. So a sweep or an eviction pass never removes a NAR that gained a narinfo after it
listed the store.

`kasha sweep` runs the box sweep from a separate process, next to a running `kasha serve`, so the server's in-memory
index cannot see its deletions directly. The sweep stamps `state/last-sweep` when it finishes, and at the start of its
next sync cycle the server drops index entries whose narinfo is gone from disk. A deleted path that a manifest still
lists then shows up as a gap again, and mirror-down fetches it back.

Two more guards protect data only the box holds:

- A **local-origin** generation (its manifest was pushed to the box) is kept, whatever the policy says, until mirror-up
  has uploaded it to the remote cache.
- A box with no manifests at all skips its sweep. An empty mark set would delete the whole store, and an unsynced store
  (a fresh volume, a restored backup) is not a retention decision.

## Unlisted paths and the size cap

An **unlisted** path is one no retained manifest lists: a path [pull-through](../eager-replica/#pull-through-on-a-miss)
stored, a path pushed without a manifest, or a path whose generation retention dropped. Generation retention cannot
decide for these. What happens to them depends on `KASHA_UNLISTED_MAX_BYTES`, on `kasha serve` and `kasha sweep`.

**Unset** (the default), the box behaves as it always did:

- **With manifests** (a remote cache, or generations pushed to it): the box sweep deletes an unlisted path once it is
  past the 24-hour grace window, however often it is read. The next request pulls it through again.
- **Without manifests**: the sweep is skipped, and without `KASHA_REMOTE` the GC timer does not run at all. Nothing is
  ever deleted, the store grows until the volume is full, and anyone who can reach the box can fill it.

**Set**, the box keeps unlisted paths within that many bytes and evicts the least recently used first, with or without
a remote cache or manifests:

- **What counts.** File sizes: each unlisted narinfo, and each NAR only unlisted narinfos name, once however many name
  it. A NAR that a listed path names counts toward nothing and is never evicted. An old `.nar.xz` and a `.nar.zst` of
  the same NAR are two files.
- **What use is.** A `GET` or `HEAD` of a narinfo or NAR, or storing it. The box records it as the file's modification
  time, rewritten at most once a minute per file (and at once if it lies in the future); a path was last used when its
  narinfo or its NAR was. The record is on disk, so a restart loses nothing. A client that probes every path, with
  `HEAD` or narinfo `GET`s, keeps every path recent. If the box cannot set times on its files (it does not own them),
  it logs a warning once and eviction goes by when files were stored.
- **When it runs.** Inside `kasha serve`, with or without a remote cache: every 5 minutes while NAR bytes arrive or the
  last pass left work, hourly on a quiet box, and within 5 seconds once the NAR bytes pulled through or pushed since the
  last pass could take the unlisted paths over the cap. `kasha sweep` runs a pass too.
- **How far.** Over the cap, a pass removes the least recently used until 90% of the cap is left, so the next pass is
  not due at the next path stored. A path used in the last 10 minutes is never evicted, so the store can exceed the cap
  by the paths stored or read in the last 10 minutes; eviction does not look at free disk space.
- **Who may get a `404`.** A client that fetched a narinfo and asks for its NAR more than 10 minutes later: one
  substituting a large closure queries every narinfo before it downloads, and a long-lived consumer caches positive
  narinfos for 30 days. If the path was evicted meanwhile, the NAR request gets `404` and Nix falls back to its next
  substituter.
- **Generation retention.** Listed paths are kept as before. Unlisted narinfos are no longer swept by age. A path whose
  last retained manifest goes keeps the time of its last use, so an unused one is first in line.

A narinfo is always removed before its NAR, and a NAR only once no remaining narinfo names it. A path whose NAR a
pull-through fetch is producing or reusing is left whole. Mirror-down writes a narinfo only if the NAR it counted on is
still there, and keeps the path a gap otherwise. So inside `kasha serve` no deletion leaves a narinfo without its NAR;
`kasha sweep` in a separate process shares no lock with `serve` and keeps the narrow window the sweep always had. A NAR
no narinfo names goes after the grace window, as in the sweep. A removal that fails is logged and stops the pass: the
narinfo already removed leaves its NAR behind for the grace window, and the next pass tries again. Requests never wait
on a pass.

A manifest that lists a pulled-through path retains it like any other path: the sweep looks at what manifests list,
not at how a path arrived.

## Why it is this simple

R2, the reference remote, charges nothing for deletes and little for requests; stored gigabytes are the only real cost.
So the sweep is infrequent and straightforward (no locks, no tombstones) rather than clever.

Run the remote sweep: [Run a remote GC sweep](../../guides/remote-gc/). Rationale:
[ADR-0008](https://github.com/Zebradil/kasha/blob/main/docs/adr/0008-single-rust-binary.md).
