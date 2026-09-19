//! App-facing SDK (ARCHITECTURE.md §19). Grows one surface per build phase:
//! `collection` (1) · `identity` `query` `subscribe` (3) · `inbox` `edge`
//! `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).
//!
//! Every capability is written once in Rust and exposed to JavaScript from
//! [`js`], so the builder and apps get it the phase it lands.

pub use freenet_prolly::{cid, Cid};

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content id of `bytes`, as hex — the form apps and links use.
pub fn cid_hex(bytes: &[u8]) -> String {
    hex(&cid(bytes))
}

#[cfg(target_arch = "wasm32")]
pub mod js {
    //! The JavaScript surface. Thin: no logic lives here.
    use wasm_bindgen::prelude::*;

    /// SDK version, so an app can tell which build it loaded.
    #[wasm_bindgen]
    pub fn version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Content id of `bytes`, as hex.
    #[wasm_bindgen(js_name = cidHex)]
    pub fn cid_hex(bytes: &[u8]) -> String {
        crate::cid_hex(bytes)
    }
}

#[cfg(test)]
mod tests {
    /// The same vectors are checked from JavaScript (`tests/js/cid.test.cjs`), so a
    /// binding that mangles bytes on the way in or out cannot pass both.
    #[test]
    fn cid_vectors_hold_in_rust() {
        let mut n = 0;
        for line in include_str!("../tests/vectors.txt").lines() {
            let (input, want) = line.split_once(' ').unwrap();
            let bytes: Vec<u8> = (0..input.len() / 2)
                .map(|i| u8::from_str_radix(&input[i * 2..i * 2 + 2], 16).unwrap())
                .collect();
            assert_eq!(super::cid_hex(&bytes), want, "input {input}");
            n += 1;
        }
        assert!(n >= 4, "only {n} vectors");
    }
}
