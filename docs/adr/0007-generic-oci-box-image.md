# Generic OCI box image is non-systemd and environment-configured

The box already has a NixOS module, but the OCI artifact is a generic runtime image
instead of a NixOS/systemd container: it keeps the deployment surface familiar for k3s,
uses environment variables as the runtime contract, and mounts a persistent data root at
`/kasha` so the image's own `/nix/store` is never shadowed. CI publishes
`ghcr.io/zebradil/kasha-box` from `main` as `edge` plus `sha-*`, and from `v*` tags as
immutable release tags plus `sha-*`; it smokes the HTTP cache before publishing and
attests the published image with GitHub provenance.

Considered: running the existing NixOS module under systemd inside OCI (less duplicate
wiring, rejected because the container should stay a simple process tree), and a
per-deployment image with baked Nix options (more Nix-native, rejected because the
requested artifact is a generic published image).
