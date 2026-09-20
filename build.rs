//! Bake into the wasm what this build IS.
//!
//! The builder copies a built SDK into its own tree and records the rev it
//! asked for beside it. Nothing can currently check that those two agree,
//! because the rev is not inside the wasm — so a stale copy compares equal to
//! a fresh one. This file is what puts it in.
//!
//! The rev must come from the SOURCE, never from the caller. The builder
//! fetches the SDK with `git archive <rev> | tar -x`, so passing the rev in as
//! an environment variable would make the check circular: both sides of the
//! comparison would come from `SDK_REV` and a stale wasm would still match.
//! `git archive` substitutes `$Format:…$` in files marked `export-subst`, so
//! `src/build_rev.txt` carries the real commit inside an archive and the
//! literal placeholder in an ordinary checkout — where `git rev-parse` answers
//! instead.

use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/build_rev.txt");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rustc-env=SDK_BUILD_REV={}", rev());
    println!("cargo:rustc-env=SDK_PROLLY_REV={}", prolly_rev());
}

/// The commit this build came from, or `unknown` when nothing can say.
///
/// `unknown` is a deliberate value, not a fallback that hides a problem: the
/// builder treats it as a mismatch, because a build that cannot name itself is
/// exactly the case the version panel exists to surface.
fn rev() -> String {
    let stamped = std::fs::read_to_string("src/build_rev.txt").unwrap_or_default();
    let stamped = stamped.trim();
    // Inside a `git archive` this is a 40-hex commit. In a checkout it is still
    // the literal placeholder, which starts with `$Format:`.
    if stamped.len() == 40 && stamped.chars().all(|c| c.is_ascii_hexdigit()) {
        return stamped[..7].to_string();
    }
    let Some(head) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".to_string();
    };
    // A rev that names a commit whose code is not what compiled is the
    // confidently-wrong value this whole mechanism exists to prevent.
    match git(&["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => format!("{head}-dirty"),
        Some(_) => head,
        None => format!("{head}-dirty"),
    }
}

/// The `freenet-prolly` rev this build LINKS, read from the lock file.
///
/// From `Cargo.lock` and not `Cargo.toml`: the manifest says what was asked
/// for, the lock says what was resolved, and only one of those was compiled.
fn prolly_rev() -> String {
    let Ok(lock) = std::fs::read_to_string(lock_path()) else {
        return "unknown".to_string();
    };
    let mut in_pkg = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_pkg = false;
        } else if line == "name = \"freenet-prolly\"" {
            in_pkg = true;
        } else if in_pkg {
            if let Some(src) = line.strip_prefix("source = \"") {
                // git+https://…?rev=afcada3#afcada31b3b396d3264455175aef913d5a1ef067
                if let Some(full) = src.rsplit('#').next() {
                    let full = full.trim_end_matches('"');
                    if full.len() >= 7 && full.chars().all(|c| c.is_ascii_hexdigit()) {
                        return full[..7].to_string();
                    }
                }
                return "unknown".to_string();
            }
        }
    }
    "unknown".to_string()
}

/// The lock beside this crate, or the workspace's if this is a member.
fn lock_path() -> std::path::PathBuf {
    let here = Path::new("Cargo.lock");
    if here.exists() {
        return here.to_path_buf();
    }
    Path::new("..").join("Cargo.lock")
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
