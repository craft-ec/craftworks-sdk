#!/usr/bin/env bash
# ONE MUTANT, APPLIED, JUDGED AND UNDONE (sdk#391).
#
#   tools/mutant.sh <file> <search> <replace> -- <test command...>
#
#   exit 0  mutant killed    the command failed with the mutant in place
#   exit 1  SURVIVED         the command passed with the mutant in place
#   exit 2  refused / could not judge: <search> is not in <file> exactly once, the
#           mutant changed nothing, or the restore could not be proven
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
# A kill whose output shows a Rust compile error is named as one: the mutant was refused
# by the compiler, which is a kill but not an assertion (CLAUDE.md, compile-time gates).
#
# THEN THE RESTORED TREE IS BUILT: MUTANT_CHECK (a shell command), or for a .rs file
# `cargo test --no-run -q -p <its package>` from the package's directory -- the test
# artefacts are rebuilt from the ORIGINAL now, not at some later run. A failed check is
# exit 2: the verdict stands, but the tree is not proven back.
#
# Env: MUTANT_LOG (default: a temp file; the command's whole output, also printed),
#      MUTANT_CHECK (the post-restore build; see above).
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
sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
mtime_ns() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }
original_sum=$(sum "$saved")

mutated=0
mutant_mtime=0
child=
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
on_signal() {
  echo "mutant: interrupted ($1); restoring $file" >&2
  [ -n "$child" ] && kill -TERM "$child" 2>/dev/null && wait "$child" 2>/dev/null
  restore || exit 2
  rm -rf "$tmp"
  exit "$2"
}
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
trap 'restore; [ -z "${MUTANT_LOG:-}" ] && rm -rf "$tmp"' EXIT

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
# In the BACKGROUND and waited on: bash runs a trap only between commands, so a
# foreground child would hold an INT/TERM until the whole test run ended.
"$@" > "$log" 2>&1 &
child=$!
wait "$child"
rc=$?
child=
cat "$log"

restore || exit 2
echo "mutant: $file restored (byte-identical, mtime newer than the mutant's)"

check=${MUTANT_CHECK:-}
if [ -z "$check" ]; then
  case "$file" in
    *.rs)
      dir=$(cd "$(dirname "$file")" && pwd)
      while [ "$dir" != / ] && [ ! -f "$dir/Cargo.toml" ]; do dir=$(dirname "$dir"); done
      pkg=$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$dir/Cargo.toml" 2>/dev/null | head -1)
      [ -n "$pkg" ] || { echo "mutant: no package found for $file, so the restored tree cannot be rebuilt; set MUTANT_CHECK" >&2; exit 2; }
      check="cd '$dir' && cargo test --no-run -q -p '$pkg'"
      ;;
  esac
fi
if [ -n "$check" ]; then
  if ! /bin/bash -c "$check" > "$tmp/check" 2>&1; then
    cat "$tmp/check" >&2
    echo "mutant: the RESTORED tree did not build ($check): the verdict below stands, the tree is not proven back" >&2
    [ "$rc" -eq 0 ] && echo "SURVIVED: the command passed with the mutant in place" || echo "mutant killed (rc=$rc)"
    exit 2
  fi
  echo "mutant: restored tree rebuilt ($check)"
fi
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
