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
# ONE FILE PER MEMBER (sdk#279): `gate.baseline.d/<member>` holds that
# member's recorded count and nothing else. A single counts file conflicted on
# nearly every merge (14 rebases in one day, each a full re-gate) although no
# two PRs had touched the same member; per member, two PRs conflict only when
# they move the SAME count, which is when they should. A PR's diff still names
# exactly which members' counts moved: the files it changes.
BASELINE=gate.baseline.d

# `--accept [--accept-loss MEMBER]...`: see tools/gate-accept.sh.
# `--pr [--dry-run] [--accept-loss MEMBER]...`: the one command every PR runs (below).
# `--controls`: only the structural controls (below).
ACCEPT=0
ACCEPT_ARGS=()
MODE=full
DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --accept) ACCEPT=1; shift ;;
    --pr) MODE=pr; shift ;;
    --controls) MODE=controls; shift ;;
    --dry-run) DRY=1; shift ;;
    --accept-loss)
      [ $# -ge 2 ] || { echo "gate: --accept-loss needs a member name" >&2; exit 2; }
      ACCEPT_ARGS+=(--accept-loss "$2"); shift 2 ;;
    *) echo "gate: unknown argument \`$1\`" >&2; exit 2 ;;
  esac
done
if [ ${#ACCEPT_ARGS[@]} -gt 0 ] && [ $ACCEPT -eq 0 ] && [ "$MODE" != pr ]; then
  echo "gate: --accept-loss only means something with --accept or --pr" >&2; exit 2
fi
if [ "$MODE" != full ] && [ $ACCEPT -eq 1 ]; then
  echo "gate: --accept is the batch gate's (the full run); a PR never records counts" >&2; exit 2
fi

# THE OLD FORM IS REFUSED, not merged with the new (sdk#279). A branch made
# before the split still carries `gate.baseline`; reading one form and writing
# the other would leave two answers to "what does this member expect", and the
# stale one would be the one nobody looks at.
if [ -e gate.baseline ]; then
  echo "${RED}gate: gate.baseline (the old single file) is present. The counts live in${OFF}" >&2
  echo "${RED}gate: $BASELINE/ now, one file per member (sdk#279): take main's $BASELINE/${OFF}" >&2
  echo "${RED}gate: and delete gate.baseline. Refusing to run with both forms.${OFF}" >&2
  exit 1
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

# --------------------------------------------- the structural controls ----
# THE ONE LIST of the checks that read the WHOLE workspace's source (or the whole branch), whatever a change touches:
# a violation planted in a crate a PR never touches must still fail that PR. `name|command`; a new control is added
# HERE and nowhere else, and `--controls`, `--pr` and the full gate all run this list. A cargo control must report
# at least one passing test: a filter that matches nothing exits 0 and would pass as a control that checked nothing.
CONTROLS=(
  "one_home|cargo test -p core-types --test one_home"
  "one_parity|cargo test -p engine --test one_parity"
  "client_api_allowlist|cargo test -p probe --lib only_allowlisted_crates_may_know_freenets_client_api"
  "node_path_rules|cargo test -p web --test node_path_rules"
  "rg01_one_home|cargo test -p signer-proto --test rg01_one_home"
  "fixture-gate|./fixture-gate.sh"
  "dup-gate|node tools/dup-gate.mjs"
  "owners|node tools/owners.mjs"
)
# BATCH-ONLY TESTS (the owner: a PR runs what is relevant to it): test targets too slow for every PR, as
# `member@target`, each with its measured time. `--pr` skips them BY NAME and prints them ("batch-only, skipped");
# the batch gate (the full run) runs them, and records each one's own count as `gate.baseline.d/member@target`, so a
# PR's count for that member compares against the baseline minus them.
#
# They run in RELEASE: measured 2026-09-24, one model test at CRAFTWORKS_MODEL_SEEDS=2, the same result in both
# profiles, at load ~106-110: debug 35.7 s wall / 13.5 s CPU, release 0.86 s wall / 0.54 s CPU.
BATCH_ONLY=(
  "page@model"
)
batch_only_of() { local e; for e in "${BATCH_ONLY[@]}"; do [ "${e%%@*}" = "$1" ] && echo "${e#*@}"; done; }

run_controls() {
  for c in "${CONTROLS[@]}"; do
    name=${c%%|*}; cmd=${c#*|}
    # Only for the controls' own test (a planted violation, run cheaply): never a way to skip one in a real run.
    [ -n "${GATE_CONTROLS_ONLY:-}" ] && [ "$name" != "$GATE_CONTROLS_ONLY" ] && continue
    out=$(eval "$cmd" 2>&1); rc=$?
    case "$cmd" in
      cargo*)
        n=$(echo "$out" | grep -E "^test result" | awk '{s+=$4} END {print s+0}')
        if [ $rc -ne 0 ]; then step_fail "control $name FAILED: $cmd"; echo "$out" | grep -E "^(error|---- |thread .* panicked)" -A3 | head -12 >&2
        elif [ "$n" -eq 0 ]; then step_fail "control $name ran ZERO tests ($cmd): a control that checks nothing"
        else echo "control $name: ok ($n test(s))"; fi ;;
      *)
        if [ $rc -ne 0 ]; then step_fail "control $name FAILED: $(echo "$out" | tail -1)"; else echo "control $name: ok — $(echo "$out" | tail -1)"; fi ;;
    esac
  done
}

# ONE TARGET PER WORKTREE (the team's rule): a CARGO_TARGET_DIR shared by two worktrees of this workspace builds one
# checkout's code for the other (measured: `unresolved import` in a worktree whose source has it). A target outside
# this tree is claimed by the first worktree that uses it (`.craftworks-worktree`), and any other is refused.
target_guard() {
  local t here mark
  t=$(tools/target-dir.sh) || { echo "${RED}gate: cannot ask cargo where the target is${OFF}" >&2; exit 1; }
  here=$(pwd -P)
  case "$t" in "$here"/*) return 0 ;; esac
  mark="$t/.craftworks-worktree"
  if [ -f "$mark" ] && [ "$(cat "$mark")" != "$here" ]; then
    echo "${RED}gate: CARGO_TARGET_DIR $t belongs to $(cat "$mark"), another worktree.${OFF}" >&2
    echo "${RED}gate: a shared target links the other checkout's code; use one per worktree.${OFF}" >&2
    exit 1
  fi
  mkdir -p "$t" && echo "$here" > "$mark"
}

if [ "$MODE" = controls ]; then
  target_guard
  step "structural controls"
  run_controls
  [ "$FAILED" -ne 0 ] && { echo "${RED}gate: controls FAILED${OFF}" >&2; exit 1; }
  echo "${GREEN}gate: controls ok (${#CONTROLS[@]})${OFF}"; exit 0
fi

# -------------------------------------------------------------- --pr ----
# THE ONE COMMAND EVERY PR RUNS. The full gate (every member, --accept) is the BATCH gate, run once on the batch
# integration. A PR runs: every structural control; the members its files belong to PLUS their reverse dependents
# (tools/pr-scope.mjs, from cargo metadata), each tested with its count printed `before -> after` against the base's
# baseline (a DROP fails, unless named with --accept-loss); clippy on that set; and npm when JS or pkg/ changes.
if [ "$MODE" = pr ]; then
  base=${GATE_PR_BASE:-origin/main}
  git rev-parse -q --verify "$base^{commit}" >/dev/null || { echo "${RED}gate: no base $base — fetch it${OFF}" >&2; exit 1; }
  # The branch's commits AND what is not committed yet: a PR gate run before the commit must see the edit.
  # GATE_PR_CHANGED (space-separated paths) stands in for the diff: the scoping's own test.
  if [ -n "${GATE_PR_CHANGED:-}" ]; then
    changed=$(printf '%s\n' $GATE_PR_CHANGED)
  else
    changed=$({ git diff --name-only "$base"...HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u)
  fi
  meta=$(mktemp)
  cargo metadata --format-version 1 --no-deps > "$meta" 2>/dev/null || { echo "${RED}gate: cargo metadata failed${OFF}" >&2; exit 1; }
  plan=$(printf '%s\n' "$changed" | node tools/pr-scope.mjs plan "$meta")
  trap 'rm -f "$meta"' EXIT
  field() { node -e 'const p=JSON.parse(process.argv[1]); const v=p[process.argv[2]]; process.stdout.write(String(Array.isArray(v)?v.join(" "):v)+"\n")' "$plan" "$1"; }
  scope=$(field members); npm_needed=$(field npm)
  echo "gate --pr: base $base; changed files: $(printf '%s\n' "$changed" | grep -c .)"
  echo "gate --pr: controls: $(for c in "${CONTROLS[@]}"; do printf '%s ' "${c%%|*}"; done)"
  echo "gate --pr: changed members: $(field changed)"
  echo "gate --pr: members tested (changed only; dependents run at the batch gate): ${scope:-(none)}"
  echo "gate --pr: batch-only (skipped here, run by the batch gate): ${BATCH_ONLY[*]}"
  echo "gate --pr: npm: $npm_needed"
  [ "$DRY" -eq 1 ] && exit 0
  target_guard
  step "structural controls"
  run_controls
  drop_ok() { local a; for a in ${ACCEPT_ARGS[@]+"${ACCEPT_ARGS[@]}"}; do [ "$a" = "$1" ] && return 0; done; return 1; }
  base_count() { git show "$base:$BASELINE/$1" 2>/dev/null | head -1 | tr -d '[:space:]'; }
  lines=()
  # EVERY dependent still BUILDS (the architect): compile only, the whole workspace, all targets. Measured warm at
  # load ~113-125: 0.56 s unchanged, 3.3 s after touching engine/src/lib.rs.
  step "cargo check --workspace --all-targets"
  t0=$(date +%s)
  if [ -n "${GATE_CONTROLS_ONLY:-}" ]; then
    echo "cargo check SKIPPED: GATE_CONTROLS_ONLY is the controls' own test, never a PR run"
  elif ! cargo check --workspace --all-targets > /tmp/gate-check.$$ 2>&1; then
    step_fail "cargo check --workspace --all-targets failed: a dependent no longer builds"; grep -E "^error" -A5 /tmp/gate-check.$$ | head -12 >&2
  fi
  rm -f /tmp/gate-check.$$
  echo "cargo check --workspace --all-targets: $(( $(date +%s) - t0 )) s"
  if [ -n "$scope" ]; then
    step "cargo test, the PR's members (before -> after, against $base)"
    for m in $scope; do
      skip=$(batch_only_of "$m" | tr '\n' ' ')
      if [ -n "$skip" ]; then
        ta=$(node tools/pr-scope.mjs test-args "$meta" "$m" $skip)
        targs=$(echo "$ta" | sed -n 1p); tdoc=$(echo "$ta" | sed -n 2p)
        echo "batch-only, skipped: $(for t in $skip; do printf '%s@%s ' "$m" "$t"; done)"
        # shellcheck disable=SC2086
        out=$(cargo test -p "$m" --no-fail-fast $targs 2>&1); rc=$?
        if [ "$tdoc" = doc ]; then dout=$(cargo test -p "$m" --doc 2>&1) || rc=1; out="$out"$'\n'"$dout"; fi
      else
        out=$(cargo test -p "$m" --no-fail-fast 2>&1); rc=$?
      fi
      n=$(echo "$out" | grep -E "^test result" | awk '{s+=$4} END {print s+0}')
      [ $rc -ne 0 ] && { step_fail "cargo test -p $m FAILED"; echo "$out" | grep -E "^(error|test result: FAILED|---- )" | head -5 >&2; }
      b=$(base_count "$m"); [ -z "$b" ] && b=-
      for t in $skip; do
        bt=$(base_count "$m@$t")
        if [ -z "$bt" ] || [ "$b" = - ]; then b="?"; break; fi
        b=$((b - bt))
      done
      if drop_ok "$m"; then l=$(node tools/pr-scope.mjs count "$m" "$b" "$n" --drop-ok); else l=$(node tools/pr-scope.mjs count "$m" "$b" "$n") || fail "$m: count DROPPED"; fi
      echo "$l"; lines+=("$l")
    done
    step "cargo clippy on the PR's members"
    pargs=(); for m in $scope; do pargs+=(-p "$m"); done
    if ! cargo clippy "${pargs[@]}" --all-targets -- -D warnings > /tmp/gate-clippy.$$ 2>&1; then
      step_fail "clippy failed"; grep -E "^(error|warning)" /tmp/gate-clippy.$$ | head -8 >&2
    fi
    rm -f /tmp/gate-clippy.$$
  fi
  if [ "$npm_needed" = true ]; then
    step "build.sh + npm test (JS or pkg/ changed)"
    if ! ./build.sh > /tmp/gate-build.$$ 2>&1; then step_fail "build.sh failed"; tail -5 /tmp/gate-build.$$ >&2
    else
      npm test > /tmp/gate-npm.$$ 2>&1 || { step_fail "npm test failed"; grep -E "FAIL|Error" /tmp/gate-npm.$$ | head -8 >&2; }
      js=$(grep -c "^  ok " /tmp/gate-npm.$$ || true)
      [ "$js" -eq 0 ] && step_fail "npm test reported ZERO passing tests"
      b=$(base_count npm); [ -z "$b" ] && b=-
      if drop_ok npm; then l=$(node tools/pr-scope.mjs count npm "$b" "$js" --drop-ok); else l=$(node tools/pr-scope.mjs count npm "$b" "$js") || fail "npm: count DROPPED"; fi
      echo "$l"; lines+=("$l")
    fi
    rm -f /tmp/gate-build.$$ /tmp/gate-npm.$$
  fi
  step "for the PR body"
  echo "gate --pr ($base): controls ${#CONTROLS[@]}; members ${scope:-none}; npm $npm_needed"
  for l in ${lines[@]+"${lines[@]}"}; do echo "  $l"; done
  [ "$FAILED" -ne 0 ] && { echo "${RED}gate --pr: FAILED${OFF}" >&2; exit 1; }
  echo "${GREEN}gate --pr: ok${OFF}"; exit 0
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
# Only for the gate's OWN test (its batch-only path, run cheaply): never a way to skip members in a batch run, and
# said on every run that uses it.
if [ -n "${GATE_ONLY_MEMBERS:-}" ]; then
  MEMBERS=$(printf '%s\n' $GATE_ONLY_MEMBERS)
  echo "${RED}gate: GATE_ONLY_MEMBERS=$GATE_ONLY_MEMBERS: NOT a batch run — only these members are tested${OFF}"
fi

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
# THE MODEL'S SEEDS (page/tests/model.rs, CRAFTWORKS_MODEL_SEEDS): the batch gate runs the full count, stated.
export CRAFTWORKS_MODEL_SEEDS=${CRAFTWORKS_MODEL_SEEDS:-40}
echo "gate: model seeds $CRAFTWORKS_MODEL_SEEDS (CRAFTWORKS_MODEL_SEEDS)"
echo "gate: batch-only targets, run here: ${BATCH_ONLY[*]}"
[ "$DRY" -eq 1 ] && { echo "gate: --dry-run, nothing run"; exit 0; }
step "cargo test, per member"
declare -a NAMES COUNTS
total=0
FULL_META=$(mktemp)
cargo metadata --format-version 1 --no-deps > "$FULL_META" 2>/dev/null
for m in $MEMBERS; do
  skip=$(batch_only_of "$m" | tr '\n' ' ')
  bo_counts=""
  if [ -n "$skip" ]; then
    # Its batch-only targets apart, in RELEASE (above); the rest of the member as usual.
    ta=$(node tools/pr-scope.mjs test-args "$FULL_META" "$m" $skip)
    # shellcheck disable=SC2086
    out=$(cargo test -p "$m" --no-fail-fast $(echo "$ta" | sed -n 1p) 2>&1); rc=$?
    if [ "$(echo "$ta" | sed -n 2p)" = doc ]; then dout=$(cargo test -p "$m" --doc 2>&1) || rc=1; out="$out"$'\n'"$dout"; fi
    for t in $skip; do
      rout=$(cargo test --release -p "$m" --no-fail-fast --test "$t" 2>&1) || rc=1
      out="$out"$'\n'"$rout"
      bo_counts="$bo_counts $t=$(echo "$rout" | grep -E "^test result" | awk '{s+=$4} END {print s+0}')"
    done
  else
    out=$(cargo test -p "$m" --no-fail-fast 2>&1)
    rc=$?
  fi
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
  # A batch-only target's OWN count, recorded beside its member's, so `--pr` (which skips it) compares like with like.
  for t in $skip; do
    nt=$(printf '%s\n' $bo_counts | grep "^$t=" | cut -d= -f2)
    [ -n "$nt" ] && [ "$nt" -gt 0 ] || { step_fail "batch-only $m@$t ran no tests in the full run"; nt=0; }
    NAMES+=("$m@$t"); COUNTS+=("$nt")
  done
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

# ---------------------------------------------- duplicates, owners ----
# MACHINE checks for what people missed (craftworks-sdk#320): a NEW duplicate
# block (tools/dup-gate.mjs, jscpd pinned, against dup-baseline.json), and a
# branch that changes more than one owner's files (OWNERS) without a
# `shared:` line. Each prints what it found; "could not check" is a failure.
step "dup-gate"
dup_out=$(node tools/dup-gate.mjs 2>&1)
dup_rc=$?
echo "$dup_out" | grep -E '^(NEW DUPLICATE|gone)' | head -20
dup_line=$(echo "$dup_out" | tail -1)
echo "$dup_line"
[ $dup_rc -ne 0 ] && step_fail "dup-gate: $dup_line"

step "owners"
owners_out=$(node tools/owners.mjs 2>&1)
owners_rc=$?
echo "$owners_out"
owners_line=$(echo "$owners_out" | tail -1)
[ $owners_rc -ne 0 ] && step_fail "owners: $owners_line"

# ----------------------------------------------------------- summary ----
# WHAT IT RAN and the COUNTS, not a verdict on its own.
step "summary"
printf "%-18s %8s %8s\n" "member" "tests" "vs base"
moved=0
# A lookup, not an associative array: macOS ships bash 3.2, which has none.
# A member's recorded count: its file's one line, or nothing. NOTHING is not
# zero — a member with no file is NEW (or its file was deleted) and FAILS below,
# so deleting a file can never quietly lower what the gate expects (sdk#279).
base_for() { [ -f "$BASELINE/$1" ] && head -1 "$BASELINE/$1" | tr -d '[:space:]'; }
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
if [ -d "$BASELINE" ]; then
  for f in "$BASELINE"/*; do
    [ -f "$f" ] || continue
    k=$(basename "$f")
    [ "$k" = "npm" ] && continue
    # No pipe (sdk#138): under pipefail, `echo | grep -q` can report FAILURE
    # exactly when the match succeeds — grep exits on the first match and
    # echo takes SIGPIPE (5 misfires in 3000 at load ~10, reproduced).
    grep -qx -- "${k%%@*}" <<< "$MEMBERS" || fail "$k is in $BASELINE but is no longer a workspace member"
  done
fi

echo
# COVERAGE ON THE SUCCESS LINE, for both halves, because success is what gets
# believed without reading. A green run that does not say WHICH surface it
# covered reads the same whether it covered one or both.
echo "ran: cargo test per member ($total passing, ${#NAMES[@]} members vs baseline), \
clippy --workspace --all-targets -D warnings ($clippy_warnings warnings), \
npm test ($js_ok ok vs baseline ${js_base:-none}), fixture-gate ($fixture_line), $dup_line, $owners_line"

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
