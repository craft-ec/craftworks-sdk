//! THE CONTROL (the owner, 2026-09-23: "provide me solution, control so that things won't happen"): hex and the
//! name rules have ONE home, `core_types::{hex, name}`. A hand-written hex encode or decode, or a hand-written
//! app / domain character rule, anywhere else in the workspace fails this test, so a new copy cannot merge -- it is
//! caught by the build, not by a reviewer noticing it.

use std::path::{Path, PathBuf};

/// The hand-written shapes: a two-digit hex format, and a base-16 parse of a slice.
const SHAPES: [&str; 3] = [":02x}", ":02X}", ", 16)"];

/// The name rules' character sets, written as a byte-string `contains`.
const NAME_SHAPES: [&str; 2] = [r#"b"_-".contains"#, r#"b"_-.".contains"#];

/// Lines of `text` that write hex or a name rule by hand.
fn copies(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| {
            (SHAPES.iter().any(|s| l.contains(s)) && (l.contains("format!") || l.contains("from_str_radix")))
                || NAME_SHAPES.iter().any(|s| l.contains(s))
        })
        .map(str::to_string)
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !matches!(name, "target" | "node_modules" | "pkg" | ".git" | "core-types") && !name.starts_with('.') {
                rust_files(&p, out);
            }
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

#[test]
fn hex_and_name_rules_are_written_only_in_core_types() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("the workspace");
    let mut files = Vec::new();
    rust_files(root, &mut files);
    // Not vacuous: the walk must have found the workspace's sources.
    assert!(files.len() > 100, "the scan found only {} .rs files: it is not looking at the workspace", files.len());
    let found: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            copies(&text).into_iter().map(move |l| format!("{}: {}", f.strip_prefix(root).unwrap_or(f).display(), l.trim()))
        })
        .collect();
    assert!(found.is_empty(), "hex or a name rule written by hand outside core-types (use core_types::hex / ::name):\n{}", found.join("\n"));
}

/// The detector's control: each shape it exists for IS caught, and the one call it points to is not.
#[test]
fn the_detector_catches_each_copy_shape() {
    assert_eq!(copies(r#"    b.iter().map(|x| format!("{x:02x}")).collect()"#).len(), 1);
    assert_eq!(copies(r#"    u8::from_str_radix(&s[i..i + 2], 16).ok()"#).len(), 1);
    assert_eq!(copies(r#"    app.bytes().all(|b| b.is_ascii_lowercase() || b"_-".contains(&b))"#).len(), 1);
    assert_eq!(copies(r#"    d.bytes().all(|b| b.is_ascii_digit() || b"_-.".contains(&b))"#).len(), 1);
    assert_eq!(copies("    core_types::hex::encode(&b)").len(), 0);
    assert_eq!(copies("    core_types::name::app_ok(app)").len(), 0);
    assert_eq!(copies(r#"    // format!("{x:02x}") in a comment is not code"#).len(), 0);
}
