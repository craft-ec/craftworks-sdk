#!/usr/bin/env bash
# Build the engine delegate and GATE it, in one step.
#
# The gate is not optional and not separate: a delegate whose imports the node
# does not define fails to INSTANTIATE, so it never runs at all — and the
# symptom is a delegate that is simply never called, a long way from the
# dependency bump that caused it. Building without checking produces exactly
# the artefact that fails that way.
#
# `env -u CARGO_TARGET_DIR` on purpose: the workspace shares one target
# directory for its gates, and a wasm build must not race them or be served a
# host-target artefact from the same path.
set -euo pipefail
cd "$(dirname "$0")/.."

env -u CARGO_TARGET_DIR cargo build -p engine-delegate --release --target wasm32-unknown-unknown
env -u CARGO_TARGET_DIR cargo build -p probe --release --bin import-gate

raw=target/wasm32-unknown-unknown/release/engine_delegate.wasm
wasm=target/wasm32-unknown-unknown/release/engine_delegate.stripped.wasm
gate=target/release/import-gate

# Strip the custom sections. 40% of this artefact is the `name` section --
# debug symbols -- and it is fetched from the network by every node that ever
# loads the delegate, so it is 391 KB each of them pays for symbol names
# nobody reads: a delegate's panics do not reach a developer's backtrace.
#
# Stripped is what is GATED and what ships. Gating the unstripped one and
# shipping the stripped one would be checking a different artefact from the
# one that runs.
if command -v wasm-tools > /dev/null; then
  wasm-tools strip --all "$raw" -o "$wasm"
else
  echo "note: wasm-tools not found — shipping unstripped, ~40% larger" >&2
  cp "$raw" "$wasm"
fi

before=$(wc -c < "$raw" | tr -d ' ')
size=$(wc -c < "$wasm" | tr -d ' ')
echo "engine-delegate: ${size} B stripped (${before} B before)"

# Both halves: an unwired delegate (no imports at all) and an import the node
# does not define. The binary refuses on either.
"$gate" "$wasm"

# C7 (no wake-ups) is enforced BY the allowlist above, not by a second check
# here: `schedule_wakeup` is one of the seven functions the node does not
# define, so a delegate importing it is refused by the line above and never
# reaches this one.
#
# A `grep schedule_wakeup` here would be a check that can never fail — under
# `set -e` the refusal exits first — and a gate that cannot fail is worse than
# no gate, because it reads like one that passed. What proves the allowlist
# really excludes it is a TEST, not a grep: probe's
# `the_gate_refuses_a_host_function_the_node_does_not_define`
# builds a module importing `__frnt__delegate__schedule_wakeup` by hand and
# asserts it is refused, alongside the other six the node dropped.
echo "ok: no wake-up import — C7 holds because schedule_wakeup is not on the node's list"
