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

**Box**:
A net-local binary cache instance reachable only from one physical network (e.g. home
LAN). Holds a full nix store, serves reads, and accepts pushes at LAN speed.
_Avoid_: local cache, edge cache, proxy

**Eager bidirectional replica**:
The box's sync model: it continuously pulls new generations *down* from the remote
cache in the background (before anything asks for them) and mirrors locally-pushed
generations *up* to the remote cache. Neither direction waits on a client request.
_Avoid_: pull-through cache, mirror (ambiguous about direction)

**Root manifest**:
A small object (`roots/<flake>/<gen>.json`) published by a writer (CI or a local
push) alongside a generation's NARs. Lists that generation's top-level output paths
only — not their closures. Readers discover new generations by listing the `roots/`
prefix; syncers expand a root into its full closure via `nix copy`'s own
closure-awareness.
_Avoid_: index, manifest (without "root")

**Generation**:
One published set of build outputs for one flake, identified by a gen-id and
timestamp, described by exactly one root manifest.

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

**Reverse flow**:
A local build pushed up (box or remote cache) instead of the usual CI-builds /
user-pulls direction. Optional; exists to let CI skip work others already built.
The push entrypoint probes the box's HTTP cache with a `.narinfo` HEAD and targets
the box's `ssh-ng://` endpoint when reachable, otherwise the remote cache; `--to`
forces a target for manual runs and performance tests.

Box-local root manifests live under `/var/lib/kasha/roots/<flake>/<gen>.json` and
are the up-mirror's source of truth for locally-pushed generations.
