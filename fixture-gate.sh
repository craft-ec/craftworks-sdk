#!/usr/bin/env bash
# Backstop: does any test file build the system BARE instead of through the
# fixtures?
#
# This is the WEAKEST of the three enforcements and deliberately the last. The
# strong ones are structural: the probed fixture is what `Node::new` gives you,
# the bare path is spelled `new_unprobed_for_benchmark`, and the failure dump
# comes free with the fixture so the probed path is also the easy one. A grep
# is what catches the shapes those miss.
#
# Its limits, stated rather than discovered later:
#   * it is a GREP. A helper that constructs bare on a test's behalf is
#     invisible to it, and a legitimate unit test of a constructor looks
#     exactly like a violation.
#   * it therefore has an allow-list, and the allow-list is the honest part:
#     every entry is a file that SHOULD build bare, with the reason beside it.
#
# And the standing rule for any selective scan: it PRINTS ITS COVERAGE on
# success and FAILS if it scanned nothing. A gate that silently matches no
# files is a gate that checks nothing, and it passes for ever.
set -euo pipefail
cd "$(dirname "$0")"

# Files that may construct bare, and why.
allow() {
  case "$1" in
    # The fixtures themselves have to build the thing they wrap.
    testkit/src/lib.rs) return 0 ;;
    testkit/src/full_node.rs) return 0 ;;
    # Tests OF a constructor are about the constructor.
    tests/cached_store.rs) return 0 ;;
    *) return 1 ;;
  esac
}

BARE='CachedStore::new\(|Shell::resume_with\(|Engine::new\('
scanned=0; through_fixture=0; bare_files=()

while IFS= read -r f; do
  scanned=$((scanned + 1))
  if grep -qE "$BARE" "$f"; then
    if allow "$f"; then continue; fi
    bare_files+=("$f")
  elif grep -q "testkit::" "$f"; then
    through_fixture=$((through_fixture + 1))
  fi
done < <(find tests engine/tests engine-delegate/tests testkit -name '*.rs' 2>/dev/null | sort)

# "Could not check" is a FAILURE, not a pass. A scan that matched no files at
# all has told you nothing, and is the shape that stays green for weeks.
if [ "$scanned" -eq 0 ]; then
  echo "fixture-gate: scanned ZERO files — the paths are wrong, and a gate that" >&2
  echo "fixture-gate: checks nothing passes for ever. Failing instead." >&2
  exit 1
fi

# A RATCHET, not an allow-list.
#
# Seventeen files build the system bare today. An allow-list of seventeen
# "not yet migrated" entries is a list of excuses that never shrinks, and one
# more entry always looks reasonable. A recorded COUNT that may fall and never
# rise is the same information with the opposite incentive: migrating lowers
# it, and the next person who adds a bare construction has to explain why the
# number went up.
baseline=$(cat fixture-gate.baseline 2>/dev/null || echo 0)
bare=${#bare_files[@]}

echo "fixture-gate: $scanned test file(s) scanned, $through_fixture through the fixtures, $bare bare (baseline $baseline)"

if [ "$bare" -gt "$baseline" ]; then
  echo >&2
  echo "fixture-gate: bare constructions went UP, $baseline -> $bare:" >&2
  for f in "${bare_files[@]}"; do echo "  $f" >&2; done
  echo "fixture-gate: use testkit's fixtures. If a file genuinely must build bare," >&2
  echo "fixture-gate: add it to allow() WITH ITS REASON — not to the baseline." >&2
  exit 1
fi

if [ "$bare" -lt "$baseline" ]; then
  echo >&2
  echo "fixture-gate: $((baseline - bare)) file(s) migrated since the baseline was set." >&2
  echo "fixture-gate: lower fixture-gate.baseline to $bare so the ground you gained is kept." >&2
  exit 1
fi
