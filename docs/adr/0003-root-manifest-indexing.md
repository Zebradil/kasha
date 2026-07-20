# Discovery and future GC both key off root manifests, not a full object index

Nix has no cheap way to list "all paths in this cache," and the box needs to know
which new generations to eagerly pull, so every writer (CI, local push) publishes a
small root manifest (`roots/<flake>/<gen>.json`, `version: 2`) listing just that
generation's top-level roots as `{outPath, drvPath}` objects. Readers discover new
generations by listing the `roots/` prefix; a root's `drvPath` is the entry point to
its input closure, so manifests never need to enumerate closures. The same root
manifests will later double as GC roots (mark-sweep from the retained `outPath`s), so
this single mechanism serves both discovery and future retention — we deliberately
avoided introducing a separate index database before evidence shows manifest scanning
doesn't scale.

The box never builds a top-level. It is a Linux host and cannot assemble a
cross-system (e.g. aarch64-darwin) top output, and no cache holds that output anyway:
CI publishes only the recipe (the top-level's `.drv` requisite closure) plus the
newly-built input NARs. So to mirror a generation the box (1) copies the `.drv`
recipe from the remote — only the remote stores `.drv` files; upstream caches store
outputs — then (2) substitutes the drv's input output-closure *minus* the top-level
output from remote + upstream. Everything it realises is already cached, so nothing
builds. Each consumer assembles (builds) its own top-level from that cached input
closure at deploy time; assembly verification moves to the consumer.
