# Release-tag generations never go stale

Partly supersedes ADR-0010 (its consequence for release tags). ADR-0010 accepted that an
old tag's generation expires once its flake's other tier moves `S` past it, on the view
that CI publishes tags for convenience, not as an archive. That view was wrong. Consumers
pin a release by commit (Renovate resolves `emit-manifest@<sha> # v0.9.0`), and kasha's own
release workflow republishes each release under a generation named for its tag precisely
so that pin keeps hitting the cache. Under ADR-0010 it did so for four weeks: the remote
sweep of 2026-11-10 deleted `roots/kasha/v0.9.0-*` and the `kasha-static` closure with it,
and every nix-ci consumer then rebuilt kasha from source on each run (98 derivations, about
2.5 minutes, 910 MiB fetched). v0.5.0 to v0.8.0 had gone the same way.

**Decision.** A group whose branch is a release tag is never stale. A release tag is `v`,
three dot-separated decimal numbers, and an optional non-empty `-` suffix (`v1.2.3`,
`v1.2.3-rc.1`). The count and age rules still apply, so the newest generation of each
`(flake, tag, attr)` group stays for good; republishing a tag replaces it.

**Consequence.** The remote cache grows by one closure per release, per attr, forever.
Releases are rare and mostly share paths with each other, so this is accepted. To drop an
old release, delete its manifest under `roots/`; the next sweep removes what only it held.
The box needs no change: its count-only policy already keeps the newest generation of every
group it mirrors.

The tag test is on the branch name because that is all a manifest records. A branch that
happens to be named like a release tag is kept like one.
