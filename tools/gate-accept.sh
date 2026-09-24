#!/usr/bin/env bash
# THE RATCHET'S ONE WRITER: `./gate.sh --accept` records counts through here.
#
#   gate-accept.sh BASELINE STEP_FAILED COUNTS [--accept-loss NAME]...
#
# BASELINE is a DIRECTORY of one file per member (`gate.baseline.d/`, sdk#279:
# a single counts file conflicted on nearly every merge), or — for the tests
# of this script's refusals, which predate the split — a single `name=count`
# file. Both are read and written by the same rules below.
#
# COUNTS is this run's `name=count` lines. It is written to BASELINE only if
#   - NO step of the run failed (STEP_FAILED is 0). A partial run's counts
#     are what the run REACHED, not what the tree holds: a suite that stopped
#     partway recorded npm=14 over 146, and the next honest run then read as
#     "counts moved UP" (craftworks-sdk#159).
#   - and NO count is LOWER than the baseline's, unless that member is named
#     with --accept-loss, which prints what is being given up. A lowered count
#     is a test that stopped running; recording it has to be a decision with a
#     name on it, never a side effect of accepting a raise.
# Refused, it exits non-zero and leaves BASELINE byte-identical.
#
# bash 3.2 (macOS): no associative arrays, no mapfile; an empty array under
# `set -u` is expanded as ${a[@]+"${a[@]}"}.
set -uo pipefail
[ $# -ge 3 ] || { echo "usage: gate-accept.sh BASELINE STEP_FAILED COUNTS [--accept-loss NAME]..." >&2; exit 2; }
baseline=$1; failed=$2; counts=$3; shift 3
allow=()
while [ $# -gt 0 ]; do
  case "$1" in
    --accept-loss)
      [ $# -ge 2 ] || { echo "gate --accept: --accept-loss needs a member name" >&2; exit 2; }
      allow+=("$2"); shift 2 ;;
    *) echo "gate --accept: unknown argument \`$1\`" >&2; exit 2 ;;
  esac
done
allowed() { local a; for a in ${allow[@]+"${allow[@]}"}; do [ "$a" = "$1" ] && return 0; done; return 1; }
as_dir=0
case "$baseline" in *.d) as_dir=1 ;; esac
[ -d "$baseline" ] && as_dir=1
# What BASELINE records for one member, or nothing.
recorded() {
  if [ $as_dir -eq 1 ]; then
    [ -f "$baseline/$1" ] && head -1 "$baseline/$1" | tr -d '[:space:]'
  elif [ -f "$baseline" ]; then
    local bk bn
    while IFS='=' read -r bk bn; do [ "$bk" = "$1" ] && { echo "$bn"; return; }; done < "$baseline"
  fi
}

if [ "$failed" != "0" ]; then
  echo "gate --accept: REFUSED — a step of this run FAILED, so its counts are what it reached, not what the tree holds. $baseline is unchanged." >&2
  exit 1
fi
[ -s "$counts" ] || { echo "gate --accept: REFUSED — this run produced no counts. $baseline is unchanged." >&2; exit 1; }

refused=0
lowered=""
while IFS='=' read -r k n; do
  [ -n "$k" ] || continue
  b=$(recorded "$k")
  [ -n "$b" ] || continue
  if [ "$n" -lt "$b" ]; then
    if allowed "$k"; then
      echo "gate --accept: $k LOWERED $b -> $n: $((b - n)) test(s) given up, as named by --accept-loss $k" >&2
      lowered="$lowered $k"
    else
      echo "gate --accept: REFUSED — $k would fall $b -> $n ($((b - n)) test(s) lost). Name it with --accept-loss $k if that is meant." >&2
      refused=1
    fi
  fi
done < "$counts"

# An override that names nothing that fell is stale: it would silently
# license the NEXT loss of that member.
for a in ${allow[@]+"${allow[@]}"}; do
  case " $lowered " in *" $a "*) ;; *)
    echo "gate --accept: REFUSED — --accept-loss $a, but $a did not fall. $baseline is unchanged." >&2
    refused=1 ;;
  esac
done

[ $refused -eq 0 ] || { echo "gate --accept: $baseline is unchanged." >&2; exit 1; }
# THE WRITE IS CHECKED, not assumed (craftworks-sdk#252): on a full disk this
# once left the baseline unchanged while the run looked accepted. So: the free
# space is checked again RIGHT BEFORE writing (the gate's own floor, handed in
# as GATE_MIN_GIB; the start-of-run check is minutes stale by now), a failed
# copy or rename is a failure naming the file, and the baseline must READ BACK
# byte-identical to what was meant — or this says so and exits non-zero.
min_gib=${GATE_MIN_GIB:-5}
free_gib=${GATE_FREE_GIB_FOR_TEST:-$(df -k "$(dirname "$baseline")" | awk 'NR==2 {printf "%d", $4/1048576}')}
[ -n "$free_gib" ] || free_gib=0
if [ "$free_gib" -lt "$min_gib" ]; then
  echo "gate --accept: REFUSED — ${free_gib} GiB free, under ${min_gib} GiB, right before writing $baseline; it is unchanged." >&2
  exit 1
fi
if [ $as_dir -eq 1 ]; then
  # ONE FILE PER MEMBER, each by write-then-rename, and the files of members
  # this run did not count are removed — the directory says what the tree
  # holds, as the whole-file replace did.
  if ! mkdir -p "$baseline"; then
    echo "gate --accept: FAILED to create $baseline — the counts were NOT recorded." >&2
    exit 1
  fi
  names=" "
  while IFS='=' read -r k n; do
    [ -n "$k" ] || continue
    names="$names$k "
    tmp="$baseline/.$k.accept.$$"
    if ! printf '%s\n' "$n" > "$tmp" || ! mv "$tmp" "$baseline/$k"; then
      rm -f "$tmp"
      echo "gate --accept: FAILED to write $baseline/$k — the counts were NOT all recorded." >&2
      exit 1
    fi
  done < "$counts"
  for f in "$baseline"/*; do
    [ -f "$f" ] || continue
    case "$names" in *" $(basename "$f") "*) ;; *) rm -f "$f" ;; esac
  done
  # READ BACK: every count is its member's file, and nothing else is there.
  bad=""
  while IFS='=' read -r k n; do
    [ -n "$k" ] || continue
    [ "$(recorded "$k")" = "$n" ] || bad="$bad $k"
  done < "$counts"
  want=$(grep -c '=' "$counts")
  have=$(find "$baseline" -maxdepth 1 -type f ! -name '.*' | wc -l | tr -d ' ')
  if [ -n "$bad" ] || [ "$want" -ne "$have" ]; then
    echo "gate --accept: FAILED — $baseline does not read back as what was written (${bad:- a file count of $have for $want counts}); the counts are NOT recorded." >&2
    exit 1
  fi
  echo "gate: recorded $want counts in $baseline"
else
  tmp="$baseline.accept.$$"
  if ! cp "$counts" "$tmp" || ! mv "$tmp" "$baseline"; then
    rm -f "$tmp"
    echo "gate --accept: FAILED to write $baseline — the counts were NOT recorded." >&2
    exit 1
  fi
  if ! cmp -s "$counts" "$baseline"; then
    echo "gate --accept: FAILED — $baseline does not read back as what was written; the counts are NOT recorded." >&2
    exit 1
  fi
  echo "gate: recorded $(grep -c '=' "$baseline") counts in $baseline"
fi
