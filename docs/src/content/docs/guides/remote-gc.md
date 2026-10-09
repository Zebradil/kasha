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

## 1. Preview retention

`kasha ls` takes the same `--remote` and retention flags and prints one row per manifest under `roots/`, grouped by
flake, branch and attr, newest first. It reads only each manifest's first kilobyte, so it needs read-only credentials
and finishes in seconds where a sweep takes many minutes.

```text
VERDICT     AGE  FLAKE  BRANCH  ATTR                      KEY
keep:age    0d   znix   main    checks.x86_64-linux.lint  roots/znix/main-1a2b3c4-checks.x86_64-linux.lint.json
keep:count  40d  znix   pr      checks.x86_64-linux.lint  roots/znix/pr-5d6e7f8-checks.x86_64-linux.lint.json
drop:stale  38d  znix   main    update                    roots/znix/main-9a8b7c6-update.json
```

| Verdict | Meaning |
| --- | --- |
| `keep:age` | younger than `M` |
| `keep:count` | among the `N` newest of its group |
| `drop:stale` | among the `N` newest, but its group is stale |
| `drop` | neither |
| `garbage` | not a valid v3 manifest |

`ls` trusts the header: a manifest whose closure list is invalid shows a verdict, but the sweep deletes it as garbage.
The 24-hour grace window is not applied either; a `drop` row whose object is younger than that survives the next sweep.

## 2. Dry run

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

## 3. Adjust retention if needed

A generation is kept when it is among the `N` newest in its `(flake, branch, attr)` group, or younger than `M` weeks.
The branch `main` has its own tier:

| Tier | Flags | Default |
| --- | --- | --- |
| `main` | `--main-keep`, `--main-age-weeks` | `N=5`, `M=4` |
| every other branch | `--other-keep`, `--other-age-weeks` | `N=1`, `M=1` |

`--stale-weeks` (default `4`, `0` disables) retires a group whose newest generation trails the newest of its flake's
tier by that long; the `N` rule stops applying to it. See [Retention and GC](../../concepts/retention/).

`--grace-hours` sets the grace window. The box's own sweep keeps a subset of this (see
[Retention and GC](../../concepts/retention/)), so shrinking the remote policy also shrinks what boxes keep once they
next sync.

## 4. Run it for real

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
