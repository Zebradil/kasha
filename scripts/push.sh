#!/usr/bin/env bash
# Select a reverse-flow push target, then push paths to it.
#
# Env:
#   KASHA_REMOTE_TARGET   remote cache nix copy target, usually s3://... (required unless --to)
#   KASHA_BOX_TARGET      box nix copy target                    (default: ssh-ng://kasha-push@box)
#   KASHA_BOX_CACHE       box HTTP cache endpoint                 (default: http://box:5000)
#   KASHA_PROBE_NARINFO   .narinfo path used for reachability     (default: 00000000000000000000000000000000.narinfo)
#   KASHA_CONNECT_TIMEOUT probe timeout seconds                   (default: 2)
#   KASHA_BOX_REACHABLE   test seam: 1/0, skips network probe      (optional)
#   KASHA_PRINT_TARGET    print selected target, do not push       (optional)
set -euo pipefail

usage() {
	printf '%s\n' \
		"usage: push.sh [--to TARGET] [--print-target] [PATH ...]" \
		"" \
		"With no PATH args, non-empty stdin lines are pushed."
}

choose_target() {
	local override="$1"
	local reachable="${2:-}"

	if [[ -n "$override" ]]; then
		printf '%s\n' "$override"
		return 0
	fi

	case "$reachable" in
	1) printf '%s\n' "${KASHA_BOX_TARGET:-ssh-ng://kasha-push@box}" ;;
	0) printf '%s\n' "${KASHA_REMOTE_TARGET:?KASHA_REMOTE_TARGET required}" ;;
	*)
		echo "push: invalid reachability result: $reachable" >&2
		return 2
		;;
	esac
}

box_reachable() {
	case "${KASHA_BOX_REACHABLE:-}" in
	1 | true | yes | reachable) return 0 ;;
	0 | false | no | unreachable) return 1 ;;
	"") ;;
	*)
		echo "push: KASHA_BOX_REACHABLE must be 1 or 0" >&2
		return 2
		;;
	esac

	local cache="${KASHA_BOX_CACHE:-http://box:5000}"
	local narinfo="${KASHA_PROBE_NARINFO:-00000000000000000000000000000000.narinfo}"
	local timeout="${KASHA_CONNECT_TIMEOUT:-2}"
	local curl="${KASHA_CURL:-curl}"

	[[ "$narinfo" == *.narinfo ]] || {
		echo "push: KASHA_PROBE_NARINFO must end with .narinfo" >&2
		return 2
	}

	"$curl" --head --silent --show-error \
		--connect-timeout "$timeout" --max-time "$timeout" \
		--output /dev/null "${cache%/}/${narinfo#/}" || return 1
}

override="${KASHA_TO:-}"
print_target="${KASHA_PRINT_TARGET:-}"
paths=()

while [[ $# -gt 0 ]]; do
	case "$1" in
	--to)
		[[ $# -ge 2 ]] || {
			usage >&2
			exit 2
		}
		override="$2"
		shift 2
		;;
	--print-target)
		print_target=1
		shift
		;;
	--help)
		usage
		exit 0
		;;
	--)
		shift
		paths+=("$@")
		break
		;;
	-*)
		usage >&2
		exit 2
		;;
	*)
		paths+=("$1")
		shift
		;;
	esac
done

reachable=0
if [[ -z "$override" ]]; then
	if box_reachable; then
		reachable=1
	else
		code=$?
		[[ "$code" == 1 ]] || exit "$code"
	fi
fi

target="$(choose_target "$override" "$reachable")"

if [[ -n "$print_target" ]]; then
	printf '%s\n' "$target"
	exit 0
fi

if [[ ${#paths[@]} -eq 0 && ! -t 0 ]]; then
	while IFS= read -r path || [[ -n "$path" ]]; do
		[[ -z "$path" ]] && continue
		paths+=("$path")
	done
fi

[[ ${#paths[@]} -gt 0 ]] || {
	usage >&2
	exit 2
}

"${KASHA_NIX:-nix}" copy --to "$target" "${paths[@]}"
