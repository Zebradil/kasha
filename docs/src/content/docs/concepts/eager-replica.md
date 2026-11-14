---
title: How the box works
description: The box is an eager bidirectional replica of the remote cache, stored as plain binary-cache files.
---

A **box** is a binary cache reachable from one physical network, such as a home LAN. It sits between the hosts on that
network and the **remote cache**, the durable S3-compatible cache every environment can reach. The box's job is to move
the slow remote hop off the interactive path: when a host asks for a path, the box should already have it.

## Why eager, not pull-through

A pull-through proxy fetches a path the first time someone asks for it. On a network with a single consumer that
retains what it builds, every request is a first request, so a pull-through box pays the full remote latency every
time and is no faster than reading the remote cache directly.

The box is instead an **eager bidirectional replica**. It pulls new generations *down* before anyone asks for them, and
mirrors locally pushed generations *up*. Neither direction waits on a client request. A read the box cannot answer gets
`404`; the host's next substituter, the remote cache, answers instead.

Rationale: [ADR-0001](https://github.com/Zebradil/kasha/blob/main/docs/adr/0001-eager-bidirectional-replica.md).

## Pull-through on a miss

That argument assumes one consumer that keeps what it fetched. A box that serves many short-lived consumers is
different: ephemeral build VMs start every job with an empty store and fetch the same paths again and again, and most
of those paths come from public caches that no generation manifest lists. For such a box the first request is the first
of many, so kasha has an optional **pull-through** mode (`KASHA_PULL_THROUGH`, off by default). It runs beside the
eager replica and replaces nothing.

With the mode on, a `GET` for a narinfo the box lacks goes through these steps:

1. The box asks its sources in order: the remote cache when it has one, then each upstream in `KASHA_UPSTREAMS`. The
   first narinfo that passes the [trust check](../trust-model/) and names the requested path wins.
2. The box answers at once with its own rendering of that narinfo: the same fields and signatures, a `URL` it derives
   from `NarHash`, and no `FileHash` or `FileSize` yet.
3. The NAR is downloaded from the same source in the background, on a pool of `KASHA_PULL_THROUGH_WORKERS` workers. A
   NAR a client is already waiting for is downloaded ahead of the others.
4. The box decompresses the NAR and checks it against `NarHash` and `NarSize`. Only then does it store the NAR, and after
   it the narinfo, now with the stored file's `FileHash` and `FileSize`.

A `GET` for a NAR that is being downloaded waits and is served when the download completes. Nix asks for the narinfos
of a whole closure before it downloads anything, so answering them early lets the box start every download while the
client is still asking.

Everything that does not end in a stored path is a `404`, which sends Nix to its next substituter or to a build:

- No source has the path. The box remembers that for `KASHA_PULL_THROUGH_NEGATIVE_TTL` seconds (60 by default) and
  answers without asking again.
- A source is down or too slow, a NAR fails its check, or the disk is full. Nothing is remembered; the next request
  tries again.
- More than `KASHA_PULL_THROUGH_MAX_PENDING` paths are answered but not stored yet.
- Half of `KASHA_MAX_INFLIGHT` requests already wait on a source, for a narinfo lookup or for a NAR download. The other
  half stays free for requests the store can answer.

Only a `GET` for a narinfo starts a fetch; `HEAD` reports what the store holds. Two Nix commands meet that rule in ways
worth knowing:

- **`nix copy --to` a pull-through box** asks for the narinfo of every path with a `GET` before it uploads anything. The
  box pulls each path one of its sources holds, and Nix skips those; only paths no source has are uploaded. If such a
  download then fails, the box lacks a path the writer believes it pushed, until mirror-down fills the gap or a later
  push brings it.
- **`nix copy --from` a pull-through box** first asks whether the path is valid with a `HEAD`, so it sees only paths
  the box already stores. Substitution, which is what a build does, asks with a `GET`.

A path that a push or mirror-down stores first is kept as that route stored it, and the download is discarded.
Pull-through never replaces a stored object and never triggers mirror-up.

Two costs come with the mode. A client waiting for a NAR receives no bytes until the box has all of it; for a NAR that
takes the box longer than Nix's `stalled-download-timeout` (300 seconds by default), Nix gives up and retries. And a
narinfo that was asked for is downloaded and stored even if no client ever requests its NAR.

Set it up: [Deploy a box](../../guides/deploy-box/#6-serve-short-lived-consumers-with-pull-through). Rationale:
[ADR-0011](https://github.com/Zebradil/kasha/blob/main/docs/adr/0011-optional-pull-through.md).

## The store is the database

On disk the box holds the same flat binary-cache layout as the remote bucket:

| Path | Content |
| --- | --- |
| `<hash>.narinfo` | one narinfo per store path, byte-identical as received; for a pulled-through path, the box's rendering |
| `nar/…` | the NAR files narinfos point to |
| `roots/<flake>/<gen>.json` | generation manifests |
| `state/local-origin/`, `state/mirrored/` | markers for locally pushed generations |

There is no Nix and no database on the box. At boot it rebuilds an in-memory index by scanning the narinfos. Objects
are never recompressed.

## The sync cycle

One worker thread runs the cycle, then sleeps `KASHA_SYNC_INTERVAL` seconds (300 by default). It runs only when the box
has a remote cache.

1. **Mirror-down** lists `roots/` in the remote cache and stores every new valid v3 manifest. A manifest that has
   disappeared from the remote is dropped locally too, which is how remote retention reaches the box. Then, for every
   manifest, the box fetches each listed path it lacks: from the remote cache first, then from each upstream
   (`https://cache.nixos.org` by default). A narinfo and its NAR always come from the same source. A path no source has
   is a **gap**: counted in `/status`, retried at most once an hour, never an error.
2. **Mirror-up** takes every **local-origin** generation (one whose manifest was pushed to the box) that has not been
   mirrored yet, and uploads what the remote lacks: each NAR, then its narinfo, and the manifest last, so remote readers
   only ever see complete generations. It waits while any listed path is missing on the box.
3. **Box GC** runs when `KASHA_GC_INTERVAL` has passed since the last sweep, or `kasha sweep` runs it on demand; see
   [Retention and GC](../retention/).

Mirror-down is a dumb fetch of what the manifest lists: the box never expands closures or builds anything. That is why
manifests carry the full build closure; see [Generation manifests](../generation-manifest/).

Every narinfo the box stores, from a push or from mirror-down, passes the same trust check; see
[Trust model](../trust-model/).

Design record: [ADR-0008](https://github.com/Zebradil/kasha/blob/main/docs/adr/0008-single-rust-binary.md).
