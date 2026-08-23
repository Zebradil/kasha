# Testing the live box

End-to-end checks against the deployed box (`https://kasha.lan.zebradil.dev`, k3s
`homelab-lan/kasha`) and the remote cache (`https://znix.zebradil.dev`, Cloudflare R2). Every
snippet is copy-pasteable as written once §0 has been sourced.

What each section proves:

| § | Feature | Live writes? |
| --- | --- | --- |
| 1 | Box is serving; workers alive | no |
| 2 | Mirror-down, via the real producer path (`nix run .#cache-push` in znix) | pushes to R2 |
| 3 | Mirror-up, via an authenticated box push | pushes to box, then R2 |
| 4 | Serving + signature-gated substitution from the box | no |
| 5 | Box GC sweep | patches the deployment, may delete objects |
| 6 | Remote GC sweep | dry-run only |

Sections 1–4 are independent. Run 5 and 6 last: 5 restarts the pod, 6 fixes a workflow that
has been failing nightly.

## 0. Setup

Run once per shell. Requires `kubectl`, `gh`, `sops` (with your age key), `curl`, `python3`,
and a trusted-user nix (`trusted-users` includes you, so `--option netrc-file` is honoured).

```sh
export KASHA_URL=https://kasha.lan.zebradil.dev
export CACHE_URL=https://znix.zebradil.dev
export KUBE="kubectl --context homelab-lan --namespace kasha"
export KASHA_REPO="${KASHA_REPO:-$HOME/code/github.com/zebradil/kasha}"
export ZNIX_REPO="${ZNIX_REPO:-$HOME/code/github.com/zebradil/znix}"
export WORK="$(mktemp -d)"
```

Build the CLI once — the same binary the box runs, used here for `emit` and `gc`:

```sh
export KASHA_BIN="$(nix build --no-link --print-out-paths "$KASHA_REPO#kasha")/bin/kasha"
"$KASHA_BIN" --version
```

Two helpers used throughout — the store hash of a path, and a bulk presence check of a
manifest's closure against any binary cache:

```sh
cat > "$WORK/storehash.sh" <<'EOF'
#!/usr/bin/env bash
# /nix/store/<32-char hash>-name -> <hash>
basename "$1" | cut -c1-32
EOF
chmod +x "$WORK/storehash.sh"

cat > "$WORK/closure-present.sh" <<'EOF'
#!/usr/bin/env bash
# closure-present.sh <cache-base-url> <manifest-url>
# Prints "<present>/<total> present" and lists up to 10 missing hashes.
set -euo pipefail
base="$1"; manifest="$2"
hashes="$(curl -fsS "$manifest" | python3 -c '
import json,sys
for p in json.load(sys.stdin)["closure"]:
    print(p.split("/")[-1][:32])
')"
total="$(printf %s "$hashes" | grep -c . || true)"
missing="$(printf %s "$hashes" | xargs -P 16 -I{} sh -c \
  'curl -fsS -o /dev/null "'"$base"'/{}.narinfo" || echo {}')"
n_missing="$(printf %s "$missing" | grep -c . || true)"
echo "$((total - n_missing))/$total present in $base"
[ "$n_missing" -eq 0 ] || printf %s "$missing" | head -10
EOF
chmod +x "$WORK/closure-present.sh"
```

## 1. Health check (read-only)

```sh
curl -fsS "$KASHA_URL/status" | python3 -m json.tool
curl -fsS "$KASHA_URL/nix-cache-info"
```

`/status` reports object count, store bytes, `pending_mirror_up`, and per-flake
`last_sync` + `gaps`. A healthy box has `last_sync` within the last 5 minutes
(`KASHA_SYNC_INTERVAL=300`) and `pending_mirror_up: 0`. `gaps` counts closure paths listed in
a manifest that neither the remote cache nor the upstreams could supply — a non-zero steady
value is expected, not a failure.

The sync worker logs one line per cycle:

```sh
$KUBE logs -l app.kubernetes.io/name=kasha --tail=20 | grep synced
```

Live-follow for the rest of this document (leave it running in a second terminal):

```sh
$KUBE logs -l app.kubernetes.io/name=kasha -f --since=1m
```

## 2. Mirror-down (R2 → box)

Publishes a real generation through the producer path znix CI uses, then watches the box
discover and fetch it. `nix run .#cache-push` decrypts the signing key and R2 credentials from
`secrets/cache.yaml`, resolves the attr's build closure, signs it, `nix copy`s it to R2, and
emits the v3 manifest — the manifest is what makes it discoverable.

`checks.aarch64-darwin.lint` is the cheapest attr; substitute any other.

```sh
cd "$ZNIX_REPO"
nix run .#cache-push -- checks.aarch64-darwin.lint 2>&1 | tee "$WORK/push.log"
export GEN="$(sed -n 's/^Emitting manifest znix\/\(.*\)…$/\1/p' "$WORK/push.log" | tail -1)"
echo "generation: $GEN"
```

The manifest is now in R2 (this is a plain public read, no credentials):

```sh
curl -fsS "$CACHE_URL/roots/znix/$GEN.json" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); d["closure"]=len(d["closure"]); print(d)'
```

Within one sync cycle (≤5 min) the box discovers it. Watch for the discovery line, then the
cycle summary:

```sh
$KUBE logs -l app.kubernetes.io/name=kasha -f --since=6m \
  | grep --line-buffered -E "new remote generation|synced|mirror-down failed"
```

Expected, in order:

```
{"message":"new remote generation","flake":"znix","gen":"<GEN>"}
{"message":"synced","fetched":<n>,"gaps":0}
```

`fetched` is the number of paths pulled this cycle. Then confirm the box actually holds the
manifest and its whole closure:

```sh
curl -fsS "$KASHA_URL/roots/znix/$GEN.json" > /dev/null && echo "manifest on box"
"$WORK/closure-present.sh" "$KASHA_URL" "$KASHA_URL/roots/znix/$GEN.json"
```

A clean run prints `<n>/<n> present`; it HEADs every path, so a 5k-path closure takes about a
minute. Paths reported missing here are the same ones counted as
`gaps` in `/status` — re-probed every cycle, so re-run after another cycle before treating one
as a fault.

## 3. Mirror-up (box → R2)

Pushes a path the remote cache has never seen straight to the box, publishes its manifest
through the authenticated API (which marks the generation **local-origin**), and watches the box
carry it up to R2 unprompted.

The manifest goes out under the real `znix` flake id, with `branch=selftest` so it lands in its
own `(branch, attr)` retention group and never displaces main history. The non-main tier keeps
the newest 1 for 1 week, so the remote sweep reclaims it on its own.

Build a unique zero-dependency path (closure of exactly one):

```sh
export SELFTEST="$(nix build --no-link --print-out-paths --impure --expr '
  derivation {
    name = "kasha-selftest";
    system = builtins.currentSystem;
    builder = "/bin/sh";
    args = [ "-c" "echo kasha selftest $stamp > $out" ];
    stamp = toString builtins.currentTime;
  }')"
export SELFTEST_HASH="$("$WORK/storehash.sh" "$SELFTEST")"
echo "$SELFTEST"
```

Confirm neither cache has it — this is what makes the test meaningful:

```sh
curl -fsS -o /dev/null "$CACHE_URL/$SELFTEST_HASH.narinfo" && echo "UNEXPECTED: already in remote" || echo "not in remote (expected)"
curl -fsS -o /dev/null "$KASHA_URL/$SELFTEST_HASH.narinfo" && echo "UNEXPECTED: already on box" || echo "not on box (expected)"
```

Sign it with the znix key — the box verifies every ingested narinfo and holds no signing key
itself (ADR-0004):

```sh
sops decrypt --extract '["signing-key"]' "$ZNIX_REPO/secrets/cache.yaml" > "$WORK/signing.key"
chmod 600 "$WORK/signing.key"
nix store sign --key-file "$WORK/signing.key" --recursive "$SELFTEST"
```

Write a throwaway netrc with the box's write token and push. The token never lands in a
dotfile; `--option netrc-file` overrides Determinate's netrc for this command only:

```sh
export KASHA_TOKEN="$($KUBE get secret kasha -o jsonpath='{.data.KASHA_TOKEN}' | base64 -d)"
umask 077; printf 'machine kasha.lan.zebradil.dev login nix password %s\n' "$KASHA_TOKEN" > "$WORK/netrc"
nix copy --to "$KASHA_URL" --option netrc-file "$WORK/netrc" "$SELFTEST"
curl -fsS -o /dev/null "$KASHA_URL/$SELFTEST_HASH.narinfo" && echo "NAR ingested by box"
```

An unsigned or wrong-key push is rejected here with 400 — worth one deliberate run to see the
gate work.

Now publish the manifest through the authenticated API. `--to` on an `http(s)` target sends a
bearer-token PUT, and the box marks the generation local-origin on receipt:

```sh
export SELFTEST_GEN="selftest-$(date -u +%Y%m%d%H%M%S)"
echo "$SELFTEST" | "$KASHA_BIN" emit \
  --flake znix --gen "$SELFTEST_GEN" --branch selftest --attr kasha-selftest \
  --to "$KASHA_URL" > /dev/null
$KUBE logs -l app.kubernetes.io/name=kasha --tail=20 | grep "ingested manifest"
```

Within one cycle the box mirrors it up. Expected log lines:

```
{"message":"mirrored up","hash":"<SELFTEST_HASH>"}
{"message":"generation mirrored up","flake":"znix","gen":"<SELFTEST_GEN>"}
```

```sh
$KUBE logs -l app.kubernetes.io/name=kasha -f --since=6m \
  | grep --line-buffered -E "mirrored up|mirror-up deferred"
```

Confirm it landed in R2 — NAR and narinfo first, manifest last (the ordering GC depends on):

```sh
curl -fsS -o /dev/null "$CACHE_URL/$SELFTEST_HASH.narinfo" && echo "narinfo in remote"
curl -fsS "$CACHE_URL/roots/znix/$SELFTEST_GEN.json" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["gen"], "in remote")'
curl -fsS "$KASHA_URL/status" | python3 -c 'import json,sys; print("pending_mirror_up:", json.load(sys.stdin)["pending_mirror_up"])'
```

`pending_mirror_up` back to `0` means nothing is stuck. A non-zero value with a
`mirror-up deferred` log line means the box is holding a local-origin generation it could not
push — most often an incomplete closure (`local push incomplete (N paths missing)`), because
mirror-up refuses to publish a partial generation.

## 4. Serving and signature-gated substitution

Copy the generation's paths out of the box into a throwaway chroot store. `--from` restricts
the source to the box, so a success proves the box served it — not the remote, not
`cache.nixos.org`:

```sh
export DEST="$(mktemp -d)"
nix copy --from "$KASHA_URL" --to "$DEST" "$SELFTEST"
ls "$DEST/nix/store"
```

The copy is gated by `trusted-public-keys`; it only succeeds because `znix.zebradil.dev:…` is
trusted locally and the path carries that signature. A path with no trusted signature fails
here, which is the same gate the box applies on ingest.

Timing check against the remote for the LAN-speed claim, on a path both hold:

```sh
export BIG="$(curl -fsS "$KASHA_URL/roots/znix/$GEN.json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["closure"][0])')"
time curl -fsS -o /dev/null "$KASHA_URL/$("$WORK/storehash.sh" "$BIG").narinfo"
time curl -fsS -o /dev/null "$CACHE_URL/$("$WORK/storehash.sh" "$BIG").narinfo"
```

Off-network behaviour (box unreachable → fall back to the remote within `connect-timeout=5`)
is a client-side property of znix's substituter list; verify it by running a build off the LAN,
not from here.

## 5. Box GC

**The box has never swept.** The deployment does not set `KASHA_GC_INTERVAL`, so the interval is
the 86400s default and the first sweep is still ahead. The box stamps each sweep in
`state/last-sweep` and sleeps only the remainder of the interval since that stamp, so a restart no
longer pushes the schedule back — but there is no on-demand trigger, so exercising a sweep means
temporarily shortening the interval, which restarts the pod.

What the sweep does with defaults: keep the newest **3** manifests per `(branch, attr)` group on
`main`, **1** per non-main group, plus every unmirrored local-origin generation (the box may hold
the only copy); mark the union of the retained closures; delete every unmarked narinfo and NAR
whose **mtime is older than 24h**. It deletes no manifests — mirror-down reflects remote
retention.

> **This deletes real cache objects.** Anything only reachable from an older generation goes, and
> re-fetching it is a full re-download from R2. Consequences are bounded — nothing is lost that
> the remote cache does not still hold, and GC never touches an unmirrored local-origin
> generation — but do not run it while you need the box for a build.
>
> The restart also drops the box for a few seconds and forces an index rescan of every narinfo on
> boot.

Two things bound the blast radius. The 24h mtime grace skips objects written in the last day
regardless of retention, and a store holding no manifests is skipped outright (an empty mark set
means "not synced yet", never "retain nothing"). (The PVC was recreated recently at
the time of writing, so a sweep now would likely report `deleted=0` with a high `skipped_young` —
verify from the log line rather than assuming.)

```sh
$KUBE set env deploy/kasha KASHA_GC_INTERVAL=120
$KUBE rollout status deploy/kasha --timeout=120s
```

Wait ~2 min for the first sweep:

```sh
$KUBE logs -l app.kubernetes.io/name=kasha -f --since=5m \
  | grep --line-buffered -E "box sweep done|box sweep failed"
```

Expected:

```
{"message":"box sweep done","retained":<manifests>,"deleted":<objects>,"skipped_young":<n>}
```

`retained` is the manifest count the mark set was built from, `deleted` the objects removed,
`skipped_young` those spared by the 24h grace. Cross-check the store shrank (or did not, if
everything was young):

```sh
curl -fsS "$KASHA_URL/status" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["objects"], "objects,", round(d["store_bytes"]/1e9, 2), "GB")'
```

Restore the default and let the pod settle:

```sh
$KUBE set env deploy/kasha KASHA_GC_INTERVAL-
$KUBE rollout status deploy/kasha --timeout=120s
```

## 6. Remote GC

### 6a. Local dry-run

Reads the bucket and reports what a sweep would delete, deleting nothing. Safe with the
read/write credentials in znix's sops file:

```sh
cd "$ZNIX_REPO"
export KASHA_REMOTE="$(sops decrypt --extract '["cache-s3-url"]' secrets/cache.yaml)"
export AWS_ACCESS_KEY_ID="$(sops decrypt --extract '["aws-access-key-id"]' secrets/cache.yaml)"
export AWS_SECRET_ACCESS_KEY="$(sops decrypt --extract '["aws-secret-access-key"]' secrets/cache.yaml)"
"$KASHA_BIN" gc --dry-run | tee "$WORK/gc-dry.log" | tail -20
```

Every line is `would delete <key>`, closing with:

```
retained <n> manifests, <m> objects to delete (dry run), <k> young skipped
```

Sanity-check the retention decision before anyone runs this for real — that the survivors are
the newest 5 per main `(branch, attr)` group plus anything under 4 weeks old:

```sh
grep -c '^would delete ' "$WORK/gc-dry.log"
grep '^would delete roots/' "$WORK/gc-dry.log" || echo "(no manifests would be pruned)"
```

Retention can be widened or narrowed for a what-if run without touching the defaults:

```sh
"$KASHA_BIN" gc --dry-run --main-keep 10 --main-age-weeks 8 | tail -3
```

### 6b. Fix and dispatch the scheduled sweep

`.github/workflows/gc.yml` runs nightly at 04:00 UTC and **has been failing on every run**:

```
Error: remote must be s3://…, got
```

The repo has no `KASHA_GC_ACCESS_KEY_ID` / `KASHA_GC_SECRET_ACCESS_KEY` secrets and no
`KASHA_REMOTE` variable, so no remote sweep has ever run and R2 grows without bound.

The workflow deliberately wants credentials the box never holds: **delete-capable**. Mint a
separate R2 API token in the Cloudflare dashboard with object read/write **and delete** on the
`znix` bucket — do not reuse the box's read/write pair. Then:

```sh
cd "$KASHA_REPO"
gh variable set KASHA_REMOTE --body "$(sops decrypt --extract '["cache-s3-url"]' "$ZNIX_REPO/secrets/cache.yaml")"
gh secret set KASHA_GC_ACCESS_KEY_ID       # paste the new token's access key id
gh secret set KASHA_GC_SECRET_ACCESS_KEY   # paste the new token's secret
gh variable list && gh secret list
```

Dispatch a dry run and read the same report from CI:

```sh
gh workflow run gc.yml -f dry-run=true
sleep 5 && gh run watch "$(gh run list --workflow=gc.yml -L1 --json databaseId --jq '.[0].databaseId')"
gh run view "$(gh run list --workflow=gc.yml -L1 --json databaseId --jq '.[0].databaseId')" --log | grep -E "would delete|retained .* manifests"
```

Only once that report looks right should the nightly schedule be allowed to delete — it does so
by default (`dry-run` is false on the schedule), so a wrong retention setting is felt on the next
night's run.

## 7. Cleanup

```sh
rm -rf "$WORK" "$DEST"
```

The selftest generation from §3 needs no manual cleanup: `branch=selftest` puts it in the
non-main tier (keep newest 1, 1 week), so the first remote sweep past a week reclaims it, and the
box drops its objects on any sweep where a newer selftest generation exists.

Deleting it earlier requires delete-capable credentials (§6b) and removing
`roots/znix/<gen>.json` from R2 — the manifest *is* the retention decision, so the objects follow
on the next sweep.
