# Count retention lapses for stale groups, measured against the flake's own tier

ADR-0008 retains a generation when it is among the `N` newest of its
`(flake, branch, attr)` group or younger than `M`. The count rule has no end: once a
group stops receiving generations, its last `N` stay forever, each pinning a full
closure. Groups stop for ordinary reasons: an attr is renamed or removed from the flake,
every release tag is a branch of its own, and a one-off branch name (`main-dirty`) never
recurs. On the reference bucket in October 2026 that was 20 groups and 36 manifests,
the oldest 47 days past its last generation.

A group is now **stale** when its newest generation trails the newest generation of
the same flake *and tier* (`main`, or every other branch) by at least `S`, default
4 weeks (`kasha gc --stale-weeks`, `0` disables). A stale group loses count retention;
the age rule still applies, so with `S ≥ M` nothing young is ever dropped by it.

Measuring against the flake's tier rather than the wall clock is the point. A wall-clock
rule would delete a dormant flake's whole cache after `S` — every consumer then rebuilds
from source the day the project gets touched again. Measured against the tier, a flake
that stops publishing keeps its newest generations indefinitely; only groups the flake
itself has moved past expire. Tiers are separate so pull-request churn never makes
`main` stale: an otherwise quiet `main` keeps its generations however many PRs build.

The consequence for release tags: an old tag's generation expires once the flake's other
tier (PRs, newer tags) has moved `S` past it. Consumers pinned to an old release build
from source. Accepted: CI publishes tags for convenience, not as an archive.

The box needs no change. It keeps a count-only subset of the manifests it mirrors, and
mirror-down drops a local manifest once the remote sweep deletes it.
