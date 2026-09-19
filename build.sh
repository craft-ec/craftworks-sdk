#!/usr/bin/env bash
# Build the SDK for JavaScript: pkg/web (ES module, for the builder and apps) and
# pkg/node (CommonJS, for tests).
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/craftworks_sdk.wasm
wasm-bindgen --target web    --out-dir pkg/web  "$wasm"
wasm-bindgen --target nodejs --out-dir pkg/node "$wasm"
echo "pkg/web $(wc -c < pkg/web/craftworks_sdk_bg.wasm | tr -d ' ') B"
