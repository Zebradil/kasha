# todo

Open investigations not yet tracked as GitHub issues. Deferred features live in the issue
tracker (#63–#67), each with the evidence that should trigger it. Terminology: `CONTEXT.md`.
Decisions already made: `docs/adr/`.

## Dangling-narinfo warning (unresolved, cheap first step unrun)

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

The box cannot shorten the client's TTL: nix reads only `StoreDir`, `WantMassQuery` and
`Priority` from `/nix-cache-info` (`BinaryCacheStore::init`), so the positive TTL is a
client-side setting only.
