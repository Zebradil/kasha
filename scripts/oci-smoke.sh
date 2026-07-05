#!/usr/bin/env bash
set -euo pipefail

image="${1:?image required}"
cid=""
volume="kasha-oci-smoke-$$"

# shellcheck disable=SC2329 # invoked by trap
cleanup() {
	[[ -n "$cid" ]] && docker rm -f "$cid" >/dev/null 2>&1 || true
	docker volume rm "$volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker volume create "$volume" >/dev/null
cid="$(docker run -d -p 127.0.0.1::5000 -v "$volume:/kasha" "$image")"
port="$(docker port "$cid" 5000/tcp | sed 's/.*://')"

for _ in $(seq 1 100); do
	if curl --fail --silent --show-error "http://127.0.0.1:$port/nix-cache-info" >/dev/null; then
		exit 0
	fi
	sleep 0.2
done

docker logs "$cid" >&2
exit 1
