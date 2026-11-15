---
title: Deploy a box
description: Run the kasha box image on your network with a persistent store and a remote cache to mirror.
---

The box is one container running the static `kasha` binary. It serves the store over HTTP, accepts authenticated pushes,
and, once it has a remote cache, mirrors generations both ways and garbage-collects itself.

## Prerequisites

- A container runtime (Docker, k3s, …) on a machine with a stable LAN address.
- The public key(s) your producers sign with, in `name:base64` form.
- Optional, for mirroring: an S3-compatible bucket that holds your remote cache, and read/write credentials for it.
- A store volume whose filesystem supports hard links: mirror-down and pull-through create NAR files by hard link, so
  that an existing file is never replaced.

## 1. Pick an image tag

The image is `ghcr.io/zebradil/kasha-box`. CI publishes these tags:

| Tag | Follows | Use |
| --- | --- | --- |
| `edge` | every push to `main` | tracking the latest build |
| `vX.Y.Z` | release tags | reproducible deployments |
| `sha-<12-char commit>` | every published build | pinning an exact commit |

## 2. Choose the trusted keys

The box stores a narinfo only if it carries a signature from a key in `KASHA_TRUSTED_KEYS`, or a `CA` field that
reproduces its store path. This applies to pushes, to everything mirror-down fetches and to everything pulled through,
including paths from the upstream caches. To let the box take paths from `https://cache.nixos.org`, include its key
too:

```sh
KASHA_TRUSTED_KEYS='znix.zebradil.dev:AAAA... cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY='
```

Keys are separated by commas or spaces. The box refuses to start without at least one.

## 3. Run the container

Mount a persistent volume at `/kasha` (the store root) and publish port 5000:

```sh
docker run -d -p 5000:5000 -v kasha-data:/kasha \
  -e KASHA_TRUSTED_KEYS='znix.zebradil.dev:AAAA...' \
  -e KASHA_TOKEN='...' \
  -e KASHA_REMOTE='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
  -e AWS_ACCESS_KEY_ID=... -e AWS_SECRET_ACCESS_KEY=... \
  ghcr.io/zebradil/kasha-box:edge
```

- `KASHA_TOKEN` is the write token. Leave it unset for a read-only box: every write is then refused.
- `KASHA_REMOTE` enables mirroring and box GC. The URL names a bare bucket and must carry `endpoint`; a host without a
  scheme gets `https://`, and `region` defaults to `auto`.
- The AWS credentials need read and write on the bucket, never delete. Deleting is the
  [remote GC sweep](../remote-gc/)'s job, which runs elsewhere.

The store is flat files with no database, so any storage class works for the volume. On k3s, run the box as an
always-on workload with a stable `LoadBalancer` or `NodePort` address and its data on a PVC mounted at `/kasha`.

:::caution[Unverified]
The `docker run` above needs real keys and bucket credentials and was not run while writing this page. CI runs a
smoke test of the image with only `KASHA_TRUSTED_KEYS` set (`.github/workflows/oci.yml`).
:::

## 4. Tune the workers

Every setting is an environment variable and a `kasha serve` flag; the [reference](../../reference/cli/kasha-serve/)
lists them with defaults. The ones you may want to change:

| Variable | Default | Effect |
| --- | --- | --- |
| `KASHA_UPSTREAMS` | `https://cache.nixos.org` | caches mirror-down and pull-through try after the remote, comma separated |
| `KASHA_SYNC_INTERVAL` | `300` | seconds to sleep between sync cycles (mirror-down, then mirror-up) |
| `KASHA_GC_INTERVAL` | `86400` | minimum seconds between box GC sweeps; the last sweep time survives restarts |
| `KASHA_MAX_INFLIGHT` | `256` | cap on concurrent requests; a slow download holds its slot until it ends |
| `KASHA_PRIORITY` | `10` | `Priority` advertised in `nix-cache-info`; Nix asks substituters with a lower number first |

`KASHA_HTTP_THREADS` is retired: the box logs a warning when it is set and ignores it. Use `KASHA_MAX_INFLIGHT`.

To sweep now instead of waiting for the interval, run `kasha sweep` inside the running container, for example
`kubectl exec deploy/kasha -- /bin/kasha sweep`. It reads the same `KASHA_DATA`, prints each deleted object, and resets
the interval. With `KASHA_UNLISTED_MAX_BYTES` set it also runs an eviction pass and prints each evicted object.

## 5. Check it

```sh
curl -s http://box.lan:5000/nix-cache-info
curl -s http://box.lan:5000/status
```

After its first sync cycle, a box with a remote reports `last_sync` for each flake it holds manifests for. See [Monitor a box](../monitor-box/) for
what the fields mean.

Next, [point hosts at the box](../consumer-host/).

## 6. Serve short-lived consumers with pull-through

Skip this for a box with one long-lived consumer per network: it keeps what it fetched, so the box has nothing to
save it. Turn pull-through on for a box whose consumers start empty again and again, such as one box per build host
serving ephemeral VMs. [How the box works](../../concepts/eager-replica/#pull-through-on-a-miss) describes what the mode
does.

```sh
docker run -d -p 5000:5000 -v kasha-data:/kasha \
  -e KASHA_TRUSTED_KEYS='cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=' \
  -e KASHA_PULL_THROUGH=true \
  -e KASHA_UNLISTED_MAX_BYTES=100000000000 \
  ghcr.io/zebradil/kasha-box:edge
```

That box keeps what it pulled through within 100 GB, evicting the least recently used paths first. It has no remote
cache: it pulls through from `KASHA_UPSTREAMS`, `https://cache.nixos.org` by default. With
`KASHA_REMOTE` set, the remote cache is asked first, at the cost of one more round trip before each upstream lookup.
Consumers can list the box as their only substituter, or put it first. With the box as the only substituter, every
`404` it answers means that consumer builds the path from source, or fails if it cannot: a path no source has, a source
that is down, and a request turned away because too many already wait. Keep a second substituter behind the box unless
that is what you want.

| Variable | Default | Effect |
| --- | --- | --- |
| `KASHA_PULL_THROUGH` | off | `true` turns the mode on |
| `KASHA_PULL_THROUGH_NEGATIVE_TTL` | `60` | seconds a path no source has is answered `404` without asking again; `0` asks every time |
| `KASHA_PULL_THROUGH_WORKERS` | `8` | concurrent NAR downloads |
| `KASHA_PULL_THROUGH_MAX_PENDING` | `4096` | cap on paths answered but not stored yet, queued downloads included; past it a narinfo miss is `404` |
| `KASHA_PULL_THROUGH_LOOKUP_TIMEOUT` | `10` | seconds a source gets to accept a connection and to answer a narinfo lookup |
| `KASHA_PULL_THROUGH_STALL_TIMEOUT` | `60` | seconds a NAR download may receive nothing before it is dropped; below `3600`, or the box refuses to start |
| `KASHA_UNLISTED_MAX_BYTES` | unset | bytes kept of paths no retained manifest lists, pulled-through ones included; the least recently used go first. Unset, nothing evicts them |

Before you enable it:

- **Add every upstream's key** to `KASHA_TRUSTED_KEYS`. Without it the box takes only content-addressed paths from that
  upstream and answers `404` for the rest.
- **Set a size cap.** Anyone who can reach the box can make it download what its sources serve. Without
  `KASHA_UNLISTED_MAX_BYTES`, a box without manifests never deletes a pulled-through path and grows until the volume is
  full; with manifests, the sweep deletes each a day after it arrived, however often it is read. Eviction does not
  look at free disk space, and it never evicts a path used in the last 10 minutes, so size the volume as at least:

  ```text
  volume ≥ retained generations
         + max(cap, bytes stored in 10 minutes + bytes of older paths read in 10 minutes)
         + NARs orphaned in the last 24 hours + downloads in flight
  ```

  For example, a box that can receive 1 Gb/s stores up to 75 GB in 10 minutes. See
  [Retention and GC](../../concepts/retention/#unlisted-paths-and-the-size-cap).
- **Budget CPU for the workers.** Each download is decoded to be checked and, unless the source serves zstd, encoded
  again as zstd, on one core per busy worker.
- **Size `KASHA_MAX_INFLIGHT` for NAR waits.** A request that waits on a source holds a slot until the source answers.
  A quarter of the slots may be held by narinfo requests during a lookup and another quarter by NAR requests waiting
  for a download; a request past its cap gets `404`. Lookups are short. NAR waits last as long as the download, and one
  Nix client waits on up to 16 at once (`max-substitution-jobs`). The default of 256 lets 64 NAR requests wait, which
  is four such clients; for more consumers fetching at the same time, set `KASHA_MAX_INFLIGHT` to at least 64 times
  their number.
- **Expect pushes to pull.** `nix copy --to` this box makes it pull every path a source holds instead of receiving it;
  see [How the box works](../../concepts/eager-replica/#pull-through-on-a-miss).
- **Raise `KASHA_PULL_THROUGH_MAX_PENDING`** if single jobs need closures of more paths than the cap. The paths past it
  are not lost: Nix fetches them from its next substituter, and the next job asks the box again.
