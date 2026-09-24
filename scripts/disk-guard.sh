#!/usr/bin/env bash
# DISK GUARD: refuse a gate or a realnet run when the data volume is short of space.
#
#   disk-guard.sh "<what is about to run>"   exit 0: enough room; 1: REFUSED; 2: could not check
#   disk-guard.sh --report                   the free space and the table, no verdict (exit 0)
#
# ONE copy, here in the SDK (public: it owns gate.sh, and a public repo's gate must not need a private
# sibling). The SDK's gate.sh calls it as ./scripts/disk-guard.sh; the builder's gate and tools/realnet.sh
# call "$CRAFTWORKS_SDK/scripts/disk-guard.sh" -- they already find the SDK that way. Nobody copies it.
# Why it exists: the team's job dirs reached ~120 GB and gates failed for a reason that was not the code
# (gate-accept refusing under 5 GiB, npm "LOST 25 tests"). A run that starts short of space fails late and
# misleads; this one refuses at the start and NAMES who holds the space.
#
# WHO HOLDS IT: every ~/.claude/jobs/*/tmp (with the session's name from that job's state.json), every
# repo's target/ and .claude/worktrees/ in the workspace (the dir holding the repos), and the shared build
# caches in ~/.cache/craftworks-target -- largest first, so the holder is named, not guessed. The table
# costs a `du` of all of it (~18 s measured at 120 GB of job dirs), so it prints on a REFUSAL and on
# --report only, never on a passing run: the refusal is when it matters.
#
# Could-not-check is a FAILURE (exit 2), never a pass: an unreadable free-space figure refuses.
#
# Env: DISK_GUARD_MIN_GB (default 25), DISK_GUARD_PATH (the volume to measure; default $HOME),
#      DISK_GUARD_JOBS (default ~/.claude/jobs), DISK_GUARD_WORKSPACE (default: the dir holding this
#      repo's main checkout), DISK_GUARD_CACHES (default ~/.cache/craftworks-target).
set -u
min=${DISK_GUARD_MIN_GB:-25}
path=${DISK_GUARD_PATH:-$HOME}
jobs=${DISK_GUARD_JOBS:-$HOME/.claude/jobs}
caches=${DISK_GUARD_CACHES:-$HOME/.cache/craftworks-target}
here=$(cd "$(dirname "$0")" && pwd)
workspace=${DISK_GUARD_WORKSPACE:-$(cd "$(git -C "$here" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)/../.." 2>/dev/null && pwd)}
what=${1:-"this run"}

case "$min" in ''|*[!0-9]*) echo "disk-guard: DISK_GUARD_MIN_GB must be a whole number of GiB, not '$min'"; exit 2;; esac

# One row per holder: "<KiB> <label>", then sorted largest first.
rows() {
  for d in "$jobs"/*/tmp; do
    [ -d "$d" ] || continue
    job=$(basename "$(dirname "$d")")
    name=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("name") or "?")' "$(dirname "$d")/state.json" 2>/dev/null || echo "?")
    echo "$(du -sk "$d" 2>/dev/null | cut -f1) job $job ($name)"
  done
  if [ -n "$workspace" ]; then
    for d in "$workspace"/*/target "$workspace"/*/.claude/worktrees; do
      [ -d "$d" ] && echo "$(du -sk "$d" 2>/dev/null | cut -f1) $d"
    done
  fi
  for d in "$caches"/*; do
    [ -d "$d" ] && echo "$(du -sk "$d" 2>/dev/null | cut -f1) $d"
  done
}
table() {
  echo "     size  holder"
  rows | sort -rn | awk '{ kb = $1; $1 = ""; printf "  %6.1fG %s\n", kb / 1048576, $0 }'
}

free_kb=$(df -Pk "$path" 2>/dev/null | awk 'NR == 2 { print $4 }')
case "$free_kb" in
  ''|*[!0-9]*) echo "disk-guard: could not read the free space of $path: $what does not run (could not check is a failure)"; exit 2;;
esac
free_gb=$((free_kb / 1048576))

if [ "$what" = "--report" ]; then
  echo "disk-guard: ${free_gb} GiB free on $path (floor ${min} GiB). Who holds the space, largest first:"
  table
  exit 0
fi

if [ "$free_gb" -ge "$min" ]; then
  echo "disk-guard: ${free_gb} GiB free on $path (floor ${min} GiB): $what may run"
  exit 0
fi

echo "disk-guard: REFUSED: ${free_gb} GiB free on $path, under the ${min} GiB floor, so $what does not run."
echo "Free space first: delete your own merged worktrees and idle target dirs. Who holds the space, largest first:"
table
exit 1
