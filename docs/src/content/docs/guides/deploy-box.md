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
reproduces its store path. This applies to pushes and to everything mirror-down fetches, including paths from the
upstream substituters. To let the box fill gaps from `https://cache.nixos.org`, include its key too:

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
| `KASHA_UPSTREAMS` | `https://cache.nixos.org` | substituters mirror-down tries after the remote, comma separated |
| `KASHA_SYNC_INTERVAL` | `300` | seconds to sleep between sync cycles (mirror-down, then mirror-up) |
| `KASHA_GC_INTERVAL` | `86400` | minimum seconds between box GC sweeps; the last sweep time survives restarts |
| `KASHA_MAX_INFLIGHT` | `256` | cap on concurrent requests; a slow download holds its slot until it ends |

`KASHA_HTTP_THREADS` is retired: the box logs a warning when it is set and ignores it. Use `KASHA_MAX_INFLIGHT`.

## 5. Check it

```sh
curl -s http://box.lan:5000/nix-cache-info
curl -s http://box.lan:5000/status
```

After its first sync cycle, a box with a remote reports `last_sync` for each flake it holds manifests for. See [Monitor a box](../monitor-box/) for
what the fields mean.

Next, [point hosts at the box](../consumer-host/).
