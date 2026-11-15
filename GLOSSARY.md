# kasha

A net-local Nix binary cache: a "box" that eagerly mirrors generations between the
LAN and a durable remote cache, so on-network builds get LAN-speed substitution
without paying the remote hop on every request. Extracted for reuse from
[`znix`](https://github.com/Zebradil/znix), a personal unified Nix configuration that
remains the reference consumer/deployment (its remote cache is
`znix.zebradil.dev`, backed by Cloudflare R2).

## Language

**Remote cache**:
The durable, always-reachable binary cache (`znix.zebradil.dev`, backed by Cloudflare
R2) that every environment can reach from anywhere. The cross-environment hub of last
resort — every generation eventually lands here.
_Avoid_: R2, the bucket, origin

**Upstream cache**:
A binary cache the box only reads from (e.g. `cache.nixos.org`). Unlike the remote cache it
carries no generation manifests and the box never writes to it.
_Avoid_: origin, mirror

**Box**:
A net-local binary cache instance reachable only from one physical network (e.g. home
LAN). Holds a flat binary-cache store (same layout as the remote bucket, no nix
installed), serves reads, and accepts authenticated HTTP pushes at LAN speed.
_Avoid_: local cache, edge cache, proxy

**Box image**:
The generic OCI packaging of a box, published for container runtimes but still backed
by a persistent box data root. It is configured by environment variables and is not a
replacement for deployment-specific manifests.
_Avoid_: Docker image, container (without box/image context)

**Eager bidirectional replica**:
The box's sync model: it continuously pulls new generations *down* from the remote
cache in the background (before anything asks for them) and mirrors locally-pushed
generations *up* to the remote cache. Neither direction waits on a client request.
_Avoid_: pull-through cache (that is the separate **Pull-through** mode), mirror (ambiguous
about direction)

**Pull-through**:
An optional box mode: a read for a path the box lacks is answered from the remote cache or
an upstream cache instead of as a miss, and the box keeps the verified path for later reads.
It works beside the eager bidirectional replica and never replaces it.
_Avoid_: proxy, lazy mirror, on-demand mirroring

**Unlisted path**:
A path on the box that no retained generation manifest lists: pulled through, pushed without a
manifest, or dropped by generation retention. With a size cap set, the box keeps unlisted paths
within it and evicts the least recently used first.
_Avoid_: orphan (a NAR no narinfo names), cached path

**Generation manifest (v3)**:
An object (`roots/<flake>/<gen>.json`) published by a writer (CI or a local push)
alongside a generation's NARs. Lists the generation's full build-closure store
paths plus explicit `branch` and `attr` fields. Readers discover new generations
by listing the `roots/` prefix; mirroring is a dumb fetch of the listed paths —
no closure expansion on the box.
_Avoid_: root manifest (v2, roots-only — dead), index

**kasha-cache-push**:
The producer-side script kasha ships (`packages.kasha-cache-push`,
`scripts/cache-push.sh`):
resolve a flake attr's build closure, `nix store sign` it, `nix copy` it to the remote
cache, then `kasha emit` the manifest. Lives here rather than in each producer repo so
the push and the manifest format it writes are pinned by one flake input.
_Avoid_: populate-nix-cache (the pre-move name in znix)

**Generation**:
One published set of build outputs for one flake, identified by a gen-id and
timestamp, described by exactly one root manifest.

**Release-tag generation**:
A generation whose branch is a release tag (`v1.2.3`, optionally `-suffix`). Remote
retention never treats its group as stale, so the newest one of every release stays
(ADR-0014).

**Selection**:
The client-side decision of which substituter to read from — box when reachable,
remote cache otherwise. MVP selection is a static substituter list plus a low
`connect-timeout`; a future **selection shim** replaces this with a local proxy that
probes reachability and eliminates the off-network timeout tax.
_Avoid_: routing (reserved for the future, richer client-side rule engine)

**Discovery backend**:
The pluggable mechanism a selection shim uses to locate the box's address: a
static-endpoint backend (works anywhere, including inside a k8s overlay network) or an
mDNS backend (zero-config, but only works when the shim shares a LAN segment with the
box — it cannot cross a Kubernetes CNI overlay).

**Image channel**:
A published tag stream for the box image. `edge` follows `main`; `sha-*` and `v*` tags
are immutable references for reproducible deployments.
_Avoid_: latest

**Reverse flow**:
A local build pushed up (box or remote cache) instead of the usual CI-builds /
user-pulls direction. Optional; exists to let CI skip work others already built.
Push is plain `nix copy --to http://box` with netrc/bearer auth; the matching
manifest goes through the same authed PUT API.

**Local-origin generation**:
A generation whose manifest arrived via the authenticated push API, tracked in
state files beside the store. Mirror-up copies everything the remote lacks from
local-origin generations; a local-origin generation is never GC-eligible until
its manifest is confirmed present in the remote cache.
