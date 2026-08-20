# Deploying the box

The box is one container running the static `kasha` binary
(`ghcr.io/zebradil/kasha-box`). It serves a flat binary-cache store over HTTP,
ingests authenticated pushes, mirrors manifest-listed generations both ways, and
GCs itself. The box holds **no signing key** (ADR-0004) — it verifies and serves
upstream signatures as-is — and no delete-capable remote credentials (the remote
sweep runs in CI, `.github/workflows/gc.yml`).

```sh
docker run -d \
  -p 5000:5000 \
  -v kasha-data:/kasha \
  -e KASHA_TRUSTED_KEYS='znix.zebradil.dev:AAAA…' \
  -e KASHA_TOKEN='…' \
  -e KASHA_REMOTE='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
  -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… \
  ghcr.io/zebradil/kasha-box:edge
```

See the README's environment-contract table for every knob; all are also flags
(`kasha serve --help`). Writes are refused when `KASHA_TOKEN` is unset;
mirroring and box GC are off when `KASHA_REMOTE` is unset.

Reverse-flow pushes are plain `nix copy --to http://box:5000` with netrc auth
(any login, password = the token). Pushed narinfos must carry a signature from a
`KASHA_TRUSTED_KEYS` key — the box verifies every ingested narinfo.

The push/serve/substitute paths are validated end-to-end by the NixOS-VM check
`integration` (`tests/v2.nix`): signed `nix copy` push with netrc auth, manifest
emit through the authed API, substitute pull gated by the trusted public key.

## k3s

The box runs as an always-on workload with a **stable LAN endpoint**
(`Service type: LoadBalancer` or `NodePort`) and its data on a PVC mounted at
`/kasha`. The store is flat files with no database — no NFS locking hazard, any
storage class works; block storage is still preferable for throughput.
