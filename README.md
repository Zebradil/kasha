# kasha

Net-local Nix binary cache box.

kasha is a single static binary that runs an always-on LAN cache: it serves
signed Nix store paths at LAN speed, accepts authenticated `nix copy` pushes
over HTTP, and mirrors manifest-listed generations to and from a durable
S3-compatible remote cache.

## Components

- `kasha serve`: the box — binary-cache HTTP server, mirror-down/up workers, GC timer.
- `kasha emit`: build and publish a v3 generation manifest (closure paths on stdin).
- `kasha gc`: sweep the remote cache; run from CI with delete-capable credentials.
- `nixosModules.consumer`: host-scoped static substituter selection — box first, remote cache second, low `connect-timeout`.
- `packages.<system>.oci-image`: box OCI image (static binary only), published as `ghcr.io/zebradil/kasha-box`.

The box holds a flat binary-cache layout on disk (same layout as the remote
bucket): `<hash>.narinfo`, `nar/…`, `roots/…`. No nix on the box, no database —
the index is rebuilt by scanning narinfos at boot. Every ingested narinfo
(push or mirror) must be signed by a trusted key; the box itself holds no
signing key.

## Run the box

```sh
docker run -d -p 5000:5000 -v kasha-data:/kasha \
  -e KASHA_TRUSTED_KEYS='znix.zebradil.dev:AAAA...' \
  -e KASHA_TOKEN='...' \
  -e KASHA_REMOTE='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
  -e AWS_ACCESS_KEY_ID=... -e AWS_SECRET_ACCESS_KEY=... \
  ghcr.io/zebradil/kasha-box:edge
```

Environment contract (all also available as flags, see `kasha serve --help`):

| Variable | Meaning | Default |
| --- | --- | --- |
| `KASHA_DATA` | store root (flat binary-cache layout) | `/kasha` |
| `KASHA_LISTEN` | listen address | `0.0.0.0:5000` |
| `KASHA_TRUSTED_KEYS` | trusted narinfo signing keys (required) | — |
| `KASHA_TOKEN` | write token; writes refused when unset | — |
| `KASHA_REMOTE` | remote cache `s3://bucket?endpoint=…&region=…`; mirroring and GC off when unset | — |
| `KASHA_UPSTREAMS` | upstream substituters tried after the remote | `https://cache.nixos.org` |
| `KASHA_SYNC_INTERVAL` / `KASHA_GC_INTERVAL` | worker intervals, seconds | `300` / `86400` |

The box needs read/write (never delete) remote credentials via `AWS_ACCESS_KEY_ID`
and `AWS_SECRET_ACCESS_KEY`.

## Wire consumer selection into one host

Import `kasha.nixosModules.consumer` only in hosts that should use the LAN box.

```nix
{
  inputs.kasha.url = "github:Zebradil/kasha";
}
{
  imports = [ kasha.nixosModules.consumer ];

  services.kasha-consumer = {
    enable = true;
    boxEndpoint = "http://box.lan:5000";
    remoteCache = "https://znix.zebradil.dev";
    connectTimeout = 2;
    trustedPublicKeys = [ "znix.zebradil.dev:AAAA..." ];
  };
}
```

Reads fall back to the remote cache within `connectTimeout` when the box is
unreachable — the box serves 404 on miss and never proxies.

## Publish generations from CI

After CI builds, signs, and copies a generation to the remote cache, publish a
v3 manifest listing the full build closure (one store path per line on stdin):

```sh
nix-store --query --requisites ./result \
  | kasha emit --flake znix --gen "main-$(date -u +%Y%m%d%H%M%S)-host" \
      --branch main --attr host \
      --to 's3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto'
```

The box discovers new manifests under `roots/<flake>/`, fetches listed paths
from the remote cache first and upstreams for the rest, and records misses as
gaps (re-probed each cycle, reported in `/status` — never a failing unit).

## Push from a client

Sign locally, then plain `nix copy` with netrc auth (any login, password =
`KASHA_TOKEN`):

```sh
echo 'machine box.lan login nix password <token>' > ~/.config/nix/netrc
nix store sign --key-file ./secret-key --recursive ./result
nix copy --to 'http://box.lan:5000' ./result
```

Publish a manifest for it through the same authed API so mirror-up picks it up:

```sh
nix-store --query --requisites ./result \
  | KASHA_TOKEN=<token> kasha emit --flake znix --gen "$gen" \
      --branch main --attr host --to http://box.lan:5000
```

Generations pushed this way are tracked as local-origin: mirror-up copies
everything the remote lacks, and the box never GCs them until their manifest is
confirmed present in the remote cache.

## GC

- **Box sweep** runs in-process on a timer: retain a generation if it is younger
  than `M` or among the `N` newest in its `(branch, attr)` group (defaults: main
  `N=5, M=4wk`, other `N=1, M=1wk`); a 24h grace window skips young objects.
- **Remote sweep** runs from CI (`.github/workflows/gc.yml`, `kasha gc`) with
  delete-capable credentials the box never holds. `--dry-run` reports only.

## Observability

Structured logs on stderr (JSON when not a terminal) and one `/status` JSON
endpoint: object count, store size, per-flake last sync and gap count, pending
mirror-up.

## Test

```sh
cargo test          # unit + HTTP round-trip tests
nix flake check     # + actionlint and the NixOS VM integration test (Linux)
nix build .#checks.x86_64-linux.integration
```

The integration test drives a real nix client: signed `nix copy` push with
netrc auth, manifest emit, then a substitute pull on another node gated by the
trusted public key.

## Out of scope (deliberate, see docs/agents/handoff-rewrite.md)

- Pull-through serving on miss.
- Re-compression (objects stored byte-identical as received).
- Mirror-up filtering (push everything the remote lacks).
- Prometheus /metrics.
- mDNS discovery and localhost selection shim.
