//! ONE WRITER of "who wants a block" (WANTED-LIFE, sdk#480 part 3; the owner's "Structure before code"): the three
//! reader indexes -- `reads.waiting`, `repair_slots`, `parked_write.needs` -- change only inside the engine's four
//! transitions `want`, `drop_reader`, `served` and `release_write`. Any other write is a reader nobody's `Unwanted`
//! accounts for: the defect sdk#480 part 1 found five times over. Held by the SOURCE of the engine's production code.
//!
//! THE CONTROL: the four transitions themselves hold writes of each index -- the scan finds what it forbids elsewhere.

const SOURCES: [(&str, &str); 3] = [
    ("src/lib.rs", include_str!("../src/lib.rs")),
    ("src/race_get.rs", include_str!("../src/race_get.rs")),
    ("src/read.rs", include_str!("../src/read.rs")),
];

/// Writes of the three reader indexes, as the engine spells them.
const WRITES: [&str; 14] = [
    "reads.waiting.entry(",
    "reads.waiting.insert(",
    "reads.waiting.remove(",
    "reads.waiting.retain(",
    "reads.waiting.get_mut(",
    "repair_slots.entry(",
    "repair_slots.insert(",
    "repair_slots.remove(",
    "repair_slots.retain(",
    "repair_slots.get_mut(",
    "needs.insert(",
    "needs.remove(",
    "parked_write = None",
    "parked_write.take()",
];

const TRANSITIONS: [&str; 4] = ["fn want(", "fn drop_reader(", "fn served(", "fn release_write("];

/// The production part of a source file: everything before its first top-level `#[cfg(test)]` module.
fn production(src: &str) -> &str {
    src.find("\n#[cfg(test)]\nmod ").map_or(src, |at| &src[..at])
}

/// `src` split into (the four transitions' bodies, everything else). A body runs from its `fn` line to the first
/// line closing it at the same indentation.
fn split(src: &str) -> (String, String) {
    let (mut inside, mut outside) = (String::new(), String::new());
    let mut closing: Option<String> = None;
    for line in src.lines() {
        if let Some(close) = &closing {
            inside.push_str(line);
            inside.push('\n');
            if line == close {
                closing = None;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if TRANSITIONS.iter().any(|t| trimmed.starts_with(t)) {
            closing = Some(format!("{}}}", &line[..line.len() - trimmed.len()]));
            inside.push_str(line);
            inside.push('\n');
            continue;
        }
        outside.push_str(line);
        outside.push('\n');
    }
    (inside, outside)
}

#[test]
fn only_the_four_transitions_write_who_wants_a_block() {
    let mut stray = Vec::new();
    let mut inside_all = String::new();
    for (path, src) in SOURCES {
        let (inside, outside) = split(production(src));
        inside_all.push_str(&inside);
        for (n, line) in outside.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for w in WRITES {
                if line.contains(w) {
                    stray.push(format!("{path}: `{w}` outside want/drop_reader/served/release_write (line {} of the rest): {}", n + 1, line.trim()));
                }
            }
        }
    }
    // THE CONTROL: each index IS written inside the transitions (so the scan can see a write when there is one), and all
    // four transitions were found.
    for t in TRANSITIONS {
        assert!(inside_all.contains(t), "THE CONTROL: the transition `{t}` was not found in the engine's production source");
    }
    for index in ["reads.waiting.", "repair_slots.", "needs."] {
        assert!(inside_all.contains(index), "THE CONTROL: no write of `{index}` inside the transitions: the scan cannot see one");
    }
    assert!(stray.is_empty(), "a reader index is written outside the one writer:\n{}", stray.join("\n"));
}

/// The scan's own control: a write placed outside the transitions is found, and one inside is not.
#[test]
fn the_scan_finds_a_write_outside_and_not_inside() {
    let src = "impl E {\n    fn want(&mut self) {\n        self.repair_slots.entry(x);\n    }\n    fn other(&mut self) {\n        self.repair_slots.remove(&x);\n    }\n}\n";
    let (inside, outside) = split(src);
    assert!(inside.contains("repair_slots.entry(") && !inside.contains("repair_slots.remove("), "the transition's body was not cut: {inside}");
    assert!(outside.contains("repair_slots.remove(") && !outside.contains("repair_slots.entry("), "a write outside was hidden: {outside}");
}
