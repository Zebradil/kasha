#!/usr/bin/env bash
# Non-systemd OCI entrypoint for the kasha box image.
# Env follows the existing KASHA_* scripts where possible.
set -euo pipefail

data_root="${KASHA_DATA_ROOT:-/kasha}"
store_dir="$data_root/nix/store"
db_path="$data_root/nix/var/nix/db/db.sqlite"
port="${KASHA_PORT:-5000}"

pids=()

bool_enabled() {
	case "${1:-}" in
	1 | true | yes | on | enable | enabled) return 0 ;;
	"" | 0 | false | no | off | disable | disabled) return 1 ;;
	*)
		echo "kasha: invalid boolean '$1'" >&2
		exit 2
		;;
	esac
}

stop_children() {
	trap - TERM INT EXIT
	for pid in "${pids[@]}"; do
		kill "$pid" 2>/dev/null || true
	done
	wait || true
}
trap stop_children TERM INT EXIT

write_config() {
	mkdir -p /etc/nix /run/harmonia "$data_root/nix/store" "$data_root/nix/var/nix/db" "$data_root/var/lib/kasha/roots"
	KASHA_STORE_DIR="$data_root/nix" kasha-check-store-fs
	nix-store --store "local?root=$data_root" --init
	cat >/run/kasha-nix <<EOF
#!/usr/bin/env sh
exec nix --store 'local?root=$data_root' "\$@"
EOF
	chmod +x /run/kasha-nix

	cat >/etc/nix/nix.conf <<EOF
experimental-features = nix-command flakes
require-sigs = true
trusted-public-keys = ${KASHA_TRUSTED_PUBLIC_KEYS:-}
EOF

	cat >/run/harmonia.toml <<EOF
bind = "[::]:$port"
workers = ${KASHA_WORKERS:-4}
max_connection_rate = ${KASHA_MAX_CONNECTION_RATE:-256}
priority = ${KASHA_PRIORITY:-50}
virtual_nix_store = "/nix/store"
real_nix_store = "$store_dir"
nix_db_path = "$db_path"
EOF
}

start_harmonia() {
	CONFIG_FILE=/run/harmonia.toml SIGN_KEY_PATHS="${KASHA_SIGN_KEY_PATHS:-}" HOME=/run/harmonia harmonia-cache &
	pids+=("$!")
}

start_nix_daemon() {
	: "${KASHA_TRUSTED_PUBLIC_KEYS:?KASHA_TRUSTED_PUBLIC_KEYS required when KASHA_PUSH_ENABLE is set}"
	mkdir -p /nix/var/nix/daemon-socket
	nix --store "local?root=$data_root" daemon --force-untrusted &
	pids+=("$!")
	for _ in $(seq 1 50); do
		[[ -S /nix/var/nix/daemon-socket/socket ]] && return
		sleep 0.1
	done
	echo "kasha: nix daemon socket did not appear" >&2
	exit 1
}

start_sshd() {
	: "${KASHA_PUSH_AUTHORIZED_KEYS:?KASHA_PUSH_AUTHORIZED_KEYS required when KASHA_PUSH_ENABLE is set}"
	user="${KASHA_PUSH_USER:-kasha-push}"
	id "$user" >/dev/null 2>&1 || useradd -m -s /bin/sh "$user"
	mkdir -p "/home/$user/.ssh" /run/sshd
	printf '%s\n' "$KASHA_PUSH_AUTHORIZED_KEYS" >"/home/$user/.ssh/authorized_keys"
	chown -R "$user:$user" "/home/$user/.ssh"
	chmod 700 "/home/$user/.ssh"
	chmod 600 "/home/$user/.ssh/authorized_keys"
	sshd -D -e &
	pids+=("$!")
}

mirror_loop() {
	local direction="$1" interval="$2"
	shift 2
	while true; do
		"$@" || echo "kasha: mirror-$direction failed" >&2
		sleep "$interval"
	done
}

start_mirrors() {
	local flakes flake
	flakes="${KASHA_FLAKES:-}"

	if bool_enabled "${KASHA_MIRROR_DOWN_ENABLE:-}" || bool_enabled "${KASHA_MIRROR_UP_ENABLE:-}"; then
		: "${KASHA_REMOTE:?KASHA_REMOTE required when mirroring is enabled}"
		: "${KASHA_FLAKES:?KASHA_FLAKES required when mirroring is enabled}"
		: "${KASHA_TRUSTED_PUBLIC_KEYS:?KASHA_TRUSTED_PUBLIC_KEYS required when mirroring is enabled}"
	fi

	if bool_enabled "${KASHA_MIRROR_DOWN_ENABLE:-}"; then
		for flake in $flakes; do
			KASHA_REMOTE="$KASHA_REMOTE" KASHA_FLAKE="$flake" KASHA_NIX=/run/kasha-nix KASHA_STATE_DIR="$data_root/var/lib/kasha/mirror-down" \
				mirror_loop down "${KASHA_MIRROR_DOWN_INTERVAL:-300}" kasha-mirror-down &
			pids+=("$!")
		done
	fi

	if bool_enabled "${KASHA_MIRROR_UP_ENABLE:-}"; then
		for flake in $flakes; do
			KASHA_REMOTE="$KASHA_REMOTE" KASHA_FLAKE="$flake" KASHA_NIX=/run/kasha-nix KASHA_STATE_DIR="$data_root/var/lib/kasha/mirror-up" \
				KASHA_LOCAL_ROOTS_DIR="$data_root/var/lib/kasha/roots" \
				mirror_loop up "${KASHA_MIRROR_UP_INTERVAL:-300}" kasha-mirror-up &
			pids+=("$!")
		done
	fi
}

write_config
start_harmonia

if bool_enabled "${KASHA_PUSH_ENABLE:-}"; then
	start_nix_daemon
	start_sshd
fi

start_mirrors

wait -n "${pids[@]}"
