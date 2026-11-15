# Paths no retained manifest lists are kept within a size cap, least recently used first

Partly supersedes ADR-0008 (box sweep: 24h grace window, no lock) and ADR-0011 (the retention
gap it left for pulled-through paths). This is the rule ADR-0011 deferred. Generation retention
knows only listed paths: where manifests exist the sweep deletes the rest a day after arrival,
however often read; on a box without manifests, the main pull-through case, nothing goes.

**Decision.** `KASHA_UNLISTED_MAX_BYTES` (`serve` and `sweep`; unset by default) caps the
**unlisted** paths: narinfos no retained manifest lists, and the NARs only they name. It counts
file sizes: each unlisted narinfo, and each such NAR once however many narinfos name it; a
`.nar.xz` and a `.nar.zst` of one NAR are two files. A NAR a listed narinfo names is neither
counted nor evicted. It applies with pull-through off too: a path pushed without a manifest, or
dropped by generation retention, is unlisted as well.

- *Use* is a GET or HEAD of a narinfo or NAR file, or storing it, recorded as the file's mtime,
  rewritten by a read only when older than a minute (or far in the future). A path's recency is
  the newer of its narinfo's and its NAR's; on disk, a restart loses nothing. A client that
  probes every path keeps every path recent.
- *Eviction* runs in `serve`, with or without a remote cache: a pass every 5 minutes while
  bytes arrive or work is left, hourly otherwise, and within 5 seconds once the NAR bytes pulled
  through or pushed could take the class over the cap (and are a tenth of it). Over it, the
  least recently used go until 90% of the cap is left; `kasha sweep` runs a pass too. A path
  used in the last 10 minutes is never evicted, so the store can exceed the cap by the paths
  stored or read in 10 minutes; free disk space is not looked at. A client asking for a NAR longer after its
  narinfo (a large closure; a long-lived one caches narinfos 30 days) gets `404` and falls back.
- *Generation retention* is unchanged for listed paths. Under the cap the sweep no longer
  deletes unlisted narinfos by age; a path that leaves its last retained manifest becomes
  unlisted with the recency of its last use.
- *Order and locking.* A narinfo goes before its NAR; a NAR only after the pass removed every
  narinfo naming it. A path whose NAR a pull-through fetch produces or reuses stays whole. Each
  removal is decided under the index lock after reading the mtime again; a NAR is renamed to a
  dead temp name, so no new file is needed, and unlinked outside. Every narinfo writer touches
  its NAR under that lock; mirror-down fails, leaving a gap, when it is gone. So within `serve`
  no deletion leaves a narinfo without its NAR; `kasha sweep` in another process has no lock.
- *Failures* are logged. A removal that fails stops the pass, its NAR left for the grace window.

With the cap unset, reads rewrite no mtime and the sweep runs as before.

Rejected: an in-memory recency map (lost on restart, so mtime is needed anyway); `atime`
(`noatime` and `relatime` mounts); evicting inside the publish path (a client-facing write
would wait on deletions); a cap on the whole store (it would evict generations retention keeps).
