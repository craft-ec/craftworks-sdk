#!/usr/bin/env bash
# THE DECODER THROUGH wasm-opt -Oz (sdk#347): usage: tools/optimise-decoder.sh <in.wasm> <out.wasm>
#
# The starter is the one fetch nothing races, so the decoder it carries goes through wasm-opt -Oz (39,488 -> 32,414 B
# measured). PINNED, like xz, because the decoder's bytes are in every app starter's address. A missing or different
# binaryen FAILS the build by name: never an unoptimised (or differently optimised) decoder shipped in silence.
set -euo pipefail
want=${WANT_WASM_OPT_OVERRIDE_FOR_TEST:-125}
got=$( (wasm-opt --version 2>/dev/null || true) | awk '{print $3}')
if [ "$got" != "$want" ]; then
  echo "wasm-opt ${got:-(not found on PATH)}, expected binaryen $want: the decoder's bytes (and so every app" >&2
  echo "  starter's address) are wasm-opt's output. Install binaryen $want." >&2
  exit 1
fi
# The features rustc's wasm32 target emits, and only those: -Oz may use what it is allowed, and a browser must run it.
wasm-opt --enable-bulk-memory --enable-sign-ext --enable-nontrapping-float-to-int --enable-mutable-globals \
  --enable-multivalue --enable-reference-types -Oz "$1" -o "$2"
