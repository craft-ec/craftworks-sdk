//! ONE WAY OUT (craftworks-docs OBSERVABILITY §2.1; the architect on sdk#399): a recording leaves the device only as the
//! ONE publish filter's output, from the ONE place that closes an observation window (`Page::close_obs_window`). No
//! other production code reads a recording's events, publishes, or takes a window; the recording is RENDERED only by
//! the two `dump` functions, for a person's eyes on the device. Held by source, over every production file of the page
//! crate and the SDK core, read at test time so a new file is covered without being listed.

mod common;
use common::production_of;
use std::path::Path;

/// `(file, pattern, the fn it sits in)` for every occurrence of `pattern` in the production part of `src`.
fn sites(file: &str, src: &str, pattern: &str) -> Vec<(String, String)> {
    let prod = production_of(src);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = prod[at..].find(pattern) {
        let pos = at + i;
        let before = &prod[..pos];
        let fn_name = before
            .rfind("fn ")
            .map(|f| before[f + 3..].split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("").to_string())
            .unwrap_or_default();
        out.push((file.to_string(), fn_name));
        at = pos + pattern.len();
    }
    out
}

fn production_files() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in [root.join("src"), root.join("../src")] {
        for e in std::fs::read_dir(&dir).expect("a source dir").flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "rs") {
                files.push((p.display().to_string(), std::fs::read_to_string(&p).expect("readable")));
            }
        }
    }
    files
}

/// The rule over the given files: `(what broke it)`s, empty when it holds.
fn breaches(files: &[(String, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (f, src) in files {
        for (file, in_fn) in sites(f, src, "dump::render(") {
            if in_fn != "dump" {
                bad.push(format!("{file}: the recording is rendered in fn {in_fn}, not a `dump` (a rendered recording must stay on the device)"));
            }
        }
        for (file, in_fn) in sites(f, src, ".events()") {
            bad.push(format!("{file}: fn {in_fn} reads a recording's events in production (only the publish filter may)"));
        }
        for pattern in ["instrument::publish::publish(", "take_window()"] {
            for (file, in_fn) in sites(f, src, pattern) {
                if in_fn != "close_obs_window" {
                    bad.push(format!("{file}: `{pattern}` in fn {in_fn}: only `close_obs_window` closes a window and publishes it"));
                }
            }
        }
    }
    bad
}

#[test]
fn a_recording_leaves_only_through_close_obs_window() {
    let files = production_files();
    assert!(files.len() > 10, "THE CONTROL: the scan found {} production files -- it is not looking where the code is", files.len());
    let joined: String = files.iter().map(|(_, s)| production_of(s)).collect();
    assert_eq!(joined.matches("instrument::publish::publish(").count(), 1, "the one publish is not exactly where it should be (or missing)");
    assert_eq!(joined.matches("dump::render(").count(), 2, "a render was added or removed: the two `dump`s are the whole list");
    let bad = breaches(&files);
    assert!(bad.is_empty(), "a recording can leave the device another way:\n{}", bad.join("\n"));
}

/// THE CONTROLS: the rule flags each way out it forbids, and passes the three it allows.
#[test]
fn the_scan_flags_each_way_out_and_passes_the_allowed_ones() {
    let ok = "impl P {\n    pub fn dump(&self) -> String { instrument::dump::render(&r, \"x\", 4) }\n    pub fn close_obs_window(&mut self) { let t = rec.take_window(); instrument::publish::publish(&t, w, None); }\n}\n";
    assert!(breaches(&[("ok.rs".into(), ok.into())]).is_empty(), "the allowed forms were flagged");
    for (name, src) in [
        ("a render outside dump", "fn support_bundle() -> String { instrument::dump::render(&r, \"x\", 4) }\n"),
        ("events read in production", "fn upload() { let e = rec.recording().events(); }\n"),
        ("a second publish", "fn flush() { instrument::publish::publish(&r, w, None); }\n"),
        ("a window taken elsewhere", "fn peek() { let t = rec.take_window(); }\n"),
    ] {
        assert_eq!(breaches(&[("x.rs".into(), src.into())]).len(), 1, "{name} was not flagged");
    }
    // Test code is not production: a test module's `.events()` is fine.
    let test_only = "fn dump() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let e = r.events(); }\n}\n";
    assert!(breaches(&[("t.rs".into(), test_only.into())]).is_empty(), "a test module's read was taken for production");
}
