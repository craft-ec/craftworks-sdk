//! The SDK must write trees the Block contract will KEEP.
//!
//! The parity tests in `parity.rs` check the writer against the checker in the
//! SAME build of the tree library, so they are self-consistent by construction
//! and CANNOT see a version skew — run them against the old pin and they still
//! pass. Measured: they do.
//!
//! The skew is the failure that actually happened. The SDK pinned the library
//! from before the parity rule while freenet-contracts' Block contract enforced
//! it, so every node this SDK wrote would have been refused, and nothing here
//! noticed — `buildInfo().prollyRev` did, on its first run. This file is that
//! observation turned into a gate: the two revisions are read from the two
//! repositories and compared.

use std::path::{Path, PathBuf};

fn contracts_repo() -> PathBuf {
    std::env::var("CRAFTWORKS_CONTRACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../freenet-contracts"))
}

/// `freenet-prolly = { git = "…", rev = "abcdef1" }` → `abcdef1`.
fn pinned_rev(manifest: &str) -> Option<String> {
    for line in manifest.lines() {
        let line = line.trim();
        if !line.starts_with("freenet-prolly") {
            continue;
        }
        let at = line.find("rev")?;
        let rest = &line[at..];
        let open = rest.find('"')? + 1;
        let close = rest[open..].find('"')? + open;
        return Some(rest[open..close].to_string());
    }
    None
}

#[test]
fn the_sdk_pins_the_tree_library_the_block_contract_pins() {
    let mine = pinned_rev(include_str!("../Cargo.toml")).expect("this crate pins freenet-prolly");
    let block = contracts_repo().join("block/Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&block) else {
        // Absent sibling repo: say so loudly rather than pass quietly. A gate
        // that silently does nothing when its subject is missing is a gate that
        // reports green for the wrong reason.
        println!(
            "SKIPPED: {} not found — set CRAFTWORKS_CONTRACTS to check the pin agreement",
            block.display()
        );
        return;
    };
    let theirs = pinned_rev(&text).expect("the Block contract pins freenet-prolly");
    assert_eq!(
        mine, theirs,
        "the SDK writes trees with freenet-prolly {mine} while the Block contract \
         validates them with {theirs}. A node written under one rule and checked \
         under another is refused by every host, and no test inside this repo can \
         see it — both sides of those tests are the same library."
    );
    println!("ok both repos pin freenet-prolly {mine}");
}

#[test]
fn the_pin_reader_reads_the_thing_it_claims_to() {
    // The comparison above is only worth anything if this parser works; a
    // parser that returned None for everything would make the assertion
    // unreachable and the test green.
    assert_eq!(
        pinned_rev(r#"freenet-prolly = { git = "https://x", rev = "abc1234" }"#).as_deref(),
        Some("abc1234")
    );
    assert_eq!(pinned_rev("serde = \"1\"\n"), None);
    assert!(pinned_rev(include_str!("../Cargo.toml")).is_some());
}
