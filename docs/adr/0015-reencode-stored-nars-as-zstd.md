# A box re-encodes the NARs it stores as zstd, after the fact

Partly supersedes ADR-0008 (objects stored byte-identical as received) and ADR-0012 (pushed
objects, objects mirrored down and objects already on disk stay as they arrived). Issue: #126.
Code: `src/reencode.rs`, `Store::publish_reencoded` in `src/store.rs`.

ADR-0012 stores pulled-through NARs as zstd; every other route into the box stores a NAR as
received, so xz keeps reaching consumers. A client can push xz, and mirror-down stores what
the remote cache or an upstream cache holds. On one homelab box on 2026-11-15, `nar/` held
4,491 `.nar.xz` files (0.87 GB) beside 61,714 `.nar.zst` (43.3 GB). On a 10 GbE LAN, an
ephemeral 4-vCPU consumer substituting 932 paths took 117.6 s from xz and 87.8 s from zstd
(#126).

**Decision.** `KASHA_REENCODE` (`serve`, off by default; off, a box behaves as before) turns on
a re-encoder. For a path whose narinfo names an `xz`, `bzip2` or uncompressed NAR file, one
function:

- *Reads* the narinfo, decodes the stored NAR and checks it against `NarHash` and `NarSize`
  (`pull::verify`), writing it as zstd at ADR-0012's level 3 under `nar/<nix32 NarHash>.nar.zst`,
  pull-through's name, so the two share files. A file of that name already stored is checked
  by decoding and reused.
- *Renders* the narinfo as pull-through does (`pull::render`): `URL`, `Compression`, `FileHash`
  and `FileSize` describe the zstd file; every other line and signature is kept, and the
  signatures stay valid (ADR-0012).
- *Swaps* under the index lock: the NAR is stored without replacing a file (hard link), then the
  narinfo is replaced by rename only while the stored one still equals the bytes read. A
  narinfo pushed or mirrored down meanwhile wins and the path is tried again on a later pass.

Two callers:

- *A pass on the housekeeping thread* (`gc::maintain`), which runs on every box, with or without
  a remote cache, every 5 minutes. Candidates are the indexed narinfos whose `URL` file name is
  not `.zst`, read from memory.
- *Mirror-up* (`Mirror::push_gen`), before it uploads a path of a local-origin generation, with
  no budget: the remote cache receives zstd, or every box that mirrors the path down re-encodes
  the same NAR again. A path it cannot re-encode is uploaded as it is, as before.

*Recency.* A listed path is always a candidate. An unlisted one is re-encoded only when its last
use (the newer of its narinfo's and its NAR's mtime, ADR-0013) is within 12 hours: a cold one is
the next to be evicted or swept, and re-encoding it costs CPU, and disk until the old file goes,
for a path nobody reads. The new NAR file and the narinfo get that last use as their mtime, so
eviction ranks the path where it was; every other narinfo writer touches its NAR (ADR-0013) and
would make a cold path the most recent. Both times are set under the index lock, after the link
and the rename. They are never older than 12 hours: the sweep and orphan removal list NARs first
and later remove one no narinfo named at listing time once its mtime is past 24 hours, so a new
file dated past that could go while a narinfo names it. Half the window keeps every new file
inside it, `kasha sweep` in another process included, which takes no lock.

*A reused zstd file* is touched to now, as by every narinfo writer, and never dated earlier.
Eviction removes a NAR once the last narinfo it listed as naming it is gone, judged under the
lock by whether the file was used in the last 10 minutes; only a recent mtime keeps it for the
path that names it now (`a_re_encoded_narinfo_keeps_its_nar_when_the_swap_cuts_into_a_pass` in
`src/gc.rs`). The cost: that path, and any other naming the file, look used now.

*Budget.* A pass decodes at most 1 GiB of `NarSize`, bar the path that crosses it, which is done
whole. A count of paths would bound neither CPU nor disk: NARs range from kilobytes to
gigabytes. At ADR-0012's single-core rates for xz (a 120.7 MB NAR decoded in 0.81 s and encoded
at level 3 in 0.20 s), 1 GiB is about 9 s of one core before hashing, about 3 % of a core per
5-minute pass; bzip2 decodes more slowly, not measured here. A backlog drains at up to 288 GiB
of `NarSize` a day. There is no knob.

*Failures.* A `NarHash` or `NarSize` mismatch, a NAR that does not decode, an unsupported
`Compression`, a `NarHash` that is not sha256, or a stored file of the zstd name holding another
NAR leaves the object untouched, is counted, and is remembered in memory by store hash so the
path is not tried again until restart. A pushed NAR is never decoded at `PUT`, so this is its
first content check. A failed write is told from a bad NAR by checking again without writing;
store errors are counted apart and retried.

*Old files.* Nothing new deletes them, and their mtime is left as it is. With
`KASHA_UNLISTED_MAX_BYTES` set, eviction's orphan pass removes a NAR no narinfo names once its
mtime is past 24 hours, on every box; with a remote cache and at least one manifest, the box
sweep does. A box with neither keeps them. Until then an old file counts against no cap
(ADR-0013 counts a NAR only while a narinfo names it), so the store exceeds the cap by those
bytes for up to a day. A client holding the old narinfo gets the old file while it is there;
for a path last used more than a day ago that can be until the next pass, after which the client
gets `404` and falls back to its next substituter (ADR-0011).

*Metrics.* Always on: NAR files on disk and their bytes by the encoding their name says, from
the cached `nar/` scan (`kasha_nar_compression_objects`, `kasha_nar_compression_bytes`), and NAR
files stored by arrival route and source encoding (`kasha_nar_stored_total`). With the flag:
`kasha_reencode_paths_total{result}` and `kasha_reencode_bytes_total{file}`.

Rejected (#126):

- *Re-encoding on mirror-down* (`Mirror::fetch_path`). No lag, but the stored file name no
  longer matches the source's, so the rule that keeps a stored NAR of that name needs rework,
  and it covers neither pushes nor objects already on disk.
- *Re-encoding on `PUT`.* The NAR arrives before its narinfo, so the box cannot verify it until
  the narinfo comes, and would have to hold or rewrite the NAR and rewrite the client's `URL`.
  Rejecting a non-zstd NAR on `PUT` would be a few lines, if a hard guarantee is ever wanted.

**What this supersedes.**

- *ADR-0008.* "Objects are stored byte-identical as received": with the flag on, a pushed or
  mirrored-down NAR is stored again as zstd and its narinfo replaced by the box's rendering; the
  file as received stays until orphan removal takes it.
- *ADR-0012.* "Pushed objects, objects mirrored down and objects already on disk stay as they
  arrived and are served as they are": with the flag on, each is re-encoded once it has a
  narinfo.

Consequences:

- The box spends CPU on objects it only stored before, and for up to a day holds both files of a
  re-encoded NAR. A pass needs the memory of one pull-through worker: the decoder (at most
  128 MiB for xz) and a level-3 encoder.
- The remote cache holds more bytes: 3.1 GB as zstd against 2.7 GB as xz for the 932 paths
  above, paid by a consumer on a slow link.
- Objects already on the remote cache as xz stay xz there: the box holds no delete credentials
  (ADR-0008). Every box that mirrors one down re-encodes it.
- A pass and mirror-up can take the same path at once: one stores the file and swaps the
  narinfo, the other finds both changed and leaves them.

Open questions:

- On by default? Re-encoding an existing store costs CPU and, for a day, disk.
- The zstd level. ADR-0012 chose 3 because a client may be waiting; nobody waits for a pass, and
  level 6 was 5 % smaller at the same decode time. The pass keeps level 3 for now: one level,
  one constant.
- Whether the xz objects on the remote cache are acceptable or a job for `kasha gc`.
