#!/usr/bin/env bash
# THE CANONICAL GATE: run what this repo actually checks with, once.
#
# # Why this exists
#
# The commands lived in README prose and in habit, and a habit is a second
# hand-written expression of a fact — which §19 says disagrees eventually. It
# did: `cargo test` at the root covers the ROOT PACKAGE ONLY, and a run of it
# reported "175 tests passing" into the bodies of #99 and #100, both merged,
# silently omitting eight workspace members including `testkit` — the crate
# whose tests had just been added. Nothing failed and nothing warned. The only
# tell was a count that did not MOVE.
#
# So the cargo invocation here is DERIVED from the workspace, never restated.
# A hand-written list of members would be the same defect wearing a new name:
# correct the day it is written, silently short the day someone adds a crate.
#
# # What it refuses to do
#
# - **Skip.** A step that cannot run is a FAILURE with its reason. A gate that
#   skips what it cannot do reports success for work it did not check.
# - **Print only a verdict.** It prints what it RAN and the COUNTS, because a
#   gate whose output nobody reads can quietly start checking nothing, and the
#   number is the only thing that shows it did.
# - **Start with no room.** Disk has hit 8.4 GiB twice in one night here, and
#   an ENOSPC inside a gate voids the run rather than failing it honestly.
#
# # If you add to this: two traps it has already hit
#
# - **macOS ships bash 3.2.** No associative arrays (`declare -A`), no `${x^^}`,
#   no `mapfile`. The baseline lookup below is a `grep` function for that
#   reason and not for style.
# - **`$?` after a PIPELINE is the LAST command's status.**
#   `out=$(cmd | tail -1); rc=$?` reads tail's, which is always 0, so a failing
#   step passes in silence. Capture the output, take `rc` from the command
#   itself, and pipe afterwards — the shape used for `fixture-gate` below.

set -uo pipefail
cd "$(dirname "$0")"

RED=""; GREEN=""; OFF=""
if [ -t 1 ]; then RED=$'\033[31m'; GREEN=$'\033[32m'; OFF=$'\033[0m'; fi
fail() { echo "${RED}gate: $*${OFF}" >&2; FAILED=1; }
# A STEP that failed, as distinct from a count that moved. `--accept` records
# a run's counts only when no step failed: a partial run's counts are what it
# reached, not what the tree holds (sdk#159).
step_fail() { STEP_FAILED=1; fail "$@"; }
step() { echo; echo "── $* ──"; }
FAILED=0
STEP_FAILED=0
BASELINE=gate.baseline

# `--accept [--accept-loss MEMBER]...`: see tools/gate-accept.sh.
ACCEPT=0
ACCEPT_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --accept) ACCEPT=1; shift ;;
    --accept-loss)
      [ $# -ge 2 ] || { echo "gate: --accept-loss needs a member name" >&2; exit 2; }
      ACCEPT_ARGS+=(--accept-loss "$2"); shift 2 ;;
    *) echo "gate: unknown argument \`$1\`" >&2; exit 2 ;;
  esac
done
if [ ${#ACCEPT_ARGS[@]} -gt 0 ] && [ $ACCEPT -eq 0 ]; then
  echo "gate: --accept-loss only means something with --accept" >&2; exit 2
fi

# ---------------------------------------------------------------- disk ----
# Before anything, because the failure it prevents is the one that does not
# look like a failure.
MIN_GIB=5
free_gib=$(df -g . 2>/dev/null | awk 'NR==2 {print $4}')
if [ -z "$free_gib" ]; then
  # BSD df -g is macOS; fall back to POSIX blocks.
  free_gib=$(df -k . | awk 'NR==2 {printf "%d", $4/1048576}')
fi
echo "gate: ${free_gib} GiB free"
if [ "$free_gib" -lt "$MIN_GIB" ]; then
  echo "${RED}gate: under ${MIN_GIB} GiB free — refusing to start.${OFF}" >&2
  echo "An ENOSPC inside a build voids the run rather than failing it honestly," >&2
  echo "and a voided run reads as a passing one." >&2
  exit 1
fi

# ----------------------------------------------------------- members ----
# From cargo, which is the only thing that cannot be out of date.
need() { command -v "$1" >/dev/null || { echo "${RED}gate: no $1 — cannot run, and will not skip${OFF}" >&2; exit 1; }; }
need cargo
need node
need npm

MEMBERS=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | python3 -c 'import json,sys; print("\n".join(sorted(p["name"] for p in json.load(sys.stdin)["packages"])))')
if [ -z "$MEMBERS" ]; then
  echo "${RED}gate: cargo metadata named no packages — cannot derive what to test${OFF}" >&2
  exit 1
fi
echo "gate: $(echo "$MEMBERS" | wc -l | tr -d ' ') workspace members, from cargo metadata"

# Members that legitimately have NO host tests, and why. Not a list of
# exceptions to tidy up later: each line is a claim a reviewer can check.
no_host_tests() {
  case "$1" in
    # A `cdylib` for wasm32, loaded into a node. There is no host binary to
    # test; what it answers is answered by RUNNING it, which `probe` does.
    probe-delegate) return 0 ;;
    *) return 1 ;;
  esac
}

# ------------------------------------------------------------- tests ----
step "cargo test, per member"
declare -a NAMES COUNTS
total=0
for m in $MEMBERS; do
  out=$(cargo test -p "$m" --no-fail-fast 2>&1)
  rc=$?
  n=$(echo "$out" | grep -E "^test result" | awk '{s+=$4} END {print s+0}')
  if [ $rc -ne 0 ]; then
    step_fail "cargo test -p $m FAILED"
    echo "$out" | grep -E "^(error|test result: FAILED|---- )" | head -5 >&2
  fi
  if [ "$n" -eq 0 ] && ! no_host_tests "$m"; then
    step_fail "$m has NO tests and no reason recorded — add tests, or add it to \
no_host_tests() WITH its reason. An uncovered member is what this gate is for."
  fi
  NAMES+=("$m"); COUNTS+=("$n")
  total=$((total + n))
done

# ------------------------------------------------------------ clippy ----
step "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings > /tmp/gate-clippy.$$ 2>&1; then
  step_fail "clippy failed"
  grep -E "^(error|warning)" /tmp/gate-clippy.$$ | head -8 >&2
fi
clippy_warnings=$(grep -cE "^(warning|error)" /tmp/gate-clippy.$$ || true)
rm -f /tmp/gate-clippy.$$

# --------------------------------------------------------------- npm ----
step "npm test"
js_ok=0
if [ ! -f pkg/web/craftworks_sdk_bg.wasm ]; then
  # NOT a skip. Four JS tests read the built package, and a gate that passed
  # over them would certify a package nobody built.
  step_fail "pkg/web is not built — run ./build.sh first. npm test checks the \
BUILT package, so passing over it would certify something that does not exist."
else
  if ! npm test > /tmp/gate-npm.$$ 2>&1; then
    step_fail "npm test failed"
    grep -E "FAIL|Error" /tmp/gate-npm.$$ | head -8 >&2
  fi
  js_ok=$(grep -c "^  ok " /tmp/gate-npm.$$ || true)
  rm -f /tmp/gate-npm.$$
  # ZERO IS ITS OWN REFUSAL, separate from "differs from the baseline".
  #
  # A filter that matches nothing exits 0 and prints a success line — every
  # runner behaves this way — so a run that executed NO tests is
  # indistinguishable from one that passed them all, in the only output
  # anyone reads. And a baseline of 0 would AGREE with it, which is how a
  # could-not-check becomes a permanent pass.
  #
  # So it is refused here rather than left to the comparison below: the
  # comparison answers "did it change", and this answers "did it run".
  [ "$js_ok" -eq 0 ] && step_fail "npm test reported ZERO passing tests. A filter \
that matches nothing exits 0 and prints a success line, so this is a run that \
checked nothing rather than a suite that passed."
fi

# ------------------------------------------------------ fixture gate ----
step "fixture-gate.sh"
[ -x ./fixture-gate.sh ] || { step_fail "fixture-gate.sh is missing or not executable"; }
fixture_out=$(./fixture-gate.sh 2>&1)
fixture_rc=$?
fixture_line=$(echo "$fixture_out" | tail -1)
[ $fixture_rc -ne 0 ] && step_fail "fixture-gate failed: $fixture_line"

# ----------------------------------------------------------- summary ----
# WHAT IT RAN and the COUNTS, not a verdict on its own.
step "summary"
printf "%-18s %8s %8s\n" "member" "tests" "vs base"
moved=0
# A lookup, not an associative array: macOS ships bash 3.2, which has none.
base_for() { [ -f "$BASELINE" ] && grep -E "^$1=" "$BASELINE" 2>/dev/null | head -1 | cut -d= -f2; }
for i in "${!NAMES[@]}"; do
  m=${NAMES[$i]}; n=${COUNTS[$i]}
  b=$(base_for "$m")
  if [ -z "$b" ]; then
    printf "%-18s %8s %8s\n" "$m" "$n" "NEW"
    fail "$m is not in $BASELINE — record it with ./gate.sh --accept"
    moved=1
  else
    d=$((n - b))
    mark=""
    [ $d -gt 0 ] && mark="+$d" || mark="$d"
    [ $d -eq 0 ] && mark="—"
    printf "%-18s %8s %8s\n" "$m" "$n" "$mark"
    if [ $d -lt 0 ]; then
      fail "$m LOST $((-d)) test(s). A count that falls is a test that stopped \
running, which is the state this gate exists to make impossible to publish."
      moved=1
    elif [ $d -gt 0 ]; then
      moved=1
    fi
  fi
done
# THE JS HALF, under the same rule as the cargo members.
#
# It was printed and not enforced, so a JavaScript test that silently stopped
# running passed the gate — and that half is what the page and the builder
# actually run. The gate's own warning said "no count moved. If you added a
# test, IT IS NOT BEING RUN" while the npm count moved 66 -> 72 untracked:
# true of a surface it did not watch (sdk#115).
js_base=$(base_for "npm")
if [ -z "$js_base" ]; then
  printf "%-18s %8s %8s\n" "npm" "$js_ok" "NEW"
  fail "npm is not in $BASELINE — record it with ./gate.sh --accept"
  moved=1
else
  jd=$((js_ok - js_base))
  jmark="$jd"; [ $jd -gt 0 ] && jmark="+$jd"; [ $jd -eq 0 ] && jmark="—"
  printf "%-18s %8s %8s\n" "npm" "$js_ok" "$jmark"
  if [ $jd -lt 0 ]; then
    fail "npm LOST $((-jd)) test(s). A count that falls is a test that stopped \
running, which is the state this gate exists to make impossible to publish."
    moved=1
  elif [ $jd -gt 0 ]; then
    moved=1
  fi
fi

# Members recorded but gone. A baseline that outlives its crate is a line
# nobody checks, which is how a stale expectation survives.
if [ -f "$BASELINE" ]; then
  while IFS='=' read -r k _; do
    [ -n "$k" ] || continue
    [ "$k" = "npm" ] && continue
    # No pipe (sdk#138): under pipefail, `echo | grep -q` can report FAILURE
    # exactly when the match succeeds — grep exits on the first match and
    # echo takes SIGPIPE (5 misfires in 3000 at load ~10, reproduced).
    grep -qx -- "$k" <<< "$MEMBERS" || fail "$k is in $BASELINE but is no longer a workspace member"
  done < "$BASELINE"
fi

echo
# COVERAGE ON THE SUCCESS LINE, for both halves, because success is what gets
# believed without reading. A green run that does not say WHICH surface it
# covered reads the same whether it covered one or both.
echo "ran: cargo test per member ($total passing, ${#NAMES[@]} members vs baseline), \
clippy --workspace --all-targets -D warnings ($clippy_warnings warnings), \
npm test ($js_ok ok vs baseline ${js_base:-none}), fixture-gate ($fixture_line)"

if [ $ACCEPT -eq 1 ]; then
  counts_file=$(mktemp)
  for i in "${!NAMES[@]}"; do echo "${NAMES[$i]}=${COUNTS[$i]}" >> "$counts_file"; done
  echo "npm=$js_ok" >> "$counts_file"
  GATE_MIN_GIB=$MIN_GIB ./tools/gate-accept.sh "$BASELINE" "$STEP_FAILED" "$counts_file" ${ACCEPT_ARGS[@]+"${ACCEPT_ARGS[@]}"}
  rc=$?
  rm -f "$counts_file"
  exit $rc
fi

if [ "$moved" -gt 0 ] && [ "$FAILED" -eq 0 ]; then
  fail "counts moved UP — record the ground you gained with ./gate.sh --accept"
fi

if [ "$moved" -eq 0 ] && [ "$FAILED" -eq 0 ]; then
  # THE OTHER HALF, and the half that caught the defect this gate is for: a
  # count that does not move when you meant to add a test is a test that is
  # not being run. The gate cannot know your intent, so it says the sentence.
  echo "gate: no count moved. If you added a test, IT IS NOT BEING RUN — that"
  echo "      is exactly how 175 got published while testkit's tests sat out."
fi

if [ "$FAILED" -ne 0 ]; then
  echo "${RED}gate: FAILED${OFF}" >&2
  exit 1
fi
echo "${GREEN}gate: ok${OFF}"
