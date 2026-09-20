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

wasm=target/wasm32-unknown-unknown/release/engine_delegate.wasm
gate=target/release/import-gate

size=$(wc -c < "$wasm" | tr -d ' ')
echo "engine-delegate: ${size} B"

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
