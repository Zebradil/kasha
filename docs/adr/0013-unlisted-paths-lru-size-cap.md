# Paths no retained manifest lists are kept within a size cap, least recently used first

Partly supersedes ADR-0008 (box sweep: 24h grace window, no lock) and ADR-0011 (the retention
gap it left for pulled-through paths). This is the rule ADR-0011 deferred.

Generation retention knows only paths a manifest lists. Where manifests exist, the box sweep
deletes every other path a day after it arrived, however often it is read; on a box without
manifests, the main pull-through case (one box per build host, no remote cache), nothing is
ever deleted.

**Decision.** `KASHA_UNLISTED_MAX_BYTES` (`serve` and `sweep`; unset by default) caps the
**unlisted** paths: narinfos no retained manifest lists, and the NARs only they name. It counts
file sizes: each unlisted narinfo, and each such NAR once however many narinfos name it; a
`.nar.xz` and a `.nar.zst` of one NAR are two files. A NAR a listed narinfo names is neither
counted nor evicted. The cap applies with pull-through off too: a path pushed without a
manifest, or dropped by generation retention, is unlisted as well.

- *Use* is a GET or HEAD of a narinfo or NAR file, or storing it. It is recorded as the file's
  mtime, rewritten by a read only when older than a minute; a path's recency is the newer of its
  narinfo's and its NAR's. It lives on disk, so a restart loses nothing.
- *Eviction* runs in `serve`, with or without a remote cache: a pass every 5 minutes, and one
  within 5 seconds once the NAR bytes pulled through or pushed since the last pass could take the
  unlisted paths over the cap, and are at least a tenth of it. Over it, the least recently used
  go until 90% of the cap is left. A path used in the last 10 minutes is never evicted, so the
  store can exceed the cap by what clients are fetching. `kasha sweep` runs a pass too.
- *Generation retention* is unchanged for listed paths. Under the cap the sweep no longer
  deletes unlisted narinfos by age; a path that leaves its last retained manifest becomes
  unlisted with the recency of its last use, so an unused one goes first.
- *Order and locking.* A narinfo goes before its NAR, and a NAR only after the pass removed
  every narinfo naming it, and never while a pull-through fetch produces or reuses it. Each
  removal is decided under the store's index lock after reading the mtime again; a NAR is moved
  aside under it and unlinked outside. Every narinfo writer touches its NAR under that lock, so
  a NAR that gains a narinfo after the pass listed the store is young and stays. A NAR no
  narinfo names goes after the grace window, as in the sweep.
- *Failures* are logged; a pass skips what it cannot remove. No request waits on a pass.

With the cap unset nothing changes: reads rewrite no mtime, and the sweep runs as before.

Rejected: an in-memory recency map (lost on restart, so mtime is needed anyway); `atime`
(`noatime` and `relatime` mounts); evicting inside the publish path (a client-facing write
would wait on deletions); a cap on the whole store (it would evict generations retention keeps).
