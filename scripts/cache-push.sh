#!/usr/bin/env bash
set -euo pipefail

# Resolve Nix store paths, sign them, and push them to an S3 binary cache.
#
# The single source of the resolve -> sign -> push logic for every kasha cache
# producer. Shipped as `packages.cache-push` so consumers pin it through the
# kasha flake input alongside the `kasha emit` binary it drives — the manifest
# format and the push that produces it cannot drift apart.
#
# Configuration is read from the environment (each step is skipped when its
# inputs are empty, so a key-less or credential-less run is a no-op for that
# step):
#   CACHE_S3_URL              s3:// URL for `nix copy --to`     (skip push when empty)
#   CACHE_SIGNING_KEY_FILE    key file for `nix store sign`     (skip signing when empty)
#   AWS_ACCESS_KEY_ID
#   AWS_SECRET_ACCESS_KEY     S3 credentials                    (skip push when empty)
#
# kasha generation-manifest emission runs after a successful push, and only
# when both of the first two are set:
#   KASHA_FLAKE               flake id the manifest is filed under (roots/<flake>/)
#   KASHA_BIN                 path to the `kasha` binary that emits the manifest
#   KASHA_GEN                 generation id  ┐
#   KASHA_BRANCH              retention tier ├ --paths-file mode only; derived
#   KASHA_ATTR                retention group┘ from git + the attr otherwise
#
# Store paths come from exactly one source:
#   --paths-file FILE   newline-separated, pre-resolved store paths (the CI path)
#   ATTR...             flake attrs to resolve, e.g. checks.aarch64-darwin.foo-build
#   (no args)           every output of `.#checks.<current-system>`

paths_file=""
attrs=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --paths-file) paths_file="$2"; shift 2 ;;
    --paths-file=*) paths_file="${1#*=}"; shift ;;
    --) shift; attrs+=("$@"); break ;;
    *) attrs+=("$1"); shift ;;
  esac
done

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
paths="$workdir/paths.txt"

if [[ -n "$paths_file" ]]; then
  # CI already resolved the valid paths; just normalise.
  sed '/^[[:space:]]*$/d' "$paths_file" | sort -u > "$paths"
else
  if [[ ${#attrs[@]} -eq 0 ]]; then
    system="$(nix eval --raw --impure --expr builtins.currentSystem)"
    echo "No attrs given; resolving every checks.${system}.* output."
    # Plain read loop (not mapfile) so the script also runs under the bash 3.2
    # that ships with macOS, for callers that run the file directly instead of
    # the nixpkgs-bash `cache-push` wrapper.
    while IFS= read -r n; do
      [[ -n "$n" ]] && attrs+=("checks.${system}.${n}")
    done < <(
      nix eval --raw ".#checks.${system}" \
        --apply 'cs: builtins.concatStringsSep "\n" (builtins.attrNames cs)'
    )
  fi
  candidates="$workdir/candidates.txt"
  : > "$candidates"
  attr_index=0
  for attr in "${attrs[@]}"; do
    attr_index=$((attr_index + 1))
    # Per-attr closure kept in its own file: a v3 manifest lists exactly the
    # paths that attr contributed, so it cannot be recovered from the merged
    # candidate list later. Indexed by position — attr names are not filenames.
    attr_paths="$workdir/attr-${attr_index}.txt"
    : > "$attr_paths"
    echo "Resolving store paths for .#${attr}"
    # Instantiate the derivation without building, then take its build closure
    # (outputs included, .drv files excluded).
    drv="$(nix path-info --derivation ".#${attr}" 2>/dev/null || true)"
    if [[ -z "$drv" ]]; then
      echo "::warning::could not resolve a derivation for '${attr}'" >&2
      continue
    fi
    nix-store --query --requisites --include-outputs "$drv" \
      | { grep -v '\.drv$' || true; } >> "$attr_paths"
    # Also publish the .drv recipe closure (text, KB-scale). The box is a NAR
    # mirror and never realizes anything; the recipe is what lets the consuming
    # host realize the top-level output from the pushed inputs.
    nix-store --query --requisites "$drv" >> "$attr_paths"
    sort -u -o "$attr_paths" "$attr_paths"
    cat "$attr_paths" >> "$candidates"
  done
  sort -u -o "$candidates" "$candidates"
  # Keep only paths valid in the store, so a partial build still contributes its
  # finished outputs. --print-invalid lists the missing ones in a single call;
  # subtract them from the candidates.
  invalid="$workdir/invalid.txt"
  xargs -r nix-store --check-validity --print-invalid < "$candidates" 2>/dev/null \
    | sort -u > "$invalid"
  comm -23 "$candidates" "$invalid" > "$paths"
fi

if [[ ! -s "$paths" ]]; then
  echo "No valid store paths to publish."
  exit 0
fi

store_paths=()
while IFS= read -r p; do
  [[ -n "$p" ]] && store_paths+=("$p")
done < "$paths"
echo "Resolved ${#store_paths[@]} store path(s)."

if [[ -n "${CACHE_SIGNING_KEY_FILE:-}" ]]; then
  echo "Signing ${#store_paths[@]} path(s)…"
  nix store sign --key-file "$CACHE_SIGNING_KEY_FILE" --recursive "${store_paths[@]}"
else
  echo "CACHE_SIGNING_KEY_FILE not set — skipping signing."
fi

if [[ -z "${CACHE_S3_URL:-}" ]]; then
  echo "CACHE_S3_URL not set — skipping remote push."
elif [[ "$CACHE_S3_URL" == http://* || "$CACHE_S3_URL" == https://* ]]; then
  # An http(s):// binary cache is read-only; `nix copy --to` cannot upload to it
  # (Determinate Nix segfaults instead of erroring). The push target must be a
  # writable store such as s3://bucket?region=…&endpoint=… — this is usually the
  # value that lived in the old CI CACHE_URL secret, NOT the https pull URL.
  echo "::error::CACHE_S3_URL is '${CACHE_S3_URL}', which looks like a read-only pull URL." >&2
  echo "::error::Set cache-s3-url to the writable s3:// push target (e.g. s3://bucket?region=…&endpoint=…)." >&2
  exit 1
elif [[ -z "${AWS_ACCESS_KEY_ID:-}" || -z "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
  echo "::warning::Remote cache push skipped — AWS credentials are not set." >&2
else
  echo "Pushing ${#store_paths[@]} path(s) to ${CACHE_S3_URL}…"
  nix copy --to "$CACHE_S3_URL" "${store_paths[@]}"
  pushed=1
fi

# ── Publish kasha generation manifests ─────────────────────────────────────
# Only after a successful push, and only when configured (KASHA_FLAKE +
# KASHA_BIN). A NAR in the remote cache without a roots/<flake>/<gen>.json is
# invisible to the box's mirror-down, so the push would be wasted. A v3
# manifest lists the full closure that was pushed — the box mirrors exactly
# those paths, it never expands or realizes anything itself.
if [[ "${pushed:-}" == 1 && -n "${KASHA_FLAKE:-}" && -n "${KASHA_BIN:-}" ]]; then
  sanitize() { printf '%s' "$1" | tr -c 'A-Za-z0-9_.-' '-'; }
  emit_manifest() { # $1=gen id, $2=branch, $3=attr; stdin=closure paths
    "$KASHA_BIN" emit --flake "$KASHA_FLAKE" --gen "$1" --branch "$2" --attr "$3" \
      --to "$CACHE_S3_URL" > /dev/null
  }

  if [[ -n "$paths_file" ]]; then
    # --paths-file mode (CI): the caller owns the provenance fields; the pushed
    # path list is the closure.
    if [[ -n "${KASHA_GEN:-}" && -n "${KASHA_BRANCH:-}" && -n "${KASHA_ATTR:-}" ]]; then
      echo "Emitting manifest ${KASHA_FLAKE}/${KASHA_GEN}…"
      emit_manifest "$KASHA_GEN" "$KASHA_BRANCH" "$KASHA_ATTR" < "$paths"
    else
      echo "::warning::kasha emit skipped — KASHA_GEN, KASHA_BRANCH and KASHA_ATTR are required in --paths-file mode." >&2
    fi
  else
    # attrs mode (local): one manifest per attr, keyed by commit provenance.
    # Idempotent on re-run; ref + the manifest's embedded timestamp are the GC
    # handles. A dirty worktree is non-reproducible, so it is marked as such.
    ref="$(sanitize "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)")"
    sha="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    dirty=""
    git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null || dirty="-dirty"
    attr_index=0
    for attr in "${attrs[@]}"; do
      attr_index=$((attr_index + 1))
      # This attr's closure minus whatever failed the validity check above.
      closure="$workdir/closure-${attr_index}.txt"
      comm -12 "$workdir/attr-${attr_index}.txt" "$paths" > "$closure"
      if [[ ! -s "$closure" ]]; then
        echo "::warning::kasha emit skipped for '${attr}' — no valid store paths." >&2
        continue
      fi
      gen="${ref}-${sha}${dirty}-$(sanitize "$attr")"
      echo "Emitting manifest ${KASHA_FLAKE}/${gen}…"
      # `-dirty` rides along in the branch too: kasha's retention gives the
      # literal branch `main` the long tier (keep 5 / 4wk), so an unreproducible
      # local push would otherwise displace reviewed main history.
      emit_manifest "$gen" "${ref}${dirty}" "$attr" < "$closure"
    done
  fi
fi
