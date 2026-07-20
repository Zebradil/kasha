# Handoff — fix the `mirror-down` VM test (plan-B recipe mirroring)

**Goal:** get `nix flake check` green on `main` — specifically
`checks.x86_64-linux.mirror-down`, the only failing check. Everything else in the
plan-B change (schema v2 emit, reader logic, box wiring, fixtures, shellcheck,
actionlint) is already passing.

## Context (read these, don't re-derive)

This repo's box-mirror flow was reworked so the box never builds a top-level
system derivation. Background, rationale, and the exact contract:

- PR: https://github.com/Zebradil/kasha/pull/17 (merged **as-is with this check
  red** — that is the thing to fix).
- Schema + design: `docs/adr/0003-root-manifest-indexing.md` (v2 roots are
  `{outPath, drvPath}` objects; box copies the recipe `.drv`, substitutes the
  input output-closure **minus** the top `outPath`; consumers assemble the
  top-level at deploy).
- Reader behavior + test seams: header comment of `scripts/mirror-down.sh`
  (`KASHA_COPY`, `KASHA_REALISE`, `KASHA_AWS`, `KASHA_DRY_RUN`, gen_ok /
  record-only-fully-mirrored / retry-on-partial).
- The failing test itself: `tests/mirror-down.nix`.

The companion producer lives in **znix** (`ci: build only uncached derivations`,
already on znix `main`): CI builds uncached leaves, skips the top-level, and
pushes the top-level `.drv` recipe closure. Do **not** modify znix from here.

## What's already fixed on the branch

- **Closure-export error** (`cannot export references of path '…-kasha-down-top.drv'
  because it is not in the input closure`): resolved by seeding the top `.drv`
  as a *source* input via `builtins.unsafeDiscardOutputDependency` (`topDrvDep`
  in `tests/mirror-down.nix`, commit `0aa42f2`). A bare context-stripped path
  string is not a real input and fails closure export — keep the
  `unsafeDiscardOutputDependency` seeding.

## The open failure (where the last session stopped)

The **crash-safety / fail-copy** assertion is unreliable. Intended behavior:

1. `touch /var/lib/kasha/mirror-down/fail-copy`
2. the `KASHA_COPY` stub sees the marker and `exit 42`
3. `mirror-down.sh` treats the gen as **incomplete** → does NOT mark it seen,
   does NOT realise `dep`.

Observed before the session was stopped: a full (~1147-path) `nix copy` of the
recipe closure ran **instead of** exiting 42 — the guard didn't short-circuit.
Suspected that an earlier "green" on this assertion was **masked** by the
realise step separately failing on the un-seeded build toolchain, so the
assertion was never actually proving the copy-failure path.

### What the fixer needs to verify / fix

- In the `KASHA_COPY` stub (`tests/mirror-down.nix`), the `fail-copy` marker
  check must run **before** any `nix copy`, and `exit 42` must propagate.
- In `scripts/mirror-down.sh`, a non-zero `KASHA_COPY` for a root must
  short-circuit that generation: no realise, gen stays unseen, exit non-zero
  (retry next timer). Confirm copy strictly precedes realise per root and that
  the failure isn't swallowed.
- Ensure the two independent failure sources (copy-fail vs realise-toolchain)
  can't mask each other — the toolchain realise must not run at all when copy
  failed, so the assertion isolates the copy path.

## Design constraints — do not regress

- **Box never builds a top-level** (Linux box, cross-system unsafe; no cache
  holds the top output). It substitutes the recipe's input output-closure and
  excludes the top `outPath`.
- `nix-store --query --requisites --include-outputs <drv>` drags the **whole
  build toolchain closure**, which is impractical to seed faithfully in a
  single-harmonia VM. The test deliberately uses no-`stdenv` `derivation`s plus
  the `KASHA_REALISE` stub to shrink the substitution surface to just `dep`.
  Keep that minimal surface — don't try to realise a real toolchain in-VM.
- `builtins.toJSON` collapses an attrset with an `outPath` key to its store-path
  string (Nix path coercion). The manifest in the test is built as a **raw
  string** to preserve the `{outPath, drvPath}` object shape. Keep it a raw
  string.

## How to iterate

- **Linux only** — the previous session was on darwin and could not run this VM
  test at all. Run on an `x86_64-linux` machine (or CI):
  `nix build .#checks.x86_64-linux.mirror-down -L`
- For step-through, the `nixosTest` interactive driver:
  `nix build .#checks.x86_64-linux.mirror-down.driverInteractive && ./result/bin/nixos-test-driver`
- Confirm the whole suite: `nix flake check -L` and `bash tests/run.sh`.

## Out of scope here (separate follow-ups — reference only)

- `scripts/mirror-up.sh` still reads `.roots[]` as **strings**; it mirrors
  hand-authored box-local manifests and has no v2 producer yet. Update it to the
  object schema when a v2 up-path producer exists.
- znix follow-up (in the znix repo, after this lands): bump the kasha/`ln` input,
  wire `drvPath` into znix's kasha emit step, and emit the manifest despite the
  top-level being unbuilt (it self-skips today).

## Suggested skills

- `grilling` — if the fix reopens a design question (e.g. how faithfully the VM
  test should model realise), stress-test the approach before coding.
- On entry, read this repo's `AGENTS.md`, `CONTEXT.md`, and `docs/adr/` for
  conventions (bash env-in→stdout-out per ADR-0006, single-signing-key ADR-0004,
  fixture-test style under `tests/`).
