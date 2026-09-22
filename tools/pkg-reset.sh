#!/usr/bin/env bash
# A BUILD SHIPS ONLY WHAT IT JUST BUILT (sdk#263): empty pkg/web and pkg/node
# before anything is written into them. Without this, a file an earlier build
# shipped stays after the build stops making it — a pre-#260 build's
# engine_delegate.wasm sat beside signer.wasm, and could be served or packaged
# as if this build had produced it.
#
# pkg/web-symbols is NOT cleared: it is the store of names for every shipped
# hash (`tools/symbolise.mjs`), and a crash report from an older build still
# needs its file.
#
#   tools/pkg-reset.sh <pkg dir>
set -euo pipefail
pkg=${1:?usage: pkg-reset.sh <pkg dir>}
for d in web node; do
  rm -rf "${pkg:?}/$d"
  mkdir -p "$pkg/$d"
  # Checked, not assumed: a directory that could not be emptied (a file held
  # open, a permission) must stop the build rather than ship beside it.
  if [ -n "$(ls -A "$pkg/$d")" ]; then
    echo "pkg-reset: $pkg/$d is not empty after clearing it" >&2
    exit 1
  fi
done
