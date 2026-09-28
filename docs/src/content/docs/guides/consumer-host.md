---
title: Point a host at the box
description: Import the consumer NixOS module into one host so it reads from the box on the LAN and the remote cache elsewhere.
---

The consumer module sets a host's substituters to the box first and the remote cache second, with a low connect timeout.
On the LAN the box answers; off it, the connection to the box times out and Nix falls back to the remote cache, with no
change to the configuration.

## Prerequisites

- A NixOS host configured from a flake.
- A running box with a stable address (see [Deploy a box](../deploy-box/)).
- The public key(s) your remote cache's paths are signed with.

## 1. Add the flake input

```nix
{
  inputs.kasha.url = "github:Zebradil/kasha";
}
```

## 2. Import the module into one host

Import `kasha.nixosModules.consumer` in the modules of the host that should use the box, not in a module every host
shares: the box endpoint only makes sense for machines on its network.

```nix
nixosConfigurations.workstation = nixpkgs.lib.nixosSystem {
  modules = [
    kasha.nixosModules.consumer
    ./hosts/workstation.nix
  ];
};
```

## 3. Configure it

In that host's configuration:

```nix
{
  services.kasha-consumer = {
    enable = true;
    boxEndpoint = "http://box.lan:5000";
    remoteCache = "https://znix.zebradil.dev";
    connectTimeout = 2;
    trustedPublicKeys = [ "znix.zebradil.dev:AAAA..." ];
  };
}
```

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | turn the selection on |
| `boxEndpoint` | none, required | the box's LAN HTTP endpoint, queried first |
| `remoteCache` | `https://znix.zebradil.dev` | the off-LAN fallback |
| `connectTimeout` | `2` | seconds Nix waits to connect to each substituter; bounds the off-LAN delay |
| `trustedPublicKeys` | `[ ]` | keys accepted on substituted paths |

The module then sets:

- `nix.settings.substituters` to exactly `[ boxEndpoint remoteCache ]`, with `mkForce`, so no other substituter
  (including the default `https://cache.nixos.org`) is queried ahead of the box. Add further substituters with
  `extra-substituters` if the host needs them.
- `nix.settings.connect-timeout` to `connectTimeout`.
- `nix.settings.trusted-public-keys` to `trustedPublicKeys`, appended to Nix's default `cache.nixos.org-1` key.

An empty `trustedPublicKeys` fails evaluation with
`services.kasha-consumer.trustedPublicKeys must list the remote-cache public key(s)`: without the key, paths
substituted from the remote cache would fail signature verification.

:::caution[Unverified]
Checked by evaluating a minimal `x86_64-linux` NixOS configuration with the module (resulting settings and the
assertion), not by switching a real host.
:::

## 4. Rebuild

```sh
sudo nixos-rebuild switch --flake .#workstation
```

The box answers `404` for paths it does not hold and never fetches them on demand, so Nix moves on to the remote cache
for those. See [Substituter selection](../../concepts/selection/) for why the fallback is a timeout rather than
discovery.
