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
   keeps the NAR of every narinfo it keeps, however old the NAR is.

The remote sweep also deletes the manifests retention did not keep, and anything under `roots/` that is not a valid v3
manifest. The box sweep deletes no manifests: mirror-down drops a local manifest once it disappears from the remote, so
remote retention reaches the box through sync.

## Safety without locks

Writers publish a manifest after every path it lists, and the grace window skips any object younger than 24 hours. A
sweep running during a push therefore leaves the push's objects alone until the manifest lands and marks them. Re-running
a sweep is harmless: it deletes what the previous run missed.

`kasha sweep` runs the box sweep from a separate process, next to a running `kasha serve`, so the server's in-memory
index cannot see its deletions directly. The sweep stamps `state/last-sweep` when it finishes, and at the start of its
next sync cycle the server drops index entries whose narinfo is gone from disk. A deleted path that a manifest still
lists then shows up as a gap again, and mirror-down fetches it back.

Two more guards protect data only the box holds:

- A **local-origin** generation (its manifest was pushed to the box) is kept, whatever the policy says, until mirror-up
  has uploaded it to the remote cache.
- A box with no manifests at all skips its sweep. An empty mark set would delete the whole store, and an unsynced store
  (a fresh volume, a restored backup) is not a retention decision.

## Pulled-through paths

A path that [pull-through](../eager-replica/#pull-through-on-a-miss) stored is in no manifest, and generation retention
is all the box has today. Until eviction by use under a size cap exists, a pull-through box behaves like this:

- **With manifests** (a remote cache, or generations pushed to it): the box sweep deletes a pulled-through path once it
  is past the 24-hour grace window, however often it is read. The next request pulls it through again.
- **Without manifests**: the sweep is skipped, and without `KASHA_REMOTE` the GC timer does not run at all. Nothing is
  ever deleted, the store grows until the volume is full, and anyone who can reach the box can fill it. Size the volume
  for it, restrict who can reach the box, or clear the volume on a schedule.

A manifest that lists a pulled-through path retains it like any other path: the sweep looks at what manifests list,
not at how a path arrived.

## Why it is this simple

R2, the reference remote, charges nothing for deletes and little for requests; stored gigabytes are the only real cost.
So the sweep is infrequent and straightforward (no locks, no tombstones) rather than clever.

Run the remote sweep: [Run a remote GC sweep](../../guides/remote-gc/). Rationale:
[ADR-0008](https://github.com/Zebradil/kasha/blob/main/docs/adr/0008-single-rust-binary.md).
