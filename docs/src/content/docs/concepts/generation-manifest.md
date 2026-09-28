---
title: Generation manifests
description: What a v3 generation manifest holds, why it lists the full build closure, and how readers use it.
---

A **generation** is one published set of build outputs for one flake. Each generation is described by exactly one
**generation manifest**: a small JSON object stored at `roots/<flake>/<gen>.json`, next to the NARs, in the remote
cache and on every box that mirrors it.

Manifests are how boxes find work. Nix has no cheap way to list every path in a cache, so readers list the `roots/`
prefix instead, and mirror exactly the paths each manifest names. A NAR without a manifest is invisible to every box.

## Format

```json
{
  "version": 3,
  "flake": "znix",
  "gen": "main-a1b2c3d-host",
  "branch": "main",
  "attr": "host",
  "timestamp": "2026-09-28T00:00:00Z",
  "closure": ["/nix/store/…-libiconv-115.100.1", "/nix/store/…-hello-2.12.3"]
}
```

| Field | Meaning |
| --- | --- |
| `version` | always `3`; anything else is rejected |
| `flake` | producer flake id; the `<flake>` in the object key |
| `gen` | generation id; the `<gen>` in the object key |
| `branch` | source branch; `main` selects the long retention tier |
| `attr` | the attr built; with `flake` and `branch`, the retention group |
| `timestamp` | RFC 3339 UTC; retention ages the generation by it |
| `closure` | every store path of the build closure, sorted, deduplicated |

A manifest is valid only when `version` is 3, `flake`, `gen`, `branch` and `attr` are non-empty, `closure` is non-empty
with every path under `/nix/store`, and `timestamp` parses. Mirror-down ignores an invalid manifest with a warning; the
remote sweep deletes it. A manifest pushed to a box must also match the key it is stored under.

The gen id is opaque: nothing parses it. Grouping and tiers come only from the explicit `branch` and `attr` fields.

## Why the full closure

Earlier versions listed only a generation's top-level roots and let the box walk their closures with Nix. That made the
box depend on Nix's closure awareness, and generations looped as incomplete whenever a `.drv` path listed as a root was
never uploaded. Version 3 lists the whole build closure the producer pushed, so mirroring is a plain fetch of named paths
and GC marking is the union of retained closures, with no narinfo walk.

`kasha-cache-push` includes each attr's `.drv` recipe closure (small text files) alongside its outputs, so a consuming
host can realize the top-level from the pushed inputs.

## Ordering

Writers publish the manifest last, after every path it lists: `kasha-cache-push` emits only after a successful push,
and the box's mirror-up uploads the manifest after all NARs and narinfos. Readers therefore only see manifests for
complete generations, and the GC grace window covers objects whose manifest has not landed yet.

Produce manifests with [`kasha emit`](../../reference/cli/kasha-emit/); see
[Publish generations from CI](../../guides/publish-from-ci/).

Rationale: [ADR-0003](https://github.com/Zebradil/kasha/blob/main/docs/adr/0003-root-manifest-indexing.md) (superseded)
and [ADR-0008](https://github.com/Zebradil/kasha/blob/main/docs/adr/0008-single-rust-binary.md).
