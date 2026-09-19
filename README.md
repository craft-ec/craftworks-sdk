# craftworks-sdk

App-facing SDK. Every substrate capability is written once in Rust and exposed to
JavaScript the phase it lands: `collection` (1) · `identity` `query` `subscribe` (3) ·
`inbox` `edge` `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).

    ./build.sh                     # → pkg/web (ES module) and pkg/node (CommonJS)
    cargo test                     # Rust side of the shared vectors
    node tests/js/cid.test.cjs     # JS side of the same vectors

`tests/vectors.txt` is checked from both languages, so a binding that mangles
bytes on the way in or out cannot pass both. Needs `wasm-bindgen-cli` 0.2.128
(pinned to the crate version in `Cargo.toml`).
