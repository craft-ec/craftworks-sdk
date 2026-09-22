#!/usr/bin/env bash
# Build the SDK for JavaScript: pkg/web (ES module, for the builder and apps) and
# pkg/node (CommonJS, for tests).
set -euo pipefail
cd "$(dirname "$0")"
# The BROWSER build is the `web` crate: the core plus the framing. The core
# itself is an rlib, so it stays a crate the boundary gate checks rather than
# one it has to excuse.
cargo build --release -p web --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/web.wasm
# Emptied first, so what ships is only what this build wrote (sdk#263).
tools/pkg-reset.sh pkg
# `--out-name`, so the artefact keeps the name every consumer already
# imports. The cdylib moved from `craftworks-sdk` to `web` to keep the core on
# the checked side of the boundary gate; that is a layout decision inside this
# repo and has no business renaming the file the builder loads.
wasm-bindgen --target web    --out-name craftworks_sdk --out-dir pkg/web  "$wasm"
wasm-bindgen --target nodejs --out-name craftworks_sdk --out-dir pkg/node "$wasm"

# THE SHIPPED WASM CARRIES NO FUNCTION NAMES (sdk#228): its `name` section is
# ~25% of the page's first load and nothing at run time reads it. Only that
# section is removed — nothing the loader or wasm-bindgen reads. The names are
# KEPT, beside the package and never shipped, keyed by the SHIPPED bytes' hash:
# a support bundle's `wasm-function[N]` frames name the build they came from,
# and `tools/symbolise.mjs` turns them back into Rust paths offline.
# Required, not best-effort: the artefacts container's address is a hash of
# these bytes, so a build without wasm-tools would publish a different one.
command -v wasm-tools >/dev/null || { echo "wasm-tools is required: build.sh strips the shipped wasm's name section" >&2; exit 1; }
shipped=pkg/web/craftworks_sdk_bg.wasm
names_before=$(wc -c < "$shipped" | tr -d ' ')
stripped_tmp=$(mktemp)
wasm-tools strip --delete '^name$' "$shipped" -o "$stripped_tmp"
stripped_hash=$(shasum -a 256 "$stripped_tmp" | cut -d' ' -f1)
mkdir -p pkg/web-symbols
rm -f pkg/web-symbols/craftworks_sdk_bg.*.wasm
cp "$shipped" "pkg/web-symbols/craftworks_sdk_bg.$stripped_hash.wasm"
mv "$stripped_tmp" "$shipped"
echo "pkg/web/craftworks_sdk_bg.wasm: $names_before -> $(wc -c < "$shipped" | tr -d ' ') B, name section stripped; names kept at pkg/web-symbols/craftworks_sdk_bg.$stripped_hash.wasm"
cp js/wrap.js js/index.js js/connection.js js/session.js js/engine-db.js js/artefacts.js pkg/web/

# EVERY MODULE THE ENTRY CAN REACH IS IN THE PACKAGE.
#
# Computed by following the imports, never by trusting the list above. A list
# is correct on the day it is written and silently wrong on the day someone
# adds a module — which has happened twice: `wrap.js` gained `session.js` and
# `engine-db.js`, this line went on copying the older set, and nothing failed.
# The symptom was ERR_MODULE_NOT_FOUND in a browser, at run time, in another
# repository.
#
# It FAILS the build rather than warning. A package that cannot be imported is
# not a package.
node tools/reachable.mjs pkg/web/index.js pkg/web > /tmp/reach.$$ || {
  echo "pkg/web is not closed under its own imports — see above" >&2; rm -f /tmp/reach.$$; exit 1; }
echo "pkg/web: $(wc -l < /tmp/reach.$$ | tr -d ' ') modules reachable from index.js, all present"
rm -f /tmp/reach.$$

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
tmp_container=$(mktemp)
if [ ! -f "$contracts/build/block.wasm" ]; then
  # A FAILURE, not a skip. A pkg/web with no artefacts produces a builder
  # whose Publish button cannot work, and the symptom would appear a long way
  # from here — the same reason the artefact gate refuses to skip.
  echo "no contracts build at $contracts/build (set CRAFTWORKS_CONTRACTS)" >&2
  exit 1
fi
# THE SIGNER: the one delegate (the engine runs in the page, ruling B). A
# published app opened on another node fetches it from the artefacts
# container like the other three (builder#104). Built and import-gated by its
# own script.
./signer/build.sh >/dev/null
cp target/wasm32-unknown-unknown/release/signer.stripped.wasm pkg/web/signer.wasm
cp "$contracts/build/block.wasm" "$contracts/build/register.wasm" pkg/web/

# THE ARTEFACTS CONTAINER (craftworks-builder#104): the artefacts in ONE
# web container under the `webapp` contract (freenet-contracts epoch 2), which
# the node serves at /v1/contract/web/<address>/<file> — where session.js
# already looks for them. The same bytes for every app of this build, so it is
# made HERE, natively (xz: ~410 KB rather than the browser's uncompressed
# ~1.9 MB), and ships beside the files it holds; a builder only PUTs it.
if [ ! -f "$contracts/build/webapp.wasm" ]; then
  echo "no webapp.wasm in $contracts/build (freenet-contracts epoch 2 or later)" >&2
  exit 1
fi
# THE XZ IS PINNED, as the contracts pin wasm-opt: the container's ADDRESS is
# a hash of xz's exact output, and xz's output can differ between versions.
# Every builder builds this SDK from its own checkout, so an unpinned xz would
# give two machines two addresses for one SDK rev — no sharing between their
# apps, and a build that does not reproduce. A different xz is refused, not
# warned about.
WANT_XZ=${WANT_XZ_OVERRIDE_FOR_TEST:-5.8.3}
got_xz=$(xz --version 2>/dev/null | head -1 | awk '{print $NF}')
if [ "$got_xz" != "$WANT_XZ" ]; then
  echo "xz $got_xz, expected $WANT_XZ: the artefacts container's address is a hash of xz's output," >&2
  echo "  so a different xz publishes a different address for the same SDK rev. Install xz $WANT_XZ." >&2
  exit 1
fi
cargo build -q --release -p wire --bin artefacts-container
container_tool=target/release/artefacts-container
container_json=$("$container_tool" pkg/web "$contracts/build/webapp.wasm" pkg/web/artefacts.webapp)
# DETERMINISM, checked on every build: made twice, it must be the same bytes,
# or its address is a function of the moment rather than of the build.
"$container_tool" pkg/web "$contracts/build/webapp.wasm" "$tmp_container" >/dev/null
cmp -s pkg/web/artefacts.webapp "$tmp_container" ||
  { echo "the artefacts container is not deterministic: two builds differ" >&2; exit 1; }
rm -f "$tmp_container"
# And it HOLDS what it says: unpacked by the real xz and tar, each file is the
# shipped one. (The node's framing: 8 B length, metadata, 8 B length, xz.)
unpacked=$(mktemp -d)
python3 - pkg/web/artefacts.webapp "$unpacked/web.xz" <<'PY'
import sys, struct
s = open(sys.argv[1], 'rb').read()
m = struct.unpack('>Q', s[:8])[0]
w = struct.unpack('>Q', s[8 + m:16 + m])[0]
assert len(s) == 16 + m + w, "the container's framing does not add up"
open(sys.argv[2], 'wb').write(s[16 + m:])
PY
(cd "$unpacked" && xz -dc web.xz | tar -xf -)
for f in craftworks_sdk_bg.wasm signer.wasm block.wasm register.wasm; do
  cmp -s "$unpacked/$f" "pkg/web/$f" ||
    { echo "the artefacts container's $f is not the shipped $f" >&2; exit 1; }
done
rm -rf "$unpacked"
# THE `webapp` CODE, shipped beside the container it validates: a builder
# PUTs a container as (this code, blake3(state), state), and with no copy of
# the code it could publish neither the artefacts container nor an app's
# (builder#104). Not IN the container, and not an artefact an app names: an
# app never runs it, the node does.
cp "$contracts/build/webapp.wasm" pkg/web/

# The signer's hash cannot be inside the SDK's own wasm — a build cannot
# contain its own digest — and the CONTRACT hashes are in `buildInfo()`,
# copied from `hashes.toml` by `build.rs`. This file carries the one that is
# left, beside the bytes it describes.
# EVERY artefact's hash, not only the signer's (sdk#5).
#
# The hashes are what the shared cache is keyed by and what each artefact is
# CHECKED against before it is used, so they are not documentation: an app
# that cannot name the hash cannot share the bytes, and an app that does not
# check it runs whatever the cache holds. Computed from the files as shipped,
# so the manifest is byte-identical to the build by construction rather than
# by someone remembering to update it.
hash_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
size_of() { wc -c < "$1" | tr -d ' '; }
block_hash=$(hash_of pkg/web/block.wasm)
register_hash=$(hash_of pkg/web/register.wasm)
sdk_hash=$(hash_of pkg/web/craftworks_sdk_bg.wasm)
cat > pkg/web/artefacts.json <<JSON
{
  "signer":   { "file": "signer.wasm",          "sha256": "$(hash_of pkg/web/signer.wasm)",
                "bytes": $(size_of pkg/web/signer.wasm) },
  "block":    { "file": "block.wasm",           "sha256": "$block_hash",
                "bytes": $(size_of pkg/web/block.wasm) },
  "register": { "file": "register.wasm",        "sha256": "$register_hash",
                "bytes": $(size_of pkg/web/register.wasm) },
  "sdk":      { "file": "craftworks_sdk_bg.wasm", "sha256": "$sdk_hash",
                "bytes": $(size_of pkg/web/craftworks_sdk_bg.wasm) },
  "container": $container_json,
  "webapp":   { "file": "webapp.wasm",          "sha256": "$(hash_of pkg/web/webapp.wasm)",
                "bytes": $(size_of pkg/web/webapp.wasm) },
  "note": "hashes key the shared artefact cache and are verified before use (sdk#5)"
}
JSON
# wasm-bindgen emits CommonJS for node; say so, since this package is ESM.
echo '{"type":"commonjs"}' > pkg/node/package.json
echo "pkg/web $(wc -c < pkg/web/craftworks_sdk_bg.wasm | tr -d ' ') B + signer $(wc -c < pkg/web/signer.wasm | tr -d ' ') B + 2 contracts"
