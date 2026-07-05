# kasha

Net-local Nix binary cache box.

kasha runs an always-on LAN cache that serves signed Nix store paths quickly,
accepts `ssh-ng` pushes, and mirrors root-manifested generations to and from a
durable S3-compatible remote cache.

## Components

- `nixosModules.box`: box service with harmonia, optional `ssh-ng` push, mirror-down, mirror-up, and NFS store guard.
- `nixosModules.consumer`: host-scoped static substituter selection: box first, remote cache second, low `connect-timeout`.
- `scripts/emit-root-manifest.sh`: emit and optionally publish `roots/<flake>/<gen>.json`.
- `scripts/push.sh`: auto-select box vs remote push target, with `--to` override.
- `scripts/mirror-down.sh` and `scripts/mirror-up.sh`: root-manifest-driven replica workers used by the box module.

The scripts are plain repo scripts today, not flake `apps`. From another repo,
call them from a checkout/submodule/vendor path, or from a Nix expression via the
flake input path. Add flake apps later if `nix run ...#push` matters.

## Integrate

### 1. Add kasha input

```nix
{
  inputs.kasha.url = "github:Zebradil/kasha";
}
```

### 2. Wire consumer selection into one host

Import `kasha.nixosModules.consumer` only in the host that should use the LAN
box. Do not add it globally if some machines, like a work host, must stay
remote-cache-only.

```nix
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

Apply it normally:

```sh
nixos-rebuild switch --flake .#your-host
```

### 3. Deploy the box

Use `kasha.nixosModules.box` on the always-on LAN machine/container. The Nix
store must be block storage, not NFS.

```nix
{
  imports = [ kasha.nixosModules.box ];

  services.kasha-box = {
    enable = true;
    storeDir = "/nix";
    port = 5000;
    trustedPublicKeys = [ "znix.zebradil.dev:AAAA..." ];

    push = {
      enable = true;
      authorizedKeys = [ "ssh-ed25519 AAAA... laptop" ];
    };

    mirrorDown = {
      enable = true;
      remoteCache = "s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto";
      flakes = [ "znix" ];
      interval = "5min";
    };

    mirrorUp = {
      enable = true;
      remoteCache = "s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto";
      flakes = [ "znix" ];
      interval = "5min";
      localRootsDir = "/var/lib/kasha/roots";
    };
  };
}
```

For k3s notes, stable LAN endpoints, and storage caveats, see
`deploy/README.md`.

### 4. Publish root manifests from CI

After CI builds, signs, and copies top-level outputs to the remote cache, publish
a root manifest for that generation. Feed only top-level output paths, one per
line; kasha lets `nix copy` expand closures later.

```sh
printf '%s\n' /nix/store/...-system /nix/store/...-home \
  | KASHA_FLAKE=znix \
    KASHA_GEN="$(date -u +%Y%m%d%H%M%S)" \
    KASHA_TARGET='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
    scripts/emit-root-manifest.sh
```

### 5. Use reverse-flow push

Use the same command on and off LAN. On LAN it pushes to the box; off LAN it
falls back to the remote cache. Use `--to` for deterministic throughput tests.

```sh
KASHA_BOX_CACHE=http://box.lan:5000 \
KASHA_BOX_TARGET=ssh-ng://kasha-push@box.lan \
KASHA_REMOTE_TARGET='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
scripts/push.sh /nix/store/...

scripts/push.sh --to ssh-ng://kasha-push@box.lan /nix/store/...
```

For locally pushed generations that must mirror up, also write a local root
manifest under `/var/lib/kasha/roots/<flake>/<gen>.json` on the box.

## Test

### Repo checks

Run all implemented checks:

```sh
nix flake check
```

This covers shell scripts, GitHub workflow linting, fixture tests, and Linux VM
smoke tests for serve, push, selection, mirror-down, and mirror-up.

Run one check while iterating:

```sh
nix build .#checks.x86_64-linux.fixtures
nix build .#checks.x86_64-linux.selection
nix build .#checks.x86_64-linux.push
nix build .#checks.x86_64-linux.mirror-down
nix build .#checks.x86_64-linux.mirror-up
nix build .#checks.x86_64-linux.smoke
```

### Integration smoke in your setup

1. Build and sign one small derivation on a client or in CI.
2. Copy it to the remote cache and publish a root manifest with `scripts/emit-root-manifest.sh`.
3. Start `kasha-mirror-down-<flake>.service` on the box and verify the path exists there with `nix-store --check-validity`.
4. Delete the path from the client and realise it again; it should substitute from `boxEndpoint`.
5. Block or leave the LAN, then realise a different remote-only signed path; it should fall back to `remoteCache` within `connectTimeout`.
6. Push a signed path with `scripts/push.sh`; verify it appears on the box immediately and is served over HTTP before mirror-up runs.
7. Add its root manifest under `/var/lib/kasha/roots/<flake>/`, start `kasha-mirror-up-<flake>.service`, and verify the path plus manifest reach the remote cache.

### Operational checks

```sh
systemctl status harmonia.socket sshd.service
systemctl status 'kasha-mirror-down-*' 'kasha-mirror-up-*'
journalctl -u 'kasha-mirror-down-*' -u 'kasha-mirror-up-*'
```

Confirm the box has no private signing key and only trusts the existing remote
cache public key. The box should hold remote cache read/write credentials, not a
binary-cache signing key.

## Out of scope

- Garbage collection.
- mDNS discovery and localhost selection shim.
- CI reading from the LAN box.
- Consumer-specific znix wiring beyond the examples above.
