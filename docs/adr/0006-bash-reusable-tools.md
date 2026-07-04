# Reusable tools are bash (latest), not a compiled/scripting language

The three reusable tools — root-manifest emission, push-target selection, and the
mirror-diff engine — are written in **bash** (latest, `#!/usr/bin/env bash` with
`set -euo pipefail`, `shellcheck` in bash mode), not Go/Rust/Python. This matches the
znix prior art being extracted (`probe-cache.sh`, `populate-nix-cache.sh`,
`cache-push-local.sh`), keeps the whole product one language under the existing
`shellcheck` + `actionlint` gate, and adds no build toolchain to a Nix-packaged
deployable. The env-in→stdout-out fixture test pattern (no bats) applies uniformly.

Push-target selection and manifest emission are trivially shell — control flow plus one
`jq -n` to build a JSON object the script never parses back. The only tool where a
"real" language was genuinely considered is the mirror-diff (issues #6/#8): it reads
structured data. It survives in bash because the diff is a **set-difference over path
strings**, not data modeling — manifests are roots-only (ADR-0003), "last-seen state" is
a flat set of already-pulled gen-ids, and the decision is `comm`/`sort -u`/`jq` set math.
Closure expansion is nix's job, not ours; crash-safety is "re-run is a no-op" (idempotent
LIST+diff), not a state machine.

**Tripwire for revisiting:** if last-seen state stops being a flat set of gen-ids and
starts carrying per-root structured status (retry counts, partial-pull tracking, a real
state machine), that is the signal to lift the diff engine into a typed language. It
does not in the MVP. Scope: MVP only — a post-MVP tool (e.g. GC mark-sweep) may make its
own choice.
