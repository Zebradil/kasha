#!/usr/bin/env bash
# Refuse to run the box when its nix store lives on NFS.
#
# Nix's store is a sqlite DB plus the store dir; sqlite's advisory locking is
# unreliable over NFS and silently corrupts the DB (ADR-0002). The box store
# MUST be block storage. This guard runs before harmonia starts.
#
# Interface (ADR-0006 env-in -> stdout-out): reads env, writes stdout, exit
# code is the verdict. 0 = not NFS (OK), 1 = NFS (refuse).
set -euo pipefail

store_dir="${KASHA_STORE_DIR:-/nix}"

# ponytail: test seam. Fixtures inject KASHA_STORE_FSTYPE so the check is
# exercisable without a real NFS mount; production leaves it unset and we read
# the live filesystem type. `stat -f -c %T` is GNU coreutils (the box is Linux).
fstype="${KASHA_STORE_FSTYPE:-$(stat -f -c %T -- "$store_dir")}"

case "$fstype" in
nfs | nfs4)
	echo "kasha: refusing to start: nix store '$store_dir' is on NFS ($fstype)." >&2
	echo "kasha: the box store must be block storage — sqlite corrupts under NFS locking (ADR-0002)." >&2
	exit 1
	;;
esac

echo "kasha: nix store '$store_dir' fstype '$fstype' OK (not NFS)."
