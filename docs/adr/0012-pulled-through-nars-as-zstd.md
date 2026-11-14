# A box stores the NARs it pulls through as zstd

Partly supersedes ADR-0008 (objects stored byte-identical as received) and ADR-0011 (a
pulled-through NAR is stored as received). Issue: #63. Nix is cited at tag 2.34.9.

Nix compresses NARs for a binary cache with xz unless told otherwise
(`src/libstore/include/nix/store/binary-cache-store.hh:21-24`), and a consumer decompresses
every NAR it substitutes. Measured on one arm64 laptop core with xz 5.8.4 and zstd 1.5.7, on
the 120.7 MB NAR of `/nix/store/0788rbnl2p558jnyjlm2s2f9r5sxpq3h-python3-3.14.6` from
`cache.nixos.org`: as xz (`-6`, 26.3 MB) it decodes in 0.81 s, as zstd (`-3`, 36.0 MB) in
0.07 s. Level 3 encodes it in 0.20 s, level 6 (34.1 MB) in 0.52 s with the same decode time.

```sh
curl -so py.nar.zst https://cache.nixos.org/nar/1y02s12fqf0rfnmy3ab4ahb0alaa1n02b4hbp1cbrpznfb3c8hnn.nar.zst
zstd -dq py.nar.zst -o py.nar && xz -6 -T1 -k py.nar && zstd -q -3 -T1 py.nar -o py.3.zst
time xz -dc -T1 py.nar.xz >/dev/null; time zstd -dqc py.3.zst >/dev/null
```

On a 1 Gb/s LAN the 9.7 MB more take 0.08 s, a tenth of the decoding they save. The gain is
for xz sources: `cache.nixos.org` serves current paths as zstd named by `NarHash` (the NAR
above) but older ones as xz (`w9yy7v61ipb5rx6i35zq1mvc2iqfmps1-hello-2.10`, nixpkgs 20.09),
and so is every cache that `nix copy` filled with its default.

**Decision.** A NAR the box pulls through (ADR-0011), from the remote cache or an upstream
cache, is stored as zstd at level 3, under `nar/<nix32 NarHash>.nar.zst`, whatever the source's
`Compression`. A source that serves zstd is stored as received, not re-encoded. The box's
narinfo keeps the source's lines and signatures and says `Compression: zstd`; its `FileHash`
and `FileSize`, once stored, are those of the zstd file. Pushed objects, objects mirrored down
and objects already on disk stay as they arrived and are served as they are.

Signatures stay valid: they cover the store path, `NarHash`, `NarSize` and references
(`src/libstore/path-info.cc:43-50`), not the file. Level 3 because a client may be waiting:
a higher level costs the box more time than its smaller file saves the client.

Consequences:

- A worker now decodes and encodes. Memory per worker: the decoder (at most 128 MiB for xz
  and zstd sources) plus a level-3 encoder; CPU is the encode time on top of the decode.
- The stored name no longer depends on the source's encoding. A `.nar.zst` of the same NAR
  already in the store is checked by decoding it and reused; zstd output need not be
  deterministic. A `.nar.xz` of it stored before this change coexists until the sweep removes
  it, and a client holding the old narinfo still gets that file. The box stores and serves
  more bytes than an xz source held.
