# todo

What is deliberately not built yet, and what would justify building it. Replaces the old
`docs/cache-roadmap.md` and the "deliberately deferred" list in
`docs/agents/handoff-rewrite.md`. Terminology: `CONTEXT.md`. Decisions already made:
`docs/adr/`.

Nothing here is committed work — each item states the evidence that should trigger it, so
none of it gets built on speculation.

## Open bugs / operational gaps

### Remote GC has never run successfully

`.github/workflows/gc.yml` fails on every scheduled run: `Error: remote must be s3://…, got`.
The repo has no `KASHA_GC_ACCESS_KEY_ID` / `KASHA_GC_SECRET_ACCESS_KEY` secrets and no
`KASHA_REMOTE` variable. Until those are set, the remote cache is never swept and R2 storage
grows without bound.

Needs: R2 credentials with delete permission (separate from the box's read/write pair) as repo
secrets, plus `KASHA_REMOTE` as a repo variable. Verify with a `workflow_dispatch` run at
`dry-run: true` before letting the schedule delete anything.

### Dangling-narinfo warning (unresolved, cheap first step unrun)

A `nix build` against the box warned `file 'nar/….nar' does not exist in binary cache` for six
paths while the build still succeeded via fallback. Evidence collected at the time points at a
**stale client-side narinfo cache** (`~/.cache/nix/binary-cache-detsys-v2.sqlite`,
`narinfo-cache-positive-ttl` defaults to 30 days) rather than a box bug: all six narinfos 404'd
on the box within the hour, and no box sweep had run in that pod's lifetime.

First step, not yet done: delete that sqlite on the affected Mac, rebuild, see if the warning
recurs. Gone means client-side and there is nothing to fix in kasha. Recurs means the box really
serves a dangling narinfo — then audit the store over the PVC for narinfos whose `URL:` target is
absent, and look at `box_sweep` NAR liveness (`src/gc.rs`), compression-variant index rewrites in
`src/mirror.rs`, and partial `put_nar` writes, in that order.

Regardless of the verdict: `/nix-cache-info` could advertise a low `narinfo-cache-positive-ttl`
so clients re-check a LAN box instead of trusting a month-old narinfo. Check that nix honours it
from the cache side before building it.

### Box GC cannot be triggered on demand

Scheduling now survives restarts (`state/last-sweep`), but there is still no way to ask for a
sweep — testing one means shortening `KASHA_GC_INTERVAL` and restarting the pod. An
authenticated `POST /gc` (or a `--sweep-now` flag) would make it a one-liner. Worth doing the
next time GC is touched; not urgent while the timer works.

## Client selection

### Selection shim

The largest open item. Today's selection is a static substituter list plus a low
`connect-timeout` (`nixosModules.consumer`), which taxes every off-network build by that timeout
per substituter. Replace it with a localhost proxy substituter that probes box reachability and
routes instantly, removing the tax entirely.

Config-mutating the generated `nix.conf` in place does not fit NixOS well, so the shim must be a
small always-running proxy, not a substituter-list rewriter.

Needs pluggable **discovery backends**:

- **static-endpoint** — configured box URL plus a reachability probe. Works anywhere, including
  inside a k8s CNI overlay.
- **mDNS** — zero-config for bare-metal/LAN-host deployments. Cannot cross a k3s CNI overlay
  (link-local TTL=1 multicast does not bridge to the LAN); becomes relevant once the box moves to
  a bare-metal host.

### Full client-side routing engine

Strictly a superset of the shim: multiple substituters, explicit priority/policy rules, metrics.
Genuinely interesting, but revisit only once the shim exists. Terminology note: "routing" is
reserved for this, "selection" for the box-vs-remote decision (`CONTEXT.md`).

## Box internals

### Re-compression

Objects are stored byte-identical as received. Candidates: recompress xz→zstd for LAN throughput,
or normalize compression at mirror-up. Owner interest exists, but this needs measurement (actual
LAN throughput vs. decompression cost on the consumer) before any design work.

### Mirror-up filtering

Mirror-up pushes everything the remote lacks from local-origin generations. Smarter logic later —
e.g. skip paths an upstream already serves, via signature check or upstream HEAD probe. Constraint
if signature filtering is ever adopted: the CLI push must sign only paths lacking a trusted
signature, not the whole closure.

### Pull-through serving on miss

Out of scope by decision (ADR-0008 / handoff-rewrite §10): the box 404s on miss and the consumer's
substituter list falls back. Revisit only if the fallback latency ever measurably hurts.

### Prometheus `/metrics`

`/status` JSON plus structured logs cover current needs. Add when something actually scrapes it.

## Not queued

- **Dedup / attic-style content-defined chunking** — not adopting `attic` (ADR-0002). Revisit only
  if box storage or bandwidth becomes a real constraint, on evidence.
- **Public multi-tenant employee cache** — offering spare storage/throughput as a shared cache for
  personal projects. Needs a team discussion (cost, support, abuse boundaries) before any design.
