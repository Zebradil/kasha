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
  };
}
```

The serving logic is validated end-to-end by the `smoke` check
(`tests/smoke.nix`): seed a signed path → client substitutes it from the box →
signature verifies under the existing public key.

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
