#!/usr/bin/env bash
# Emit and publish a generation's root manifest (ADR-0003, ADR-0006).
#
# Given a generation's already-built, store-valid top-level output paths (stdin,
# one per line), emit roots/<flake>/<gen>.json describing those roots ONLY —
# never their closures. A reader expands a root into its full closure via
# `nix copy`'s own closure-awareness, so manifests never enumerate closures.
#
# Interface (ADR-0006 env-in -> stdout-out): the manifest JSON is written to
# stdout. When KASHA_TARGET is set the same bytes are published to
# <target>/roots/<flake>/<gen>.json via `aws s3 cp`, which speaks any
# S3-compatible endpoint (not R2-specific) — landing the manifest alongside the
# NARs `nix copy` wrote to the same bucket.
#
# Env:
#   KASHA_FLAKE      flake id                              (required)
#   KASHA_GEN        gen-id                                (required)
#   KASHA_TIMESTAMP  ISO-8601 UTC stamp                    (optional; default: now)
#   KASHA_TARGET     s3://bucket?endpoint=...&region=...   (optional; publish)
# Stdin: store paths, one per line (pre-resolved, store-valid; not re-built).
set -euo pipefail

flake="${KASHA_FLAKE:?KASHA_FLAKE required}"
gen="${KASHA_GEN:?KASHA_GEN required}"
timestamp="${KASHA_TIMESTAMP:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

# ponytail: trust the caller's paths are already built and store-valid (issue
# precondition); we never build or expand closures — roots only (ADR-0003).
manifest="$(jq -Rn \
	--arg flake "$flake" \
	--arg gen "$gen" \
	--arg timestamp "$timestamp" \
	'{flake: $flake, gen: $gen, timestamp: $timestamp,
	  roots: [inputs | select(length > 0)] | unique}')"

printf '%s\n' "$manifest"

[[ -z "${KASHA_TARGET:-}" ]] && exit 0

# Publish alongside the NARs. Parse the nix copy-style S3 target into aws flags
# so any S3-compatible endpoint works, not just R2.
# ponytail: URL->aws-flags parsing is arguably the future push-target-selection
# tool's job (ADR-0006). Inline here while that tool doesn't exist; extract when
# it lands or a third param (profile=, secret-key=) needs shared handling.
target="${KASHA_TARGET#s3://}"
bucket="${target%%\?*}"
query=""
[[ "$target" == *\?* ]] && query="${target#*\?}"

aws_opts=()
IFS='&' read -ra params <<<"$query"
for p in "${params[@]}"; do
	case "$p" in
	endpoint=*)
		endpoint="${p#endpoint=}"
		# nix S3 targets often give a scheme-less host; aws needs a URL.
		[[ "$endpoint" == *://* ]] || endpoint="https://$endpoint"
		aws_opts+=(--endpoint-url "$endpoint")
		;;
	region=*) aws_opts+=(--region "${p#region=}") ;;
	esac
done

printf '%s\n' "$manifest" |
	aws "${aws_opts[@]}" s3 cp - "s3://$bucket/roots/$flake/$gen.json" \
		--content-type application/json >/dev/null
