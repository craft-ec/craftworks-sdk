//! The two data surfaces must offer the same methods.
//!
//! builder#20 part 1 says Publish "switches a project's backend from
//! in-memory to the ENGINE (the SDK's one async surface — the app code does
//! not change)". That is a claim about two lists of method names, and a claim
//! nothing checks goes stale the first time one side grows a method.
//!
//! So it is checked by reading the SOURCE, not by inspection and not at run
//! time: the wasm surface only exists on `wasm32`, and a gate that could only
//! run on the target it is hardest to run on would not run at all.

use std::collections::BTreeSet;
use std::path::Path;

/// The methods an `impl` block exposes, from its source.
fn methods_in(src: &str, impl_header: &str) -> BTreeSet<String> {
    let start = src
        .find(impl_header)
        .unwrap_or_else(|| panic!("no `{impl_header}` in the source"));
    let body = &src[start..];
    let mut out = BTreeSet::new();
    let mut depth = 0i32;
    for line in body.lines().skip(1) {
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if let Some(rest) = line.trim().strip_prefix("pub fn ") {
            if let Some(name) = rest.split('(').next() {
                out.insert(name.trim().to_string());
            }
        }
        if depth < 0 {
            break;
        }
    }
    assert!(!out.is_empty(), "read no methods from `{impl_header}`");
    out
}

/// Methods that belong to the CONNECTION rather than to the data, so they
/// have no in-memory counterpart and are not drift.
const TRANSPORT_ONLY: &[&str] = &[
    "new",
    "url",
    "outbound",
    "sent",
    "on_inbound",
    "take_progress",
    "unusable",
    "provision",
    "provisioned",
    "exhausted",
    "stalled",
    "refused",
    "reconnected",
    "tick",
    // TIME, AND THE END OF A PAGE. Both are things a page SENDS to the
    // delegate — a `Tick` frame and a `Flush` frame — and an in-memory store
    // has no delegate to tell. `tick_ms` is how often to send the first,
    // read from `protocol` so the page and the engine hold one number.
    "flush",
    "tick_ms",
    // The engine-backed surface's own additions: there is no engine to
    // preload from, or to trace, behind the in-memory one.
    "preload",
    "trace",
    "trace_on",
    // How a parked read learns its range arrived. An in-memory store has no
    // range that has not arrived, so there is nothing for these to report
    // there — they are connection mechanics, not data.
    "take_loads",
    "loads_in_flight",
    // COLD READS IN THE PAGE: how ranges this node does not hold are fetched
    // — the page's own GETs, and their log. An in-memory store holds every
    // block; there is nothing cold to fetch or to report on.
    "set_cold_reads",
    "take_cold_log",
    // The cold reader's one-shot timer: when its earliest fetch is due, and
    // its clock alone at that moment.
    "cold_due_ms",
    "cold_tick",
    // PUBLISHING a contract the app names (builder#104: its web container),
    // and what the node said. A node operation: an in-memory store has no
    // node to put anything on.
    "put_contract",
    "put_status",
    // READING SOMEBODY'S TREE (sdk#239): a reader session on the shared
    // socket — the head it names, that it writes nothing, and whose frames
    // are whose. An in-memory store is one tree and has no socket.
    "open_named",
    "read_only",
    "head_id",
    "unowned",
    // PAGE MODE (ruling B): which engine this session runs — the delegate's,
    // or the in-page one over page-io. A transport choice; an in-memory store
    // has no engine at all.
    "set_page_mode",
    // How this session finds out the head moved, and whether it is being
    // TOLD or polling. An in-memory store has no head on a node and nothing
    // to be notified by — its data cannot change under it — so there is
    // nothing for these to report there.
    "live_mode",
    // Which domains a head move made stale, and which domains are bound so
    // it can name them. An in-memory store has no head on a node and nothing
    // to be notified by — its data cannot change under it — so there is
    // nothing for these to report there.
    "take_stale",
    "bind",
    "unbind",
    // The NAME of what a binding watches — a domain, or one parent's band
    // (sdk#137). Session bookkeeping for `bind`, not a read of data.
    "watch_key",
    // Ask the engine what changed in a domain since this client last looked.
    // An in-memory store IS the tree: nothing can have changed in it that
    // this client did not do, so there is nothing to ask.
    "refresh_domain",
    // Writes made and not yet PUBLISHED, for the page's unsaved-changes
    // guard (sdk#163). An in-memory store is not waiting on a network: it has
    // nothing "not yet published", only everything, lost with the tab — a
    // different fact the page states differently ("in this tab only").
    "unsaved_writes",
];

#[test]
fn the_engine_surface_offers_everything_the_in_memory_one_does() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib = std::fs::read_to_string(dir.join("src/lib.rs")).expect("web/src/lib.rs");
    let session = std::fs::read_to_string(dir.join("src/session.rs")).expect("web/src/session.rs");

    let in_memory = methods_in(&lib, "impl Db {");
    let engine = methods_in(&session, "impl Session {");

    let missing: Vec<&String> = in_memory
        .iter()
        .filter(|m| !engine.contains(*m) && m.as_str() != "new")
        .collect();
    assert!(
        missing.is_empty(),
        "the engine-backed surface is missing {missing:?}, so an app that \
         switched backend would break on exactly those calls — which is the \
         one thing Publish promises will not happen"
    );

    // And the other way, so the engine surface cannot quietly grow a data
    // method the in-memory one lacks: an app written against the engine
    // would then fail on a project that is still in memory.
    let extra: Vec<&String> = engine
        .iter()
        .filter(|m| !in_memory.contains(*m) && !TRANSPORT_ONLY.contains(&m.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "the engine-backed surface has data methods {extra:?} that the \
         in-memory one lacks; add them there, or list them in TRANSPORT_ONLY \
         with a reason"
    );

    println!(
        "  {} data methods agree across both surfaces",
        in_memory.len() - 1
    );
}

/// THE CONTROL: the reader really does find methods, and would notice a
/// missing one.
///
/// Without it, a `methods_in` that silently returned a subset would make the
/// test above pass by finding nothing to compare.
#[test]
fn control_the_reader_notices_a_missing_method() {
    let src = "\nimpl Db {\n    pub fn put(&mut self) {}\n    pub fn get(&mut self) {}\n}\n";
    let found = methods_in(src, "impl Db {");
    assert!(found.contains("put") && found.contains("get"), "{found:?}");
    assert!(
        !found.contains("scan"),
        "the reader claims a method that is not there"
    );
}
