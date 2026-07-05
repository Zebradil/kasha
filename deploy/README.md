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
  };
}
```

The push user is a normal (non-root) account, so it is **not** a nix
trusted-user: an untrusted push must present paths signed by a
`trustedPublicKeys` key, which is exactly the `require-sigs` gate.

These paths are validated end-to-end by NixOS-VM checks: `smoke`
(`tests/smoke.nix`) covers seed → serve → substitute → verify; `push`
(`tests/push.nix`) covers signed push → serve-immediately, plus rejection of an
unsigned push and the box holding no signing key; `mirror-down`
(`tests/mirror-down.nix`) covers remote root manifest → box closure and
idempotent re-run.

## k3s

The box runs as an always-on workload with a **stable LAN endpoint**
(`Service type: LoadBalancer` or `NodePort`) and its nix store on a **block**
PVC (iSCSI storage class) — **never** an NFS-backed volume; Nix's sqlite store
DB corrupts under NFS locking (ADR-0002), and the module's startup guard rejects
it at runtime as a backstop.

Deployment options for the reference consumer (znix):

- **NixOS-as-container** (recommended): build a NixOS system with the module and
  run it (e.g. via `nixos-generators` / a systemd-nspawn or microvm image). The
  store PVC mounts at `/nix`.
- **OCI image**: note the mount gotcha — a `dockerTools` image keeps its own
  binaries under `/nix/store`, so mounting the store PVC at `/nix` shadows them.
  Mount the data store at a separate path and point harmonia at it, or ship
  harmonia outside `/nix`. This wiring lives with the deploy slice, not here.
