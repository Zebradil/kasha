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
   from `NarHash`, `Compression: zstd`, and no `FileHash` or `FileSize` yet.
3. The NAR is downloaded from the same source in the background, on a pool of `KASHA_PULL_THROUGH_WORKERS` workers. A
   NAR a client is already waiting for is downloaded ahead of the others.
4. The box decompresses the NAR and checks it against `NarHash` and `NarSize`. Only then does it store the NAR, as zstd
   whatever the source's encoding, and after it the narinfo, now with the stored file's `FileHash` and `FileSize`. A
   NAR the source serves as zstd is stored as received.

A `GET` for a NAR that is being downloaded waits and is served when the download completes. Nix asks for the narinfos
of a whole closure before it downloads anything, so answering them early lets the box start every download while the
client is still asking.

Everything that does not end in a stored path is a `404`, which sends Nix to its next substituter or to a build:

- No source has the path. The box remembers that for `KASHA_PULL_THROUGH_NEGATIVE_TTL` seconds (60 by default) and
  answers without asking again.
- A source is down or too slow, a NAR fails its check, or the disk is full. No miss is remembered; the next request
  tries again. A source whose narinfo request failed is skipped for 30 s, then one lookup tries it again, so a dead
  source does not hold up every lookup for `KASHA_PULL_THROUGH_LOOKUP_TIMEOUT`.
- More than `KASHA_PULL_THROUGH_MAX_PENDING` paths are answered but not stored yet.
- A quarter of `KASHA_MAX_INFLIGHT` requests already wait on a narinfo lookup, or a quarter already wait for a NAR
  download. Each kind has its own cap, and the other half stays free for requests the store can answer.

Requests for the same missing path share one lookup and its outcome, a failed one included.

Only a `GET` for a narinfo starts a fetch; `HEAD` reports what the store holds. Two Nix commands meet that rule in ways
worth knowing:

- **`nix copy --to` a pull-through box** asks for the narinfo of every path with a `GET` before it uploads anything. The
  box pulls each path one of its sources holds, and Nix skips those; only paths no source has are uploaded. If such a
  download then fails, the box lacks a path the writer believes it pushed. On a box with a remote cache, mirror-down
  fills the gap once a manifest lists the path; without one, only a later push brings it.
- **`nix copy --from` a pull-through box** first asks whether the path is valid with a `HEAD`, so it sees only paths
  the box already stores, unless the client's own narinfo cache already holds the path from an earlier lookup.
  Substitution, which is what a build does, asks with a `GET`.

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
| `<hash>.narinfo` | one narinfo per store path, byte-identical as received; for a pulled-through or re-encoded path, the box's rendering |
| `nar/…` | the NAR files narinfos point to, as received; for a pulled-through or re-encoded path, zstd |
| `roots/<flake>/<gen>.json` | generation manifests |
| `state/local-origin/`, `state/mirrored/` | markers for locally pushed generations |

There is no Nix and no database on the box. At boot it rebuilds an in-memory index by scanning the narinfos.
Pull-through stores every NAR as zstd, because zstd decodes several times faster than xz on every consumer. Pushed and
mirrored objects keep the encoding they arrived in, unless `KASHA_REENCODE` is on: then the box re-encodes each stored
`xz`, `bzip2` or uncompressed NAR as zstd once it has a narinfo, in the background and before mirror-up uploads it (see
[the re-encoder](#re-encoding-stored-nars)). Rationale:
[ADR-0012](https://github.com/Zebradil/kasha/blob/main/docs/adr/0012-pulled-through-nars-as-zstd.md),
[ADR-0015](https://github.com/Zebradil/kasha/blob/main/docs/adr/0015-reencode-stored-nars-as-zstd.md).

### Re-encoding stored NARs

With `KASHA_REENCODE=true` the housekeeping thread, which runs on every box, takes a pass every 5 minutes over the
narinfos whose NAR file is not named `.nar.zst`, decoding at most 1 GiB of NARs per pass. For each it decodes the NAR,
checks it against `NarHash` and `NarSize`, stores it as zstd under `nar/<NarHash>.nar.zst` (the name pull-through
uses), and replaces the narinfo with the box's rendering, which keeps every signature. A narinfo that changed meanwhile,
by a push or mirror-down, is not replaced. A NAR that fails its check is left as it is and not tried again until the box
restarts.

A path a retained manifest lists is always re-encoded. A path no manifest lists is re-encoded only when it was used in
the last 12 hours, and its new files keep its last use, so re-encoding never makes a path look recently used to
eviction.

The old file is not deleted by the re-encoder. It stays for 24 hours after the narinfo is replaced, so a client that
read the old narinfo just before keeps getting it; then orphan removal takes it: with `KASHA_UNLISTED_MAX_BYTES` set, the
eviction pass removes it on any box; with a remote cache and at least one manifest, the box sweep does. A box with
neither keeps the old files.

## The sync cycle

One worker thread runs the cycle, then sleeps `KASHA_SYNC_INTERVAL` seconds (300 by default). It runs only when the box
has a remote cache.

1. **Mirror-down** lists `roots/` in the remote cache and stores every new valid v3 manifest. A manifest that has
   disappeared from the remote is dropped locally too, which is how remote retention reaches the box. Then, for every
   manifest the box retains (see [Retention and GC](../retention/)), it fetches each listed path it lacks: from the
   remote cache first, then from each upstream (`https://cache.nixos.org` by default). A narinfo and its NAR always come
   from the same source. A path no source has is a **gap**: counted in `/status`, retried at most once an hour, never an
   error. The remote keeps more generations than the box, and the box fetches none of the extra ones: the next box
   sweep would delete them.
2. **Mirror-up** takes every **local-origin** generation (one whose manifest was pushed to the box) that has not been
   mirrored yet, and uploads what the remote lacks: each NAR, then its narinfo, and the manifest last, so remote readers
   only ever see complete generations. It waits while any listed path is missing on the box.
3. **Box GC** runs when `KASHA_GC_INTERVAL` has passed since the last sweep, or `kasha sweep` runs it on demand; see
   [Retention and GC](../retention/).

A second thread runs on every box, remote cache or not: it removes temp files a killed writer left behind, and with
`KASHA_UNLISTED_MAX_BYTES` set it evicts the least recently used paths no manifest lists; see
[Retention and GC](../retention/#unlisted-paths-and-the-size-cap).

Mirror-down is a dumb fetch of what the manifest lists: the box never expands closures or builds anything. That is why
manifests carry the full build closure; see [Generation manifests](../generation-manifest/).

Every narinfo the box stores, from a push or from mirror-down, passes the same trust check; see
[Trust model](../trust-model/).

Design record: [ADR-0008](https://github.com/Zebradil/kasha/blob/main/docs/adr/0008-single-rust-binary.md).
