#!/usr/bin/env bash
# Mirror new root manifests from the remote cache down into the box store.
#
# Env-in -> stdout-out (ADR-0006): LIST remote roots/<flake>/, diff against a
# flat last-seen gen set, copy each new root with nix (closure expansion is nix's
# job), then atomically publish the new seen set. Safe to re-run; overlap exits.
#
# Env:
#   KASHA_REMOTE      nix/S3 remote cache URL                  (required)
#   KASHA_FLAKE       flake id                                 (required)
#   KASHA_STATE_DIR   state dir                                (default: /var/lib/kasha/mirror-down)
#   KASHA_LIST_FILE   test seam: newline manifest keys/URLs    (optional)
#   KASHA_MANIFEST_DIR test seam: local <gen>.json dir         (optional)
#   KASHA_SEEN_FILE   test seam for dry-run decision tests     (optional)
#   KASHA_DRY_RUN     print roots to copy, do not copy/state   (optional)
#   KASHA_COPY        test seam: copy command                  (default: nix copy --from)
#   KASHA_AWS         test seam: aws command                   (default: aws)
set -euo pipefail

remote="${KASHA_REMOTE:?KASHA_REMOTE required}"
flake="${KASHA_FLAKE:?KASHA_FLAKE required}"
state_dir="${KASHA_STATE_DIR:-/var/lib/kasha/mirror-down}"
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

list_roots() {
	if [[ -n "${KASHA_LIST_FILE:-}" ]]; then
		cp "$KASHA_LIST_FILE" "$1"
	else
		"$aws" "${aws_opts[@]}" s3 ls --recursive "s3://$bucket/roots/$flake/" >"$1"
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
	sed -n "s#.*\(roots/$flake/[^[:space:]]*\.json\).*#\1#p" "$list_file" |
		sed "s#^roots/$flake/##; s#\.json\$##" |
		sort -u >"$out_file.remote"
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

while IFS= read -r gen; do
	manifest="$(manifest_for "$gen")"
	jq -r '.roots[]' <<<"$manifest" | while IFS= read -r root; do
		if [[ -n "${KASHA_COPY:-}" ]]; then
			# shellcheck disable=SC2086
			$KASHA_COPY "$remote" "$root"
		else
			"$nix" copy --from "$remote" "$root"
		fi
	done
	printf '%s\n' "$gen" >>"$tmp.seen"
	echo "kasha mirror-down: copied $flake/$gen"
done <"$tmp.new"

sort -u "$tmp.seen" >"$tmp.next"
mv "$tmp.next" "$seen"
