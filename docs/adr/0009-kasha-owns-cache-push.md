# kasha owns the producer-side cache-push script

ADR-0008 made the box one Rust binary and left the *producer* side — resolve a build
closure, sign it, `nix copy` it to the remote cache, emit the manifest — in each
producer repo's CI. znix carried it as `.github/scripts/populate-nix-cache.sh`.

That script and `kasha emit` are two halves of one contract: the manifest lists exactly
the closure the push uploaded, and its gen-id and `branch` field decide which retention
tier the box and the remote GC apply. Splitting them across repos meant the format's
owner and its only writer could be bumped independently.

kasha now ships the script as `packages.kasha-cache-push` (`scripts/cache-push.sh`), a
`writeShellApplication` whose binary carries the same name. Producers already pin kasha
as a flake input for the emitter; the push logic now arrives through that same pin, so a
fix propagates on a `flake.lock` bump and the two halves cannot skew.

Still bash, not a `kasha push` subcommand: the script is control flow around `nix`,
`nix-store` and `nix copy` invocations that a Rust process would only shell out to.
`writeShellApplication` runs `shellcheck` at build time and
`checks.kasha-cache-push` puts that in `nix flake check`.

`KASHA_BIN` is deliberately not baked into the wrapper. Producers pass their own
emitter path, and hard-wiring one would make this script depend on the Rust build —
which CI publishes *using* this script.

**Tripwire for revisiting:** if the script starts parsing structured data back (reading
manifests, per-path state, retry bookkeeping) rather than emitting it, that is the
signal to lift it into the binary — the same tripwire ADR-0006 set and ADR-0008 fired.
