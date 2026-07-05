#!/usr/bin/env bash
# Fixture test runner (ADR-0006): env-in -> stdout-out, no bats.
#
# Each fixture lives at tests/fixtures/<group>/<case>/ and is defined by:
#   cmd           - script under scripts/ to run (one line, required)
#   env           - KEY=value lines exported before the run (optional)
#   args          - one argument per line passed to the script (optional)
#   stdin         - fed to the script on stdin (optional; default /dev/null)
#   bin/          - dir prepended to PATH, for fake tools (optional)
#   expected.out  - exact expected stdout (optional; not checked if absent)
#   expected.exit - expected exit code (optional; default 0)
#
# This is the pattern every reusable bash tool is tested with. Run: bash tests/run.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scripts="$root/scripts"
fixtures="$root/tests/fixtures"

fail=0
count=0

while IFS= read -r -d '' casedir; do
	count=$((count + 1))
	name="${casedir#"$fixtures"/}"
	cmd="$(cat "$casedir/cmd")"

	args=()
	if [[ -f "$casedir/args" ]]; then
		while IFS= read -r arg; do
			[[ -z "$arg" ]] && continue
			args+=("$arg")
		done <"$casedir/args"
	fi

	env_args=()
	if [[ -f "$casedir/env" ]]; then
		while IFS= read -r line; do
			[[ -z "$line" || "$line" == \#* ]] && continue
			env_args+=("$line")
		done <"$casedir/env"
	fi

	[[ -d "$casedir/bin" ]] && env_args+=("PATH=$casedir/bin:$PATH")

	stdin_file="/dev/null"
	[[ -f "$casedir/stdin" ]] && stdin_file="$casedir/stdin"

	set +e
	out="$(env "${env_args[@]}" bash "$scripts/$cmd" "${args[@]}" <"$stdin_file" 2>/dev/null)"
	code=$?
	set -e

	want_code=0
	[[ -f "$casedir/expected.exit" ]] && want_code="$(cat "$casedir/expected.exit")"

	ok=1
	if [[ "$code" != "$want_code" ]]; then
		ok=0
		echo "FAIL $name: exit $code, want $want_code"
	fi
	if [[ -f "$casedir/expected.out" ]]; then
		want="$(cat "$casedir/expected.out")"
		if [[ "$out" != "$want" ]]; then
			ok=0
			echo "FAIL $name: stdout mismatch"
			echo "  got:  $out"
			echo "  want: $want"
		fi
	fi
	if [[ "$ok" == 1 ]]; then
		echo "ok   $name"
	else
		fail=1
	fi
done < <(find "$fixtures" -mindepth 2 -maxdepth 2 -type d -print0 | sort -z)

echo "---"
echo "$count fixture(s)"
exit "$fail"
