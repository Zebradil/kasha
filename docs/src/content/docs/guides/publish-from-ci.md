---
title: Publish generations from CI
description: Sign and push a build closure to the remote cache, then publish the manifest that makes boxes mirror it.
---

A box mirrors only what a generation manifest lists. Pushing NARs to the remote cache is not enough: until
`roots/<flake>/<gen>.json` exists, no box sees the push. This guide covers the three ways to publish, from most to least
automated:

- `kasha-cache-push` resolves, signs, pushes and emits in one command.
- The `emit-manifest` GitHub Action emits for a closure you already pushed.
- `kasha emit` by hand.

## Prerequisites

- A signing key file whose public half your boxes and hosts trust.
- An S3-compatible remote cache and credentials that can write to it.
- Nix with flakes enabled on the runner.

## Option A: kasha-cache-push

`packages.<system>.kasha-cache-push` wraps the whole sequence: resolve each flake attr's build closure, `nix store sign`
it, `nix copy` it to S3, then emit one manifest per attr. Pinning it through the same flake input as `kasha emit` keeps
the push and the manifest format in step.

```sh
CACHE_S3_URL='s3://znix-cache?endpoint=…&region=auto' \
CACHE_SIGNING_KEY_FILE=./secret-key \
KASHA_FLAKE=znix KASHA_BIN="$(nix build --no-link --print-out-paths github:Zebradil/kasha#kasha-bin)/bin/kasha" \
  nix run 'github:Zebradil/kasha#kasha-cache-push' -- checks.x86_64-linux.host
```

`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` must be in the environment too. Each step is skipped when its inputs
are empty, so a run without a key or credentials does nothing harmful:

| Variable | Used for | When empty |
| --- | --- | --- |
| `CACHE_S3_URL` | `nix copy --to` target and manifest target; must be `s3://`, an `http(s)://` URL is an error | push and emit skipped |
| `CACHE_STORE_PARAMS` | store settings appended to the push URL | defaults to `compression=zstd&compression-level=6` |
| `CACHE_SIGNING_KEY_FILE` | `nix store sign --recursive` | signing skipped |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | S3 credentials | push and emit skipped |
| `KASHA_FLAKE` | flake id the manifests are filed under | emit skipped |
| `KASHA_BIN` | path to the `kasha` binary | emit skipped |
| `KASHA_GEN`, `KASHA_BRANCH`, `KASHA_ATTR` | manifest fields in `--paths-file` mode | emit skipped in that mode |

`CACHE_STORE_PARAMS` is not applied when `CACHE_S3_URL` already names a `compression`. Set it to an empty string to push
with Nix's own default.

The script takes its store paths from one of three sources:

| Arguments | Paths | Manifests |
| --- | --- | --- |
| `ATTR...` | each attr's build closure (outputs and `.drv` recipes) | one per attr, gen id `<branch>-<short sha>[-dirty]-<attr>` |
| none | every output of `.#checks.<current system>` | one per check |
| `--paths-file FILE` | the pre-resolved paths in `FILE` | one, from `KASHA_GEN`/`KASHA_BRANCH`/`KASHA_ATTR` |

In attr mode the branch comes from git. A dirty worktree gets `-dirty` appended to both the gen id and the branch, so a
local push never lands in the `main` retention tier.

:::caution[Unverified]
The command above needs S3 credentials and a signing key and was not run while writing this page. The script is
shellchecked at build time (`checks.kasha-cache-push`).
:::

## Option B: the emit-manifest action

When your workflow already signs and pushes (for example with `nix-fast-build --copy-to`), use the action to publish
the manifest only. It builds `kasha` from the ref you pin, substituting it from kasha's own cache when that ref has a
published build:

```yaml
- uses: zebradil/kasha/.github/actions/emit-manifest@<ref>
  with:
    flake: znix
    paths-file: paths.txt
    attr: checks.x86_64-linux.host
    s3-url: ${{ secrets.CACHE_S3_URL }}
    aws-access-key-id: ${{ secrets.AWS_ACCESS_KEY_ID }}
    aws-secret-access-key: ${{ secrets.AWS_SECRET_ACCESS_KEY }}
```

`branch` defaults to `pr` on pull request events and to `GITHUB_REF_NAME` otherwise, and `gen` to
`<branch>-<7-char sha>-<attr>`, both sanitized to `A-Za-z0-9_.-`. All pull requests share the `pr` branch because
retention keeps a group's newest generation until the group goes stale: one group per pull request would pin a closure
per pull request for weeks after it closed. The action outputs the `gen`, `branch` and `manifest-key` it used.

## Option C: kasha emit

`kasha emit` reads closure paths on stdin, one per line, and prints the manifest. With `--to` it also publishes it:

```sh
nix-store --query --requisites ./result \
  | kasha emit --flake znix --gen "main-$(date -u +%Y%m%d%H%M%S)-host" \
      --branch main --attr host \
      --to 's3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto'
```

Blank lines are dropped and the list is sorted and deduplicated:

```json
{"version":3,"flake":"demo","gen":"main-1-host","branch":"main","attr":"host","timestamp":"2026-09-28T00:00:00Z","closure":["/nix/store/0iavzb323r97k7sps762r5kj1f2bny0b-libiconv-115.100.1","/nix/store/85py0qgpd9llilbkgjpcwc4svrx056ld-hello-2.12.3"]}
```

- `--timestamp` takes an RFC 3339 UTC time and defaults to now. Retention ages generations by it.
- `--branch main` puts the generation in the long retention tier; any other branch gets the short one.
- `--to s3://…` uses the `AWS_*` credentials; `--to http(s)://box` pushes to a box with `KASHA_TOKEN`
  (see [Push from a client](../push-from-client/)).
- An empty closure, a path outside `/nix/store`, or an invalid timestamp is an error, and nothing is published.

Publish the manifest after the push has finished: mirror-down fetches exactly the listed paths, and any it cannot find
are counted as gaps until they appear. Full flag list: [kasha emit](../../reference/cli/kasha-emit/).
