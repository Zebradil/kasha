#!/usr/bin/env bash
# Mirror new root manifests from the remote cache down into the box store.
#
# Env-in -> stdout-out (ADR-0006): LIST remote roots/<flake>/, diff against a
# flat last-seen gen set, mirror each new generation, then atomically publish the
# new seen set. Safe to re-run; overlap exits.
#
# Mirroring a root (a {outPath, drvPath} object, manifest version 2) never builds
# a top-level — the box is Linux and cannot assemble a cross-system top output,
# and no cache holds that output anyway (CI ships only the recipe). Instead:
#   1. copy the recipe: `nix copy --from remote <drvPath>` (only the remote holds
#      the .drv requisite closure; upstream caches store outputs, not .drv files),
#   2. realise the recipe's input derivations, which substitutes their (all cached)
#      output closures from the box's configured substituters (remote + upstream)
#      without building. The top-level output is never realised. Each consumer
#      assembles its own top-level at deploy time.
#
# Env:
#   KASHA_REMOTE      nix/S3 remote cache URL                  (required)
#   KASHA_FLAKE       flake id                                 (required)
#   KASHA_STATE_DIR   state dir                                (default: /var/lib/kasha/mirror-down)
#   KASHA_LIST_FILE   test seam: newline manifest keys/URLs    (optional)
#   KASHA_MANIFEST_DIR test seam: local <gen>.json dir         (optional)
#   KASHA_SEEN_FILE   test seam for dry-run decision tests     (optional)
#   KASHA_DRY_RUN     print planned recipe copies, no state    (optional)
#   KASHA_COPY        test seam: recipe copy (default: nix copy --from)
#   KASHA_REALISE     test seam: realise command (default: nix-store --realise)
#   KASHA_AWS         test seam: aws command                   (default: aws)
set -euo pipefail

remote="${KASHA_REMOTE:?KASHA_REMOTE required}"
flake="${KASHA_FLAKE:?KASHA_FLAKE required}"
state_dir="${KASHA_STATE_DIR:-/var/lib/kasha/mirror-down}"
aws="${KASHA_AWS:-aws}"
nix="${KASHA_NIX:-nix}"
realise="${KASHA_REALISE:-nix-store --realise}"

target="${remote#s3://}"
bucket="${target%%\?*}"
query=""
[[ "$target" == *\?* ]] && query="${target#*\?}"
aws_opts=()
IFS='&' read -ra params <<<"$query"
for p in "${params[@]}"; do
  case "$p" in
  endpoint=*)
    endpoint="${p#endpoint=}"
    [[ "$endpoint" == *://* ]] || endpoint="https://$endpoint"
    aws_opts+=(--endpoint-url "$endpoint")
    ;;
  region=*) aws_opts+=(--region "${p#region=}") ;;
  esac
done

list_roots() {
  if [[ -n "${KASHA_LIST_FILE:-}" ]]; then
    cp "$KASHA_LIST_FILE" "$1"
  else
    # s3api, not `s3 ls`: `s3 ls` exits 1 on an empty prefix, which under set -e
    # turns "no roots published yet" into a spurious failure. list-objects-v2
    # exits 0 and yields null Contents, which jq drops to an empty list.
    "$aws" "${aws_opts[@]}" s3api list-objects-v2 --bucket "$bucket" --prefix "roots/$flake/" --output json \
      | jq -r '.Contents[]?.Key // empty' >"$1"
  fi
}

manifest_for() {
  local gen="$1"
  if [[ -n "${KASHA_MANIFEST_DIR:-}" ]]; then
    cat "$KASHA_MANIFEST_DIR/$gen.json"
  else
    "$aws" "${aws_opts[@]}" s3 cp "s3://$bucket/roots/$flake/$gen.json" -
  fi
}

decide_new() {
  local list_file="$1"
  local seen_file="$2"
  local out_file="$3"
  local prefix="roots/$flake/"
  while IFS= read -r line; do
    key="${line##*[[:space:]]}"
    case "$key" in
    "$prefix"*.json) printf '%s\n' "${key#"$prefix"}" ;;
    esac
  done <"$list_file" | sed 's#\.json$##' | sort -u >"$out_file.remote"
  sort -u "$seen_file" >"$out_file.seen"
  comm -23 "$out_file.remote" "$out_file.seen" >"$out_file"
}

if [[ -n "${KASHA_DRY_RUN:-}" ]]; then
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/kasha-mirror-down.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  seen="${KASHA_SEEN_FILE:-/dev/null}"
  list_roots "$tmp/list"
  decide_new "$tmp/list" "$seen" "$tmp/new"
  if [[ ! -s "$tmp/new" ]]; then
    echo "kasha mirror-down: no new roots for $flake"
    exit 0
  fi
  while IFS= read -r gen; do
    manifest="$(manifest_for "$gen")"
    jq -r '.roots[].drvPath' <<<"$manifest" | while IFS= read -r drvPath; do
      printf '%s %s\n' "$remote" "$drvPath"
    done
  done <"$tmp/new"
  exit 0
fi

mkdir -p "$state_dir"
seen="$state_dir/$flake.seen"
lock="$state_dir/$flake.lock"
tmp="$(mktemp "$state_dir/$flake.XXXXXX")"
trap 'rm -f "$tmp"*' EXIT

# Overlap safety: another run is already making progress; next timer can retry.
exec 9>"$lock"
flock -n 9 || {
  echo "kasha mirror-down: $flake already running"
  exit 0
}

touch "$seen"
cp "$seen" "$tmp.seen"
list_roots "$tmp.list"
decide_new "$tmp.list" "$seen" "$tmp.new"

if [[ ! -s "$tmp.new" ]]; then
  echo "kasha mirror-down: no new roots for $flake"
  exit 0
fi

# A single unresolvable root (e.g. an upstream path whose only signature isn't
# trusted here) must not abort the whole run: mirror every root, record only gens
# that fully mirrored so the rest retry next timer, and exit non-zero so the miss
# stays visible. Process-substitution (not a pipe) so gen_ok survives the loop.
#
# Permanent misses are the exception: a root with no recipe on the remote (null
# drvPath, or a .drv the remote never held / GC'd) can never mirror, so retrying
# it every timer spins forever. Such a gen is recorded as seen (abandoned) so the
# churn stops; prune its manifest from the bucket with prune-unmirrorable-roots.sh.
failed=0
while IFS= read -r gen; do
  manifest="$(manifest_for "$gen")"
  gen_ok=1
  gen_permanent=0
  while IFS= read -r drvPath; do
    # A null/absent drvPath is a legacy or malformed root with no recipe to copy
    # — `jq -r` renders JSON null as the string "null", which nix would read as
    # an installable (`flake:null`). Unmirrorable for good: abandon the gen.
    if [[ -z "$drvPath" || "$drvPath" == null ]]; then
      echo "kasha mirror-down: $flake/$gen root has no drvPath, skipping" >&2
      gen_ok=0
      gen_permanent=1
      continue
    fi
    # 1. Copy the recipe from the remote — only it holds the .drv closure.
    #    Capture output so a definitive "path is not valid" (the remote never
    #    held the recipe) is told apart from a transient copy failure.
    if [[ -n "${KASHA_COPY:-}" ]]; then
      # shellcheck disable=SC2086
      copy_out="$($KASHA_COPY "$remote" "$drvPath" 2>&1)" && copy_rc=0 || copy_rc=$?
    else
      copy_out="$("$nix" copy --from "$remote" "$drvPath" 2>&1)" && copy_rc=0 || copy_rc=$?
    fi
    if [[ "$copy_rc" != 0 ]]; then
      printf '%s\n' "$copy_out" >&2
      gen_ok=0
      # ponytail: substring-match nix's classic "path '…' is not valid" — the
      # signal that the remote genuinely lacks the recipe (legacy manifest,
      # recipe never pushed / GC'd), so this gen can never mirror. Swap for a
      # `nix path-info --store "$remote" "$drvPath"` probe if the wording drifts.
      case "$copy_out" in
      *"is not valid"*) gen_permanent=1 ;;
      esac
      continue
    fi
    # 2. Realise the recipe's input derivations. Their outputs are all cached,
    #    so this substitutes the input output-closure (never builds), and the
    #    top-level output is never realised — it is in no cache and is
    #    cross-system unbuildable here; the consumer assembles it at deploy.
    #    (--include-outputs is not usable: it lists only already-valid outputs,
    #    and on a fresh box nothing is built yet.)
    if ! refs="$(nix-store --query --references "$drvPath")"; then
      gen_ok=0
      continue
    fi
    inputs="$(printf '%s\n' "$refs" | grep '\.drv$' || true)"
    [[ -z "$inputs" ]] && continue
    # shellcheck disable=SC2086
    $realise $inputs || gen_ok=0
  done < <(jq -r '.roots[].drvPath' <<<"$manifest")
  if [[ "$gen_ok" == 1 ]]; then
    printf '%s\n' "$gen" >>"$tmp.seen"
    echo "kasha mirror-down: copied $flake/$gen"
  elif [[ "$gen_permanent" == 1 ]]; then
    # Record as seen to stop retrying an unmirrorable gen every timer. Loud on
    # stderr, but not a run failure — nothing here will ever succeed. Prune the
    # manifest from the bucket with prune-unmirrorable-roots.sh.
    printf '%s\n' "$gen" >>"$tmp.seen"
    echo "kasha mirror-down: $flake/$gen unmirrorable, abandoning (recipe absent from remote)" >&2
  else
    failed=1
    echo "kasha mirror-down: $flake/$gen incomplete, will retry" >&2
  fi
done <"$tmp.new"

sort -u "$tmp.seen" >"$tmp.next"
mv "$tmp.next" "$seen"

[[ "$failed" == 0 ]] || exit 1
