# Handoff — narinfo served, NAR missing (`does not exist in binary cache`)

**Goal:** decide whether the box ever serves a narinfo whose NAR it does not
have, and if so, find what breaks the write/delete ordering that is supposed to
make that impossible. Current evidence points at a **stale client-side narinfo
cache** rather than a box bug — confirm or refute that first, cheaply, before
touching any code.

## The observation

A `nix build` on a Mac against the box printed, for six paths:

```
warning: file 'nar/14q7l94p3qnijcvygfdrs3hk3zh11flj8vbm6138vv2s5libf8ij.nar?hash=prip7nslhk7b9hfws6y5y9lzfxjskanv' does not exist in binary cache 'https://kasha.lan.zebradil.dev'
copying path '/nix/store/prip7nslhk7b9hfws6y5y9lzfxjskanv-gnutar-1.35' from 'https://cache.nixos.org'...
```

Nix only asks for a NAR URL it learned from a narinfo, so at that instant nix
believed the box had the object. It fell back to another substituter and the
build succeeded — this is a warning, not a failure. Affected hashes seen:
`prip7nslhk7b9hfws6y5y9lzfxjskanv` (gnutar), `jjr7004giah45yh0m89g54qds51qwa72`
(libpsl), `w0hwcvhjihq11g77w0h5drqklxxj4ih2` (curl),
`2skl7vw67p05nn884nhgp044aiq3can7`, `qndx79izh6lanm535xj9pbayy6j8fz5n`,
`mfkdmplffnbc0av8r6pknl796a6b1r2n`.

## What the code promises

Both write paths order NAR before narinfo, so the index is never supposed to
point at a missing object:

- `src/mirror.rs` `fetch_path` — `put_nar` then `put_narinfo`, with the comment
  `// nar first: index never dangles`.
- `src/server.rs` PUT — ingest order is the client's (`nix copy` uploads NARs
  first); each narinfo is signature-checked before it is indexed.
- `src/gc.rs` `box_sweep` — one pass, marks from retained manifests, deletes
  unmarked narinfos and unmarked NARs under a shared 24h grace.

The index (`Store.index`, hash → URL) is updated in `put_narinfo` and
`remove_narinfo`, so it does not go stale relative to disk between boots.

## Evidence gathered (do not re-derive)

Measured on 2026-08-23, ~30 min after the observation:

- All six narinfos return **404 from the box now**, and their NARs 404 on the
  box and on R2. So the box is not *currently* holding a dangling narinfo for
  any of them.
- The serving pod (`kasha-54cccc9dd6-fm748`) started `2026-08-23T11:53:58Z`,
  `restartCount: 0`, and the warnings appeared ~12:20Z.
- `gc_loop` in `src/main.rs:303` **sleeps before its first sweep**
  (`KASHA_GC_INTERVAL=86400`), and no `box sweep done` line appears in that
  pod's log. **No box GC ran between the observation and the probe.**

Those three together mean the box already 404'd those narinfos *at the time of
the warning* — which nix could not have known unless it had the narinfo from
somewhere else. The obvious somewhere else is nix's own
`~/.cache/nix/binary-cache-detsys-v2.sqlite`, whose
`narinfo-cache-positive-ttl` defaults to 30 days. Its mtime was unchanged by
the build, consistent with a read-only cache hit. The entries for these hashes
are gone from that DB now (invalidated after the failed fetch), so this could
not be confirmed directly after the fact.

The competing explanation is that a **previous** pod's sweep deleted narinfo
and NAR together while this Mac held a cached narinfo — which lands on the same
verdict: the box is consistent, the client was stale.

## First step: confirm or refute the stale-cache theory

```sh
# on the Mac that produced the warning
rm ~/.cache/nix/binary-cache-detsys-v2.sqlite      # or: nix store gc on the client's narinfo cache
nix build --no-link .#kasha-cache-push             # or any target that hit the box
```

- Warnings **gone** → client-side stale cache. Nothing to fix in kasha; close it
  out, and consider whether the box should advertise a shorter TTL (see below).
- Warnings **recur** → the box really is serving a dangling narinfo. Continue to
  the audit.

## If it recurs: audit the box store

There is no shell in the box image (static binary + cacert), so mount the PVC:

```sh
kubectl --context homelab-lan -n kasha debug \
  pod/kasha-… --image=busybox --target=kasha -- sh
# then, over /kasha:
#   for every <hash>.narinfo, read its URL: line and stat nar/<file>
```

Every narinfo whose `URL:` target is absent is a live dangling entry. What to
check next, in order:

1. **`box_sweep` NAR liveness** (`src/gc.rs:61`) — `live_nars` is built from
   `store.url_of(h)` for `h` in `mark`. A narinfo that survives the sweep but
   whose hash is missing from the index (or whose URL changed) would keep the
   narinfo and drop the NAR. Check whether a hash can be in `mark` and marked
   live while `url_of` returns a *different* URL than the file on disk.
2. **Compression-variant rewrites** — `mirror.rs` notes NAR keys are
   compression-specific and a narinfo+NAR pair must come from one source. If the
   same hash is re-fetched from a different source, `put_narinfo` overwrites the
   index URL; the previously written NAR becomes an orphan. That direction is
   safe (orphan, not dangling), but the reverse ordering under a concurrent
   sweep is worth a look.
3. **Partial `put_nar`** — a failed/truncated NAR write followed by a successful
   narinfo write. `fetch_path` returns early on a `put_nar` error, but confirm a
   half-written NAR file is not left behind under the same name.

## Nice-to-have regardless of the verdict

The box's `/nix-cache-info` could advertise a low `narinfo-cache-positive-ttl`
so clients re-check a LAN box instead of trusting a month-old narinfo. That
would make stale-client warnings self-healing, whichever way this lands. Check
whether nix honours it from the cache side before building it.

## Not in scope

The 91 gaps reported in `/status` are unrelated: those are the pre-signing
legacy narinfos in R2 that fail kasha's signature gate. Separate cleanup,
already diagnosed.
