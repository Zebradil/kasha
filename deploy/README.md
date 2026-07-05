# Deploying the box

The box is the NixOS module `nixosModules.box` (`services.kasha-box`). It runs
`harmonia`, serving the local nix store as a signed binary cache over HTTP, and
refuses to start if that store is NFS-backed (ADR-0002). The box holds **no
signing key** (ADR-0004) — it serves upstream signatures as-is.

```nix
{
  imports = [ kasha.nixosModules.box ];
  services.kasha-box = {
    enable = true;
    storeDir = "/nix";   # must be block storage (iSCSI PV), never NFS
    port = 5000;

    # Trust the existing remote-cache key so pushed paths verify (require-sigs
    # stays on — the box holds no private key, ADR-0004).
    trustedPublicKeys = [ "znix.zebradil.dev:AAAA…" ];

    # Reverse flow: LAN-speed ssh-ng push target. Authorized clients run
    # `nix copy --to ssh-ng://kasha-push@box <path>`; the box serves the pushed
    # path over HTTP immediately, no up-mirror in the loop.
    push = {
      enable = true;
      authorizedKeys = [ "ssh-ed25519 AAAA… laptop" ];
    };

    # Eager down replica: periodically list roots/<flake>/ in the remote cache
    # and `nix copy --from` each new root, which pulls the full closure.
    mirrorDown = {
      enable = true;
      remoteCache = "s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto";
      flakes = [ "znix" ];
      interval = "5min";
    };

    # Eager up replica: periodically read box-local roots/<flake>/<gen>.json
    # manifests, `nix copy --to` each root to the remote cache, and publish the
    # same manifest upstream after the copy succeeds.
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

The push user is a normal (non-root) account, so it is **not** a nix
trusted-user: an untrusted push must present paths signed by a
`trustedPublicKeys` key, which is exactly the `require-sigs` gate.

Client-side reverse-flow pushes use `scripts/push.sh`: it probes the box HTTP
cache with a `.narinfo` HEAD, pushes to `KASHA_BOX_TARGET` when reachable, and
falls back to `KASHA_REMOTE_TARGET` when not. `--to <target>` skips probing and
forces a target.

```sh
KASHA_BOX_CACHE=http://box:5000 \
KASHA_BOX_TARGET=ssh-ng://kasha-push@box \
KASHA_REMOTE_TARGET='s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto' \
scripts/push.sh /nix/store/...

scripts/push.sh --to ssh-ng://kasha-push@box /nix/store/...
```

These paths are validated end-to-end by NixOS-VM checks: `smoke`
(`tests/smoke.nix`) covers seed → serve → substitute → verify; `push`
(`tests/push.nix`) covers signed push → serve-immediately, plus rejection of an
unsigned push and the box holding no signing key; `mirror-down`
(`tests/mirror-down.nix`) covers remote root manifest → box closure;
`mirror-up` (`tests/mirror-up.nix`) covers box-local root manifest → remote
closure + manifest. Both mirror checks cover failed-copy safety and idempotent
re-run.

## k3s

The box runs as an always-on workload with a **stable LAN endpoint**
(`Service type: LoadBalancer` or `NodePort`) and its nix store on a **block**
PVC (iSCSI storage class) — **never** an NFS-backed volume; Nix's sqlite store
DB corrupts under NFS locking (ADR-0002), and the module's startup guard rejects
it at runtime as a backstop.

Deployment options for the reference consumer (znix):

- **NixOS-as-container**: build a NixOS system with the module and run it (e.g.
  via `nixos-generators` / a systemd-nspawn or microvm image). The store PVC
  mounts at `/nix`.
- **OCI image**: run `ghcr.io/zebradil/kasha-box`. Mount the box data PVC at
  `/kasha`, not `/nix`; the image keeps its own binaries under `/nix/store` and
  uses `/kasha/nix/store` as the physical box store.

```sh
docker run --rm \
  -p 5000:5000 \
  -v kasha-data:/kasha \
  ghcr.io/zebradil/kasha-box:edge
```

OCI environment:

- `KASHA_PORT` defaults to `5000`.
- `KASHA_DATA_ROOT` defaults to `/kasha`.
- `KASHA_TRUSTED_PUBLIC_KEYS` is required for push and mirror modes.
- `KASHA_PUSH_ENABLE=1` starts `nix daemon` and `sshd`; set
  `KASHA_PUSH_AUTHORIZED_KEYS` and optionally `KASHA_PUSH_USER`.
- `KASHA_MIRROR_DOWN_ENABLE=1` or `KASHA_MIRROR_UP_ENABLE=1` starts mirror loops;
  set `KASHA_REMOTE`, `KASHA_FLAKES`, and optional interval seconds.
