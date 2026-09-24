#!/usr/bin/env bash
# ONE MEMBER'S TESTS, for gate.sh: runs `cargo test -p <member> --no-fail-fast`, prints the passed count on
# stdout, and exits with cargo's status. On a FAILURE it KEEPS the whole output -- every panic, every
# assertion message -- in <dir>/<member>-<utc time>.log and names that file on stderr, so a failure that does
# not come back on a rerun is still evidence, not a shrug (sdk#360: a page test failed once in a gate and its
# panic text was gone; five reruns could say nothing about why).
#   gate-member-tests.sh <member> <dir>
#   gate-member-tests.sh --keep <member> <dir> <rc>    (the output on stdin: a member gate.sh ran its own way,
#                                                       e.g. with batch-only targets apart -- the SAME keeping)
set -uo pipefail

# Keep `out` for member `m` in `dir` when `rc` is a failure: the ONE statement of the keeping.
keep() {
  local m=$1 dir=$2 rc=$3 out=$4 kept
  if [ "$rc" -ne 0 ]; then
    mkdir -p "$dir"
    kept="$dir/$m-$(date -u +%Y%m%dT%H%M%SZ).log"
    if printf '%s\n' "$out" > "$kept"; then
      echo "gate: the whole output of cargo test -p $m is kept at $kept" >&2
    else
      echo "gate: could not keep the output of cargo test -p $m at $kept" >&2
    fi
    echo "$out" | grep -E "^(error|test result: FAILED|---- )" | head -5 >&2
  fi
}

if [ "${1:-}" = "--keep" ]; then
  keep "$2" "$3" "$4" "$(cat)"
  exit "$4"
fi
m=$1
dir=$2
out=$(cargo test -p "$m" --no-fail-fast 2>&1)
rc=$?
echo "$out" | grep -E "^test result" | awk '{s+=$4} END {print s+0}'
keep "$m" "$dir" "$rc" "$out"
exit $rc
