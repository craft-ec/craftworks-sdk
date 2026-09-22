#!/usr/bin/env bash
# THE RATCHET'S ONE WRITER: `./gate.sh --accept` records counts through here.
#
#   gate-accept.sh BASELINE STEP_FAILED COUNTS [--accept-loss NAME]...
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

if [ "$failed" != "0" ]; then
  echo "gate --accept: REFUSED — a step of this run FAILED, so its counts are what it reached, not what the tree holds. $baseline is unchanged." >&2
  exit 1
fi
[ -s "$counts" ] || { echo "gate --accept: REFUSED — this run produced no counts. $baseline is unchanged." >&2; exit 1; }

refused=0
lowered=""
while IFS='=' read -r k n; do
  [ -n "$k" ] || continue
  b=""
  if [ -f "$baseline" ]; then
    while IFS='=' read -r bk bn; do [ "$bk" = "$k" ] && { b=$bn; break; }; done < "$baseline"
  fi
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
if [ "$free_gib" -lt "$min_gib" ]; then
  echo "gate --accept: REFUSED — ${free_gib} GiB free, under ${min_gib} GiB, right before writing $baseline; it is unchanged." >&2
  exit 1
fi
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
