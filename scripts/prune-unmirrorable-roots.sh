#!/usr/bin/env bash
# Prune root manifests the recipe-based mirror-down can never process, so the box
# stops retrying (or, once fixed there, abandoning) them every timer. Two classes:
#
#   1. Malformed/legacy root: not an object, or a null/empty drvPath — mirror-down
#      would emit `flake:null`.
#   2. Recipe absent from the remote: a root whose .drv narinfo is not in the
#      bucket (legacy manifest whose recipe was never pushed / was GC'd) — the
#      `nix copy --from` would fail with "path is not valid" forever.
#
# LIST roots/<flake>/, fetch each manifest, delete only the flagged ones.
#
# Dry-run by default: prints what it WOULD delete and changes nothing. Pass
# --force to actually remove the flagged manifests.
#
# Env:
#   KASHA_REMOTE   s3://bucket?endpoint=...&region=...   (required)
#   KASHA_FLAKE    flake id                              (required)
#   KASHA_AWS      test seam: aws command                (default: aws)
set -euo pipefail

force=0
[[ "${1:-}" == "--force" ]] && force=1

remote="${KASHA_REMOTE:?KASHA_REMOTE required}"
flake="${KASHA_FLAKE:?KASHA_FLAKE required}"
aws="${KASHA_AWS:-aws}"

# Parse the nix-style S3 target into aws flags (same shape as mirror-down.sh).
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

# True if the manifest can never be mirrored by the recipe-based flow.
unmirrorable() {
  local manifest="$1"
  # Class 1: not an object, or a null/empty drvPath.
  if jq -e 'any(.roots[]; type != "object" or .drvPath == null or .drvPath == "")' <<<"$manifest" >/dev/null; then
    return 0
  fi
  # Class 2: a root whose recipe narinfo is absent from the bucket. nix S3 caches
  # key each store path as <hash>.narinfo at the bucket root (this remote is
  # bucket-only), and the hash is the drvPath basename up to the first '-'.
  local drvPath hash
  while IFS= read -r drvPath; do
    hash="${drvPath##*/}"
    hash="${hash%%-*}"
    "$aws" "${aws_opts[@]}" s3api head-object --bucket "$bucket" --key "$hash.narinfo" >/dev/null 2>&1 && continue
    return 0
  done < <(jq -r '.roots[].drvPath' <<<"$manifest")
  return 1
}

[[ "$force" == 1 ]] || echo "prune-unmirrorable-roots: DRY RUN (pass --force to delete)" >&2

"$aws" "${aws_opts[@]}" s3api list-objects-v2 --bucket "$bucket" \
  --prefix "roots/$flake/" --query 'Contents[].Key' --output text \
  | tr '\t' '\n' \
  | while IFS= read -r key; do
      [[ -z "$key" ]] && continue
      manifest="$("$aws" "${aws_opts[@]}" s3 cp "s3://$bucket/$key" -)"
      unmirrorable "$manifest" || continue
      if [[ "$force" == 1 ]]; then
        "$aws" "${aws_opts[@]}" s3 rm "s3://$bucket/$key"
      else
        echo "would delete: $key"
      fi
    done
