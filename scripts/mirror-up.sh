#!/usr/bin/env bash
# Mirror local root manifests from the box store up to the remote cache.
#
# Env-in -> stdout-out (ADR-0006): diff local roots/<flake>/ manifests against
# remote roots/<flake>/, copy each missing root to the remote cache, then publish
# the same root manifest upstream. Safe to re-run; overlap exits.
#
# Env:
#   KASHA_REMOTE          nix/S3 remote cache URL                  (required)
#   KASHA_FLAKE           flake id                                 (required)
#   KASHA_LOCAL_ROOTS_DIR local roots dir                          (default: /var/lib/kasha/roots)
#   KASHA_STATE_DIR       state dir                                (default: /var/lib/kasha/mirror-up)
#   KASHA_LOCAL_LIST_FILE test seam: newline local manifest keys   (optional)
#   KASHA_REMOTE_LIST_FILE test seam: newline remote manifest keys (optional)
#   KASHA_MANIFEST_DIR    test seam: local <gen>.json dir          (optional)
#   KASHA_DRY_RUN         print roots to copy, do not copy/state   (optional)
#   KASHA_COPY            test seam: copy command                  (default: nix copy --to)
#   KASHA_AWS             test seam: aws command                   (default: aws)
set -euo pipefail

remote="${KASHA_REMOTE:?KASHA_REMOTE required}"
flake="${KASHA_FLAKE:?KASHA_FLAKE required}"
local_roots_dir="${KASHA_LOCAL_ROOTS_DIR:-/var/lib/kasha/roots}"
state_dir="${KASHA_STATE_DIR:-/var/lib/kasha/mirror-up}"
aws="${KASHA_AWS:-aws}"
nix="${KASHA_NIX:-nix}"

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

list_local_roots() {
	if [[ -n "${KASHA_LOCAL_LIST_FILE:-}" ]]; then
		cp "$KASHA_LOCAL_LIST_FILE" "$1"
		return
	fi

	: >"$1"
	for manifest in "$local_roots_dir/$flake"/*.json; do
		[[ -e "$manifest" ]] || continue
		printf 'roots/%s/%s\n' "$flake" "${manifest##*/}" >>"$1"
	done
}

list_remote_roots() {
	if [[ -n "${KASHA_REMOTE_LIST_FILE:-}" ]]; then
		cp "$KASHA_REMOTE_LIST_FILE" "$1"
	else
		# s3api, not `s3 ls`: `s3 ls` exits 1 on an empty prefix, which under set -e
		# turns "nothing published yet" into a spurious failure — and the first
		# up-mirror always sees an empty prefix. list-objects-v2 exits 0 with null
		# Contents, which jq drops to an empty list.
		"$aws" "${aws_opts[@]}" s3api list-objects-v2 --bucket "$bucket" --prefix "roots/$flake/" --output json |
			jq -r '.Contents[]?.Key // empty' >"$1"
	fi
}

manifest_for() {
	local gen="$1"
	if [[ -n "${KASHA_MANIFEST_DIR:-}" ]]; then
		cat "$KASHA_MANIFEST_DIR/$gen.json"
	else
		cat "$local_roots_dir/$flake/$gen.json"
	fi
}

parse_gens() {
	local list_file="$1"
	local out_file="$2"
	local prefix="roots/$flake/"
	local key
	while IFS= read -r line; do
		key="${line##*[[:space:]]}"
		case "$key" in
		"$prefix"*.json) printf '%s\n' "${key#"$prefix"}" ;;
		esac
	done <"$list_file" | sed 's#\.json$##' | sort -u >"$out_file"
}

decide_new() {
	local local_list_file="$1"
	local remote_list_file="$2"
	local out_file="$3"
	parse_gens "$local_list_file" "$out_file.local"
	parse_gens "$remote_list_file" "$out_file.remote"
	comm -23 "$out_file.local" "$out_file.remote" >"$out_file"
}

publish_manifest() {
	local gen="$1"
	local manifest="$2"
	printf '%s\n' "$manifest" |
		"$aws" "${aws_opts[@]}" s3 cp - "s3://$bucket/roots/$flake/$gen.json" \
			--content-type application/json >/dev/null
}

if [[ -n "${KASHA_DRY_RUN:-}" ]]; then
	tmp="$(mktemp -d "${TMPDIR:-/tmp}/kasha-mirror-up.XXXXXX")"
	trap 'rm -rf "$tmp"' EXIT
	list_local_roots "$tmp/local"
	list_remote_roots "$tmp/remote"
	decide_new "$tmp/local" "$tmp/remote" "$tmp/new"
	if [[ ! -s "$tmp/new" ]]; then
		echo "kasha mirror-up: no new local roots for $flake"
		exit 0
	fi
	while IFS= read -r gen; do
		manifest="$(manifest_for "$gen")"
		jq -r '.roots[]' <<<"$manifest" | while IFS= read -r root; do
			printf '%s %s\n' "$remote" "$root"
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
	echo "kasha mirror-up: $flake already running"
	exit 0
}

touch "$seen"
cp "$seen" "$tmp.seen"
list_local_roots "$tmp.local"
list_remote_roots "$tmp.remote"
decide_new "$tmp.local" "$tmp.remote" "$tmp.new"

if [[ ! -s "$tmp.new" ]]; then
	echo "kasha mirror-up: no new local roots for $flake"
	exit 0
fi

while IFS= read -r gen; do
	manifest="$(manifest_for "$gen")"
	jq -r '.roots[]' <<<"$manifest" | while IFS= read -r root; do
		if [[ -n "${KASHA_COPY:-}" ]]; then
			# shellcheck disable=SC2086
			$KASHA_COPY "$remote" "$root"
		else
			"$nix" copy --to "$remote" "$root"
		fi
	done
	publish_manifest "$gen" "$manifest"
	printf '%s\n' "$gen" >>"$tmp.seen"
	echo "kasha mirror-up: copied $flake/$gen"
done <"$tmp.new"

sort -u "$tmp.seen" >"$tmp.next"
mv "$tmp.next" "$seen"
