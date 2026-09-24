#!/usr/bin/env bash
# ONE MUTANT, APPLIED, JUDGED AND UNDONE (sdk#391).
#
#   tools/mutant.sh <file> <search> <replace> -- <test command...>
#
#   exit 0  mutant killed    the command failed with the mutant in place AND passes without it
#   exit 1  SURVIVED         the command passed with the mutant in place
#   exit 2  refused / could not judge: <search> is not in <file> exactly once, the mutant
#           changed nothing, the restore could not be proven, or the CONTROL failed (the
#           command fails on the original too, so nothing was measured)
#
# <search> is LITERAL text (newlines allowed) and must occur EXACTLY ONCE: a mutant that
# changed nothing, or changed some other occurrence than the one meant, is not a kill.
#
# THE RESTORE ALWAYS WRITES A FRESH MTIME, on every exit -- pass, fail, error, INT, TERM.
# Why (sdk#389): a restore that brought back an OLDER mtime (`mv` or `cp -p` of a saved
# copy) left the file byte-identical and cargo's build on the LAST mutant, because cargo
# judges freshness by mtime. The next gate and two reruns tested the mutant. So the saved
# copy is written back INTO the file (`cat > file`: same inode, new mtime), and the restore
# is then PROVEN: the bytes equal the original's, and the mtime is newer than the mutant's.
#
# THE COMMAND RUNS IN A PROCESS GROUP OF ITS OWN, and the WHOLE group is gone before the
# restore: on INT/TERM the group is sent TERM (then KILL after 10 s), never only the direct
# child -- a grandchild (rustc, a test binary) still working on the mutant after the restore
# could write an artefact newer than the restored source.
#
# THE CONTROL: after the restore the SAME command runs again, unmutated, and must PASS. It
# is what makes a kill a measurement (a command that already fails on the original "kills"
# every mutant), and it proves the next run builds the original.
#
# A kill whose output shows a Rust compile error is named as one: the mutant was refused
# by the compiler, which is a kill but not an assertion (CLAUDE.md, compile-time gates).
#
# If this process is KILLed (no trap runs), the original is at the path printed first.
#
# Env: MUTANT_LOG (default: a temp file; the mutant run's whole output, also printed).
set -u

usage() { echo "usage: tools/mutant.sh <file> <search> <replace> -- <test command...>" >&2; exit 2; }
[ $# -ge 5 ] || usage
file=$1 search=$2 replace=$3
[ "$4" = "--" ] || usage
shift 4
[ -f "$file" ] || { echo "mutant: no file $file" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/mutant.XXXXXX") || { echo "mutant: no temp dir" >&2; exit 2; }
saved="$tmp/original"
log=${MUTANT_LOG:-$tmp/log}
cp "$file" "$saved" || { echo "mutant: could not save $file" >&2; exit 2; }
echo "mutant: the original of $file is saved at $saved"
sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
mtime_ns() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }
original_sum=$(sum "$saved")

mutated=0
mutant_mtime=0
group=
restore() {
  [ "$mutated" = 1 ] || return 0
  mutated=0
  # A WRITE into the file, never a move or a time-preserving copy: new mtime, same inode.
  cat "$saved" > "$file"
  touch "$file"
  if [ "$(sum "$file")" != "$original_sum" ]; then
    echo "mutant: RESTORE FAILED: $file is not byte-identical to its original (saved at $saved)" >&2
    return 1
  fi
  local now
  now=$(mtime_ns "$file")
  if [ "$now" -le "$mutant_mtime" ]; then
    echo "mutant: RESTORE NOT PROVEN: $file's mtime ($now) is not newer than the mutant's ($mutant_mtime); a build would keep the mutant" >&2
    return 1
  fi
  return 0
}
# End the command's WHOLE process group and wait until none of it is left.
end_group() {
  [ -n "$group" ] || return 0
  kill -TERM -- "-$group" 2>/dev/null
  local i=0
  while kill -0 -- "-$group" 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -eq 100 ]; then
      echo "mutant: the command's process group $group outlived TERM by 10 s; KILL" >&2
      kill -KILL -- "-$group" 2>/dev/null
    fi
    [ "$i" -gt 150 ] && { echo "mutant: process group $group will not end" >&2; return 1; }
    perl -e 'select(undef, undef, undef, 0.1)'
  done
  wait "$group" 2>/dev/null
  group=
  return 0
}
on_signal() {
  echo "mutant: interrupted ($1); ending the command's process group, then restoring $file" >&2
  end_group || exit 2
  restore || exit 2
  rm -rf "$tmp"
  exit "$2"
}
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
trap 'end_group; restore; [ -z "${MUTANT_LOG:-}" ] && rm -rf "$tmp"' EXIT

# Run "$@" in a process group of its own (job control: its pid is the group id), waited on
# in the background -- bash runs a trap only between commands, so a foreground child would
# hold an INT/TERM until it ended. Sets $group; the caller waits.
start() {
  set -m
  "$@" &
  group=$!
  set +m
}

# Exactly one occurrence, replaced literally.
if ! python3 - "$file" "$search" "$replace" <<'PY'
import sys
path, search, replace = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(path, encoding="utf-8").read()
n = src.count(search)
if n != 1:
    sys.stderr.write(f"mutant: REFUSED: the search text occurs {n} times in {path}, not exactly once\n")
    sys.exit(2)
out = src.replace(search, replace)
if out == src:
    sys.stderr.write("mutant: REFUSED: the replacement is identical to the search text; the mutant changes nothing\n")
    sys.exit(2)
with open(path, "w", encoding="utf-8") as f:
    f.write(out)
PY
then
  exit 2
fi
mutated=1
mutant_mtime=$(mtime_ns "$file")
if [ "$(sum "$file")" = "$original_sum" ]; then
  echo "mutant: REFUSED: $file is unchanged after the mutation" >&2
  exit 2
fi

echo "mutant: $file: applied; running: $*"
start "$@" > "$log" 2>&1
wait "$group"
rc=$?
end_group || exit 2
cat "$log"

restore || exit 2
echo "mutant: $file restored (byte-identical, mtime newer than the mutant's)"

# THE CONTROL: the same command on the original must pass.
echo "mutant: control: the same command, unmutated"
start "$@" > "$tmp/control" 2>&1
wait "$group"
control=$?
end_group || exit 2
if [ "$control" -ne 0 ]; then
  cat "$tmp/control"
  echo "mutant: COULD NOT JUDGE: the command fails WITHOUT the mutant too (rc=$control), so nothing was measured"
  exit 2
fi
echo "mutant: control passed"

if [ "$rc" -eq 0 ]; then
  echo "SURVIVED: the command passed with the mutant in place"
  exit 1
fi
if grep -q '^error\[E[0-9]' "$log"; then
  echo "mutant killed (rc=$rc) -- by a COMPILE ERROR, not an assertion"
else
  echo "mutant killed (rc=$rc)"
fi
exit 0
