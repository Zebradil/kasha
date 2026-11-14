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
the interval.

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
  ghcr.io/zebradil/kasha-box:edge
```

That box has no remote cache: it pulls through from `KASHA_UPSTREAMS`, `https://cache.nixos.org` by default. With
`KASHA_REMOTE` set, the remote cache is asked first, at the cost of one more round trip before each upstream lookup.
Consumers can list the box as their only substituter, or put it first.

| Variable | Default | Effect |
| --- | --- | --- |
| `KASHA_PULL_THROUGH` | off | `true` turns the mode on |
| `KASHA_PULL_THROUGH_NEGATIVE_TTL` | `60` | seconds a path no source has is answered `404` without asking again; `0` asks every time |
| `KASHA_PULL_THROUGH_WORKERS` | `8` | concurrent NAR downloads |
| `KASHA_PULL_THROUGH_MAX_PENDING` | `4096` | cap on paths answered but not stored yet, queued downloads included; past it a narinfo miss is `404` |
| `KASHA_PULL_THROUGH_LOOKUP_TIMEOUT` | `10` | seconds a source gets to accept a connection and to answer a narinfo lookup |
| `KASHA_PULL_THROUGH_STALL_TIMEOUT` | `60` | seconds a NAR download may receive nothing before it is dropped |

Before you enable it:

- **Add every upstream's key** to `KASHA_TRUSTED_KEYS`. Without it the box takes only content-addressed paths from that
  upstream and answers `404` for the rest.
- **Plan the disk.** Nothing evicts pulled-through paths by use yet, and anyone who can reach the box can make it
  download what its sources serve. A box without `KASHA_REMOTE` never sweeps on its own and grows until the volume is
  full. See [Retention and GC](../../concepts/retention/#pulled-through-paths).
- **Size `KASHA_MAX_INFLIGHT` for waiting clients.** A request that waits on a source holds a slot until the source
  answers: a narinfo request during its lookup, a NAR request until the download ends. At most half of the slots may be
  held that way; further such requests get `404`. Nix opens up to 25 connections per client.
- **Expect pushes to pull.** `nix copy --to` this box makes it pull every path a source holds instead of receiving it;
  see [How the box works](../../concepts/eager-replica/#pull-through-on-a-miss).
- **Raise `KASHA_PULL_THROUGH_MAX_PENDING`** if single jobs need closures of more paths than the cap. The paths past it
  are not lost: Nix fetches them from its next substituter, and the next job asks the box again.

:::caution[Unverified]
The `docker run` above was not run with the mode on. The NixOS VM test in `tests/v2.nix` runs a pull-through box in
front of another box, with a Nix client that substitutes through it.
:::
