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
`404`; the host's next substituter, the remote cache, answers instead. The box never proxies.

Rationale: [ADR-0001](https://github.com/Zebradil/kasha/blob/main/docs/adr/0001-eager-bidirectional-replica.md).

## The store is the database

On disk the box holds the same flat binary-cache layout as the remote bucket:

| Path | Content |
| --- | --- |
| `<hash>.narinfo` | one narinfo per store path, byte-identical as received |
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
