---
title: Getting started
description: Run a kasha box on your machine, push a signed store path to it, and substitute it back.
---

In this tutorial you run a box locally, push a signed build of `hello` to it with plain `nix copy`, and pull it back into
a fresh store, gated by your signing key. It takes about five minutes and needs no S3 bucket.

You need Nix with the `nix-command` and `flakes` experimental features enabled, and `curl`. Everything happens in a
scratch directory; your own Nix store is only read.

```sh
mkdir kasha-tutorial && cd kasha-tutorial
```

## 1. Make a signing key

The box never signs anything: it only accepts paths signed by a key it trusts. Generate a throwaway key pair:

```sh
nix key generate-secret --key-name demo-1 > secret-key
nix key convert-secret-to-public < secret-key > public-key
cat public-key
```

```text
demo-1:3SEmxzPlTj0luByKrJ9hB5lOmfpOXRvGk1pPNm2/XV0=
```

Your key differs; `demo-1:` is the name you chose.

## 2. Start the box

In a second terminal, in the same directory, start the box. It trusts your public key and accepts writes authenticated
with the token `demo-token`:

```sh
KASHA_TOKEN=demo-token nix run github:Zebradil/kasha -- serve \
  --data ./box --listen 127.0.0.1:5000 --trusted-keys "$(cat public-key)"
```

The box warns `KASHA_REMOTE unset: mirroring and GC disabled`, since no remote cache is configured, then logs
`kasha serving` with its listen address.

Back in the first terminal, ask the box for its status:

```sh
curl -s http://127.0.0.1:5000/status
```

```json
{"flakes":{},"objects":0,"pending_mirror_up":0,"store_bytes":0}
```

## 3. Push a signed path

Build `hello` and copy its closure into a scratch store, `./client`, so signing it leaves your own store untouched:

```sh
p=$(nix build nixpkgs#hello --no-link --print-out-paths)
nix copy --to ./client "$p"
```

Sign the whole closure with your key:

```sh
nix store sign --store ./client --key-file secret-key --recursive "$p"
```

Tell Nix the write token through a netrc file (any login; the password is the token), then push:

```sh
echo 'machine 127.0.0.1 login nix password demo-token' > netrc
nix copy --store ./client --option netrc-file "$PWD/netrc" --to http://127.0.0.1:5000 "$p"
```

The box now holds `hello`, its dependencies, and their NARs:

```sh
curl -s http://127.0.0.1:5000/status
```

```json
{"flakes":{},"objects":3,"pending_mirror_up":0,"store_bytes":854474}
```

Counts and sizes depend on your platform's `hello` closure.

## 4. Substitute from the box

Pull the path into a second scratch store, trusting only your key:

```sh
nix copy --from http://127.0.0.1:5000 --to ./other \
  --option trusted-public-keys "$(cat public-key)" "$p"
./other"$p"/bin/hello
```

```text
Hello, world!
```

Try the same pull trusting a different key, and Nix refuses it:

```sh
nix copy --from http://127.0.0.1:5000 --to ./other2 \
  --option trusted-public-keys 'wrong-1:3SEmxzPlTj0luByKrJ9hB5lOmfpOXRvGk1pPNm2/XV0=' "$p"
```

```text
error: cannot add path '/nix/store/…' because it lacks a signature by a trusted key
```

:::caution[Unverified]
This page was run with a local build of the kasha binary and an existing `hello` output, not with
`nix run github:Zebradil/kasha` and `nix build nixpkgs#hello`; those two commands were not run while writing it.
:::

Stop the box with Ctrl-C and delete `kasha-tutorial` when you are done.

## Next steps

- [Deploy a box](../guides/deploy-box/) for your network, with a remote cache to mirror.
- [Point a host at the box](../guides/consumer-host/) so it substitutes from it automatically.
- [How the box works](../concepts/eager-replica/): what the box stores and how it syncs.
