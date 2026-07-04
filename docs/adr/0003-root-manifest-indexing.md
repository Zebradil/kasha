# Discovery and future GC both key off root manifests, not a full object index

Nix has no cheap way to list "all paths in this cache," and the box needs to know
which new generations to eagerly pull, so every writer (CI, local push) publishes a
small root manifest (`roots/<flake>/<gen>.json`) listing just that generation's
top-level output paths. Readers discover new generations by listing the `roots/`
prefix; `nix copy` expands a root into its full closure on its own, so manifests never
need to enumerate closures. The same root manifests will later double as GC roots
(mark-sweep from retained generations), so this single mechanism serves both discovery
and future retention — we deliberately avoided introducing a separate index database
before evidence shows manifest scanning doesn't scale.
