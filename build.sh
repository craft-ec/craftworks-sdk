#!/usr/bin/env bash
# Build the SDK for JavaScript: pkg/web (ES module, for the builder and apps) and
# pkg/node (CommonJS, for tests).
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/craftworks_sdk.wasm
wasm-bindgen --target web    --out-dir pkg/web  "$wasm"
wasm-bindgen --target nodejs --out-dir pkg/node "$wasm"
cp js/wrap.js js/index.js js/connection.js pkg/web/

# THE ARTEFACTS THE SDK PROVISIONS WITH.
#
# Shipped here so a browser page can register a delegate and install contracts
# without the page having to know what either is — the SDK owns provisioning,
# the app stays a consumer. This is the DEVELOPMENT path; the long-term shape
# is fetching both from the network (§19, sdk#5).
#
# The contract wasm is COPIED from the contracts build, never rebuilt here: a
# rebuild is a different key, and the key is what Freenet addresses the
# contract by (F37).
contracts=${CRAFTWORKS_CONTRACTS:-../freenet-contracts}
if [ ! -f "$contracts/build/block.wasm" ]; then
  # A FAILURE, not a skip. A pkg/web with no artefacts produces a builder
  # whose Publish button cannot work, and the symptom would appear a long way
  # from here — the same reason the artefact gate refuses to skip.
  echo "no contracts build at $contracts/build (set CRAFTWORKS_CONTRACTS)" >&2
  exit 1
fi
./engine-delegate/build.sh >/dev/null
delegate=target/wasm32-unknown-unknown/release/engine_delegate.stripped.wasm
cp "$delegate" pkg/web/engine_delegate.wasm
cp "$contracts/build/block.wasm" "$contracts/build/register.wasm" pkg/web/

# The delegate's hash cannot be inside the SDK's own wasm — a build cannot
# contain its own digest — and the CONTRACT hashes are in `buildInfo()`,
# copied from `hashes.toml` by `build.rs`. This file carries the one that is
# left, beside the bytes it describes.
delegate_hash=$(shasum -a 256 pkg/web/engine_delegate.wasm | cut -d' ' -f1)
cat > pkg/web/artefacts.json <<JSON
{
  "delegate": { "file": "engine_delegate.wasm", "sha256": "$delegate_hash",
                "bytes": $(wc -c < pkg/web/engine_delegate.wasm | tr -d ' ') },
  "note": "contract hashes are in buildInfo(), copied from the contracts build"
}
JSON
# wasm-bindgen emits CommonJS for node; say so, since this package is ESM.
echo '{"type":"commonjs"}' > pkg/node/package.json
echo "pkg/web $(wc -c < pkg/web/craftworks_sdk_bg.wasm | tr -d ' ') B + delegate $(wc -c < pkg/web/engine_delegate.wasm | tr -d ' ') B + 2 contracts"
