---
title: Substituter selection
description: How a host chooses between the box and the remote cache, and why it is a static list rather than discovery.
---

**Selection** is the client-side choice of which cache to read from: the box when it is reachable, the remote cache
otherwise. kasha makes that choice with plain Nix settings, no extra client software.

## A static list with a short timeout

The consumer module sets a host's substituters to exactly `[box, remote cache]` and lowers `connect-timeout` to 2
seconds by default:

- **On the LAN** the box answers first, at LAN speed. For a path it lacks it answers `404`, and Nix asks the remote cache.
- **Off the LAN** the connection to the box fails within `connect-timeout`, and Nix falls back to the remote cache.

Nothing changes between the two situations: no reconfiguration, no service to restart. The cost is a small timeout tax
off-network.

The module is imported per host, not flake-wide. A box endpoint only makes sense for machines on the box's network, so
other hosts in the same flake are untouched.

## Why not discovery

A local **selection shim**, a small proxy on each host that probes whether the box is reachable, would remove the
off-network timeout tax. It was deferred until that tax is actually felt: it is new client-side surface (a service plus a
discovery mechanism), while the static list delivers the on-network speedup today.

Discovery also meets an environment constraint. The reference box runs as a container in a k3s cluster, and mDNS
(link-local multicast) cannot cross the cluster's network overlay to reach the host LAN. A future shim would support
pluggable **discovery backends**: a static endpoint, which works anywhere, and mDNS, which needs the shim on the same LAN
segment as the box.

Both the shim and mDNS discovery are out of scope for now.

Set it up: [Point a host at the box](../../guides/consumer-host/). Rationale:
[ADR-0005](https://github.com/Zebradil/kasha/blob/main/docs/adr/0005-static-selection-deferred-shim.md).
