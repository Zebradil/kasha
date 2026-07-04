# Handoff — issue #2 (box read path) — Linux test + merge

Branch: `feat/box-read-path` (commit `2a2aa28`). Built and verified on macOS
(darwin) except the NixOS VM smoke test, which needs Linux + KVM. This doc is
the checklist to finish on a Linux machine.

## What was built

- `modules/box.nix` — `services.kasha-box`: harmonia serves the nix store as a
  signed binary cache over HTTP; `signKeyPaths = []` (no key on box, ADR-0004);
  firewall opens the port.
- `scripts/check-store-fs.sh` — NFS guard (ADR-0002): refuses start on an
  NFS-backed store. Ordered before harmonia, required by it.
- `tests/run.sh` + `tests/fixtures/` — ADR-0006 env-in→stdout-out fixture runner
  (no bats).
- `tests/smoke.nix` — Linux NixOS VM: seed → sign → serve → substitute → verify,
  incl. wrong-key rejection; asserts box loads no sign-key credential.
- `flake.nix` — dev shell + `checks` (shellcheck, actionlint, fixtures; `smoke`
  added only on `*-linux`).
- `.github/workflows/ci.yml` — `nix flake check` on push/PR.

## Already verified on darwin

- `bash tests/run.sh` → 3 fixtures pass.
- `nix flake check` → shellcheck, actionlint, fixtures green.
- Full NixOS config evaluates; guard package builds and behaves (nfs → exit 1).
- `checks.x86_64-linux.smoke` **evaluates** (drv builds a plan) but was **not
  run** — no Linux/KVM on the build host.

## To do on Linux

1. Check out the branch:
   ```
   git fetch && git checkout feat/box-read-path
   ```
2. Confirm KVM is available (`ls -l /dev/kvm`). NixOS VM tests need it.
3. Run the full check incl. the smoke test:
   ```
   nix flake check -L
   ```
   Or just the round-trip:
   ```
   nix build -L .#checks.x86_64-linux.smoke   # (or aarch64-linux)
   ```
   Expected: seed→serve→substitute→verify passes, wrong-key attempt fails, box
   holds no sign-key credential.

### If smoke fails

- `harmonia.socket` unit name / socket activation: confirm against the pinned
  nixpkgs (`flake.lock`). The module in that revision defines
  `systemd.sockets.harmonia`; if a future bump drops it, wait on
  `harmonia.service` instead in `tests/smoke.nix`.
- `nix store sign` / `nix copy` need `experimental-features = nix-command`
  (already set on both test nodes).

## Merge + close

Once smoke is green on Linux:

```
gh pr create --base main --head feat/box-read-path \
  --title "Box read path: serve signed store over HTTP (+ repo skeleton)" \
  --body "Closes #2"
# after CI green:
gh pr merge --squash --delete-branch
```

`Closes #2` in the PR body auto-closes the issue on merge. Issue #2 unblocks
#4, #5, #6.

Delete this file after merge (`git rm HANDOFF.md`) — it's transfer scaffolding,
not repo docs.
