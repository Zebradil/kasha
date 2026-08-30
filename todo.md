# todo

What is deliberately not built yet, and what would justify building it. Replaces the old
`docs/cache-roadmap.md` and the "deliberately deferred" list in
`docs/agents/handoff-rewrite.md`. Terminology: `CONTEXT.md`. Decisions already made:
`docs/adr/`.

Nothing here is committed work — each item states the evidence that should trigger it, so
none of it gets built on speculation.

## Open bugs / operational gaps

### Remote sweep is slow and was unobservable

The first successful scheduled run took over 30 minutes with no output at all.
`remote_sweep` is one HTTP round trip per object across three phases (manifest
reads, retained-narinfo reads, deletes), and logged nothing until it finished.
Progress is now reported every 100 objects, so the next run says whether it is
slow or wedged, and where.

If the answer is "thousands of sequential GETs", the fix is concurrency in
`src/gc.rs` — but measure first: a bucket that has never been swept is large
once and small thereafter, and the problem may not survive its own first run.

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

### Selection shim / routing engine → sito

Designed and moved out: [sito](https://github.com/Zebradil/sito) is a standalone localhost
substituter proxy (streaming, tiered upstream selection, reachability probes) that covers both
the shim and the routing-engine ideas for roaming hosts. Neither project depends on the other;
`nixosModules.consumer` stays the right answer for hosts pinned to the LAN. See sito's
`docs/adr/` for the decisions (mDNS discovery was dropped there as likely never needed).

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
