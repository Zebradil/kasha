---
title: Run a remote GC sweep
description: Delete expired generations and unreferenced objects from the remote cache with kasha gc, by hand or from CI.
---

`kasha gc` sweeps the remote cache: it keeps the generations the retention policy retains, marks their closures, and
deletes everything else that is older than the grace window. It needs delete-capable credentials, so it runs from CI or
by hand, never on the box.

## Prerequisites

- S3 credentials that can list, read and delete in the remote cache's bucket, as `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY`.
- The remote cache URL, `s3://bucket?endpoint=…&region=…`, as `--remote` or `KASHA_REMOTE`.
- The `kasha` binary, or the `ghcr.io/zebradil/kasha-box` image (the image's entrypoint is `kasha`).

## 1. Dry run first

```sh
kasha gc --remote 's3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' --dry-run
```

stdout lists one `would delete <key>` line per object; nothing is deleted. Progress and the summary go to stderr, with
a progress line every 1000 objects. The summary logs `retained`, `deleted` and `skipped_young` counts.

The sweep deletes, in one pass over a full bucket listing:

- manifests under `roots/` that retention does not keep, and any object there that is not a valid v3 manifest;
- narinfos whose store path is in no retained manifest's closure;
- NARs that no retained narinfo points to.

Any object younger than the grace window (24 hours by default) is skipped and counted in `skipped_young`, so a push in
flight is never swept before its manifest lands.

## 2. Adjust retention if needed

A generation is kept when it is among the `N` newest in its `(flake, branch, attr)` group, or younger than `M` weeks.
The branch `main` has its own tier:

| Tier | Flags | Default |
| --- | --- | --- |
| `main` | `--main-keep`, `--main-age-weeks` | `N=5`, `M=4` |
| every other branch | `--other-keep`, `--other-age-weeks` | `N=1`, `M=1` |

`--grace-hours` sets the grace window. The box's own sweep keeps a subset of this (see
[Retention and GC](../../concepts/retention/)), so shrinking the remote policy also shrinks what boxes keep once they
next sync.

## 3. Run it for real

Drop `--dry-run`. Deletes go out in batches of up to 1000 keys, and stdout lists `deleted <key>` for each.

## Schedule it in GitHub Actions

`.github/workflows/gc.yml` runs the sweep daily at 04:00 UTC and on manual dispatch, with a `dry-run` checkbox. It
reads the credentials from the `KASHA_GC_ACCESS_KEY_ID` and `KASHA_GC_SECRET_ACCESS_KEY` secrets and the URL from the
`CACHE_S3_URL` variable, runs `ghcr.io/zebradil/kasha-box:edge gc`, and uploads the stdout list as the
`deleted-objects` artifact for 14 days.

Because it runs the published `edge` image rather than building from source, a change to `kasha gc` reaches the
schedule only after the image workflow has published it.

:::caution[Unverified]
Not run while writing this page: it needs a bucket and delete-capable credentials. The sweep is covered by the unit
tests in `src/gc.rs` (`remote_sweep_full_scenario` and its neighbours).
:::

All flags: [kasha gc](../../reference/cli/kasha-gc/).
