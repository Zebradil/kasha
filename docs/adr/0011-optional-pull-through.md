# Pull-through on a miss is an optional box mode, beside the eager replica

**Partly superseded by [ADR-0012](0012-pulled-through-nars-as-zstd.md)** — a pulled-through
NAR is stored as zstd, not as received: the box's narinfo always says `Compression: zstd`, and
"its NAR still is" and the **Compression** paragraph no longer hold. The rest stands.

**Partly superseded by [ADR-0013](0013-unlisted-paths-lru-size-cap.md)** — the rule for paths
no retained manifest lists is decided there. With `KASHA_UNLISTED_MAX_BYTES` set, the gap the
**Retention** paragraph describes is closed, with or without a remote cache; with it unset, the
paragraph still holds.

Supersedes parts of ADR-0001 (eager bidirectional replica, not a pull-through proxy) and
ADR-0008 (single Rust binary), listed below. The eager replica itself and ADR-0004 (box
holds no signing key) survive unchanged. Issue: #65. Nix behaviour is cited from its source
at tag 2.34.9.

ADR-0001's read-path argument rested on one premise: each network has a single consumer
that keeps what it already fetched, so the first request for a path is also the last, and
fetching it on demand is no faster than reading the remote cache directly. ADR-0008's design
record (`docs/agents/handoff-rewrite.md` §10) made that the serve contract: `404` on a miss,
and the consumer's substituter list falls back.

The premise fails for a box that serves many short-lived consumers. Take one box per build
host, serving ephemeral VMs. Each VM starts every job with an empty Nix store, so it fetches
every path the job needs, every job. Generation manifests cannot predict that set: most of
it is paths from public upstream caches (`cache.nixos.org`) and flake input sources, which
no writer publishes a generation for. With `404` on a miss every VM repeats the same
download from the internet, per job; a box that fetches on the first miss pays it once per
host.

**Decision.** A box has an optional pull-through mode, off by default; with it off a box
behaves exactly as before. With it on:

- **Trigger.** Only `GET <hash>.narinfo` for a path the box lacks starts a fetch. `HEAD` is
  answered from the store alone; `GET nar/<file>` from the store or from a fetch of that
  file already in flight.
- **Sources.** The remote cache first, when the box has one, then the upstream caches in
  configured order: mirror-down's order, upstream list and trusted keys. The remote cache is
  a source because builds that no mirrored manifest lists exist nowhere else; the cost is
  one remote round trip before every upstream fetch.
- **Narinfo first.** The first source whose narinfo passes the trust check (a trusted
  signature or a `CA` field that reproduces the store path, ADR-0004) and names the
  requested path wins. The box answers at once with a narinfo of its own: the source's
  fields and signatures, a `URL` the box derives from `NarHash` and never takes from the
  source, and always a `Compression` field, since Nix reads a missing one as bzip2
  (`src/libstore/nar-info.cc:93-94`). Until the NAR is stored that narinfo has no `FileHash`
  or `FileSize`, which Nix does not require (`nar-info.cc:96-104`); the narinfo written to
  disk carries those of the stored file.
- **NAR in the background.** The download from the same source starts immediately on a
  bounded worker pool, and a NAR a client is waiting for goes ahead of unrequested ones.
  `GET nar/<file>` is served when the file is stored, joins the fetch and is served on its
  completion when it is in flight, and gets `404` when the fetch failed or the file is
  unknown. Pending narinfos and queued downloads are capped; past the cap a narinfo miss is
  answered `404` and is not remembered as a miss. The number is left to the implementation.
- **Verify, then store.** The NAR, decompressed, must hash to `NarHash` and have `NarSize`.
  Only then is it stored, and only then is the narinfo written; until that point the narinfo
  lives in memory. A pulled-through object never replaces an existing one.
- **Misses.** A miss on every source stays a `404`. It is remembered in memory, by store
  path hash, for a negative TTL (60 s by default, configurable), so that a path no cache
  will ever hold, such as a job's own not yet built outputs, is not looked up on every
  source by every VM. A narinfo that fails the trust check or names another path is a miss
  on that source. A source whose narinfo the box cannot verify, for an unsupported
  `Compression` or a `NarHash` that is not sha256, counts as failed, and the lookup goes on
  to the next source. A lookup in which any source failed is not a miss and is not
  remembered.
- **Priority.** The `Priority` in `nix-cache-info` becomes configurable; the default stays
  `10`. Nix sorts substituters by it and falls back to list order among equals
  (`src/libstore/store-registration.cc:98`), so several caches in one substituter list are
  ordered by giving them distinct values. `nix-cache-info` only sets the default
  (`src/libstore/binary-cache-store.cc:67-68`, `http-binary-cache-store.cc:71`); a consumer
  can override it with `?priority=` on the substituter URL
  (`src/libstore/include/nix/store/store-api.hh:218-224`).

Answering the narinfo before the NAR is fetched follows from how Nix substitutes. Before it
starts any download, Nix queries the narinfos of the whole closure, following references,
from a pool of `http-connections` threads (`src/libstore/build/worker.cc:339-340`,
`src/libstore/misc.cc:106-107`, `302-303`). A narinfo answer that waited for its NAR would
hold that query until the box had downloaded the entire closure: the client would start no
NAR download before the box had finished its last, and the two would never overlap. And a
NAR `404` after a good narinfo is not a failure to Nix: it warns and tries the next
substituter (`src/libstore/build/substitution-goal.cc:273-276`). For the same reason the
box does not hide a source that has a narinfo but not its NAR by trying another source; the
NAR request gets `404`.

Three costs are accepted. A client that joins a fetch receives no bytes until the box holds
the whole NAR; past `stalled-download-timeout` (300 s,
`src/libstore/include/nix/store/filetransfer.hh:66-74`) Nix abandons the request and
retries. Streaming to waiters is a possible later optimization, not decided here. And a
narinfo may be asked for without its NAR ever being requested; the download is stored
anyway and falls under retention. And a long-lived consumer caches a positive narinfo, for
30 days by default (`src/libstore/include/nix/store/globals.hh:75-78`): after a failed
background fetch it gets a NAR `404`, falls back each time, and does not trigger
pull-through again until that entry expires.

Eager mirroring runs unchanged beside the mode. Pull-through never triggers mirror-up; a
path it fetched goes up only when a local-origin manifest lists it.

A push to a box with the mode on goes through the same trigger. `nix copy` asks the
destination which paths it already has with a narinfo `GET` per path
(`src/libstore/store-api.cc:1026`, `779`) and uses `HEAD` only later, when it uploads
(`src/libstore/binary-cache-store.cc:296`, `386-391`). So the box pulls every pushed path
that one of its sources holds, and the writer skips it; only paths no source has are
uploaded. If such a background fetch then fails, the box lacks a path the writer believes
it pushed. With a remote cache, mirror-down fills the gap once a manifest lists the path;
without one, only a later push brings it. This is accepted for now; exempting
authenticated requests is a possible later change, not decided here.

The `HEAD` rule has a mirror image. A client that uses the box as the store it copies
from, as `nix copy --from` does, builds the path in that store
(`src/nix/copy.cc:33`, `src/libstore/build/entry-points.cc:51`), which first asks whether
it is valid there (`src/libstore/build/substitution-goal.cc:36`). A binary cache answers
that with a `HEAD`, so the client sees only stored paths, unless its narinfo cache
already marks the path valid (`src/libstore/store-api.cc:544-562`). Substitution asks
with a `GET` (`store-api.cc:479`, `substitution-goal.cc:76`).

**What this supersedes.**

- *ADR-0001.* Its title and its "Considered" paragraph reject pull-through for the box. That
  now stands only for pull-through as the sync model, in place of eager mirroring, and for
  the single-consumer network.
- *ADR-0008.* Five statements change. ADR-0001 no longer survives unchanged. The serve
  contract of its design record (§10, "404 on miss … No pull-through") becomes the default
  instead of the only behaviour. "Retention source of truth is manifest presence" covers
  only paths a manifest lists (see retention). "Box-retained ⊆ remote-retained by
  construction" holds for generations only: the box keeps pulled-through paths the remote
  cache never held. And a pulled-through narinfo is the box's own rendering, not stored
  "byte-identical as received"; its NAR still is.

**Trust.** The box still originates nothing and holds no signing key. The narinfo it serves
carries the source's signatures untouched, and they stay valid: a signature covers the
store path, NAR hash, NAR size and references, not `URL` or the compressed file. A source
whose key the box does not trust can supply content-addressed paths only, and consumers
verify signatures themselves as before. Reads are unauthenticated, so anyone who can reach
the box can make it download and store what its sources serve, but nothing else: the
narinfo must name the requested path and the NAR file is named from `NarHash`, so a source
cannot place content under another path's narinfo or NAR.

The NAR check is stricter than on push and mirror-down, which verify the narinfo and leave
the bytes to the consumer's import check: a corrupt download, once stored, would be served
to every later consumer. Only a `fixed:r:sha256` address hashes the NAR itself; for the
other `CA` kinds the check binds the bytes to the narinfo, not to the path, and the
consumer's import check closes that gap. To hash a NAR the box has to decompress it, which
it never did before; it still runs no nix.

**Failure behaviour.** A box that cannot supply an object answers `404`, never `5xx`. A
`404` is a plain miss: Nix goes on to the next substituter or builds. A `5xx` costs the
client retries with backoff (`src/libstore/filetransfer.cc:661-683`, `747`) and an error
line before it goes on (`substitution-goal.cc:51-54`, `81-83`). With `fallback` on it also
disables the box as a substituter for 60 s
(`src/libstore/http-binary-cache-store.cc:94-103`); with `fallback` off it fails the goal
when the box was the last substituter tried (`substitution-goal.cc:147-149`). Every failure
below is logged and not remembered, so the next request tries again.

- *Source down or slow.* The narinfo lookup and a stalled NAR download are each bounded in
  time, and exceeding the bound is a failure. A lookup then goes on to the next source.
- *Bad NAR.* Bytes that fail the hash check are dropped with the pending narinfo.
- *Store write failure*, a full disk included, abandons the fetch.
- *Concurrent requests.* Requests for one missing path share one lookup and one download.
  A client that disconnects does not cancel a fetch.
- *Concurrent writers.* Mirror-down or a push can write an object while pull-through
  fetches it. Writers never share a temporary file, and an object appears only complete.
- *Restart mid-fetch.* Pending narinfos are gone and the NAR request gets `404`. A
  pulled-through narinfo on disk always has its NAR on disk.

**Retention.** The sweep classifies a path, not its origin: a path is kept if a retained
manifest lists it or the rule for the rest keeps it, and a manifest may come to list a path
pull-through fetched first. That rule, for paths no retained manifest lists, is
least-recently-used eviction under a size cap, a separate decision. Until it exists the gap
is an accepted operator constraint. Box GC sweeps every object no retained manifest lists
once it is past the grace window (ADR-0008): where manifests exist, a pulled-through path
goes about a day after it arrived, however often it is read, and a manual `kasha sweep`
does the same on a box whose only manifests were pushed to it. A box without manifests is
never swept, and without a remote cache the in-process GC does not run at all. Such a box
grows without bound, and anyone who can reach it can fill its disk.

**Compression.** Stored NARs stay as received. Verification already decompresses each
pulled-through NAR, and the box already names the file and writes the narinfo, so storing
every path as zstd whatever its source encoding (#63, decided there) fits this decision.

Considered: mirroring eagerly what jobs need (rejected: no manifest lists it, and the set
is known only when a job asks); pull-through always on (rejected: on a single-consumer LAN
ADR-0001's argument still holds, and existing boxes would start making upstream requests
and storing paths no generation retains).
