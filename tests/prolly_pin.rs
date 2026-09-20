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

/// Where the sibling `freenet-contracts` checkout is.
///
/// `CARGO_MANIFEST_DIR/..` is not enough. Reviews and parallel work happen in a
/// `git worktree` under a session's own temp dir, and from there the sibling
/// repo is not beside the manifest — so this gate found nothing, printed
/// SKIPPED and passed, in exactly the situation it exists for. It was caught
/// by running it by hand with `CRAFTWORKS_CONTRACTS` set, which is not a thing
/// anyone remembers to do.
///
/// So the main checkout is asked for by name: `git rev-parse --git-common-dir`
/// resolves to the ORIGINAL repository's `.git` from inside any worktree of it.
fn contracts_repo() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CRAFTWORKS_CONTRACTS") {
        return Some(PathBuf::from(p));
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let beside = manifest.join("../freenet-contracts");
    if beside.join("block/Cargo.toml").is_file() {
        return Some(beside);
    }
    // In a worktree: find the checkout this one belongs to.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(manifest)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim().to_string());
    let common = if common.is_absolute() {
        common
    } else {
        manifest.join(common)
    };
    // <main repo>/.git → <main repo> → its sibling.
    let main = common.parent()?;
    let beside = main.join("../freenet-contracts");
    beside.join("block/Cargo.toml").is_file().then_some(beside)
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
    // A gate has three outcomes, and "could not check" is not one of them: in
    // any log anyone reads it is indistinguishable from "checked, fine". This
    // used to print SKIPPED and return green, which is how a real skew reached
    // a PR whose suite was entirely green.
    let Some(repo) = contracts_repo() else {
        panic!(
            "cannot find the freenet-contracts checkout, so the pin agreement \
             was NOT checked. Set CRAFTWORKS_CONTRACTS to its path. This is a \
             failure and not a skip: the skew this gate exists for is invisible \
             to every other test in this repo, because both sides of those \
             tests are the same library."
        );
    };
    let block = repo.join("block/Cargo.toml");
    let text = std::fs::read_to_string(&block)
        .unwrap_or_else(|e| panic!("{} could not be read: {e}", block.display()));
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
