---
title: Push from a client
description: Push a local build to the box with plain nix copy and publish its manifest so the box mirrors it up.
---

A local build can go to the box instead of waiting for CI: push the signed closure with `nix copy`, then publish a
manifest through the same authenticated API. The box mirrors the generation up to the remote cache in the background.

## Prerequisites

- The box's write token (`KASHA_TOKEN` on the box).
- A signing key whose public half is in the box's `KASHA_TRUSTED_KEYS`. The box never signs: it rejects any narinfo
  that has neither a trusted signature nor a `CA` field reproducing its store path.
- The `kasha` binary, for `kasha emit`.

## 1. Give Nix the token

The box accepts `Authorization: Bearer <token>` or HTTP Basic auth with any login and the token as password. `nix copy`
sends Basic auth from a netrc file:

```sh
echo 'machine box.lan login nix password <token>' > ~/.config/nix/netrc
```

Point Nix at the file with `netrc-file` in `nix.conf`, or pass `--option netrc-file` per command. A write without a
valid token gets `401`.

## 2. Sign and push the closure

```sh
nix store sign --key-file ./secret-key --recursive ./result
nix copy --to 'http://box.lan:5000' ./result
```

`--recursive` signs the whole closure. A path signed only by keys the box does not trust is refused with
`neither a trusted signature nor a valid content address on /nix/store/…`.

## 3. Publish the manifest

Without a manifest the box holds the paths but never mirrors them up. Emit one to the box, listing the closure you
pushed:

```sh
nix-store --query --requisites ./result \
  | KASHA_TOKEN=<token> kasha emit --flake znix --gen "$gen" \
      --branch main --attr host --to http://box.lan:5000
```

The box stores the manifest at `roots/<flake>/<gen>.json` only when its `flake` and `gen` fields match that path, and
marks the generation **local-origin**.

## 4. Watch it mirror up

On each sync cycle the box copies every path of a local-origin generation that the remote cache lacks, NAR before
narinfo, then uploads the manifest last. Until that succeeds the generation counts in `pending_mirror_up`:

```sh
curl -s http://box.lan:5000/status
```

Mirror-up waits while any listed path is missing on the box (logged as `mirror-up deferred`), so push the closure
before, or together with, the manifest.

A local-origin generation is never garbage-collected by the box until mirror-up has uploaded its manifest to the remote cache.
After that it follows the remote's retention like any other generation.

:::caution[Unverified]
Steps 1 to 3 were run against a local box without a remote cache. Mirror-up (step 4) needs a remote and was not run;
it is covered by the unit tests in `src/mirror.rs` and `src/gc.rs`.
:::

`--branch main` puts the generation in the long retention tier. For work in progress, use your branch name so it does
not displace reviewed `main` generations; see [Retention and GC](../../concepts/retention/).
