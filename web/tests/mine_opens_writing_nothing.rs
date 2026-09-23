//! OPENING AN APP WRITES NOTHING (DATA-SOURCE `mine`: the user's tree is made
//! on their FIRST WRITE; the loader: "key and tree made on their first
//! entry"). Seen live: a published guestbook opened on the owner's node PUT a
//! block through it at once, on every realnet run -- `openOwn` claimed (and
//! minted, and provisioned) at open, and `define` wrote the app's schemas.
//!
//! `Session` is `#[wasm_bindgen]` in a `cdylib`, so nothing native can call
//! it; like `writes_park.rs`, this reads the source. (The define is an
//! ordinary queued write held by the engine's `CanSign(false)` while the
//! user has no tree: page-io's `with_no_tree_writes_wait_visible_and_…`.) The BEHAVIOUR under it is
//! page-io's, tested against a wire node in `page-io/tests/wire_node.rs`
//! (`opening_and_reading_the_users_own_tree_sends_no_put_and_the_first_write_lands`,
//! `a_returning_identity_reads_its_rows_with_no_put`).

const SRC: &str = include_str!("../src/session.rs");
const JS: &str = include_str!("../../js/session.js");

/// The body of `fn <name>`, comments stripped (what the CODE does).
fn body_of(name: &str) -> String {
    let at = SRC
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("no `fn {name}` in session.rs: this gate is reading nothing"));
    let rest = &SRC[at..];
    let end = rest.find("\n    }\n").unwrap_or_else(|| panic!("could not find the end of `{name}`"));
    rest[..end]
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `open_own` (what `openOwn` calls at OPEN) claims only an identity the
/// node's signer already holds -- that claim sends nothing -- and never
/// claims (mints, provisions) where there is no key or no signer.
#[test]
fn open_own_claims_only_an_identity_already_here() {
    let b = body_of("open_own");
    let guard = b.find("Asked::Register(_)").unwrap_or_else(|| panic!("`open_own` claims without asking whether the key is already here:\n{b}"));
    let claim = b.find("self.claim_own()").unwrap_or_else(|| panic!("`open_own` no longer opens a returning identity's tree:\n{b}"));
    assert!(guard < claim, "`open_own` claims before (outside) the Register guard:\n{b}");
    assert!(!b.contains(".claim("), "`open_own` claims directly, past the guard:\n{b}");
}

/// The FIRST WRITE is where a tree is made: `writable` claims whatever the
/// signer answered (the existing provision path mints there).
#[test]
fn the_first_write_claims() {
    let b = body_of("writable");
    assert!(b.contains("self.claim_own()"), "`writable` no longer claims: a first write could never make the tree:\n{b}");
}

/// `define` is an ORDINARY write that NEVER provisions (#338's ruling): it
/// is queued on the engine -- held there while the user's tree cannot be
/// signed (`CanSign(false)`) and seen by reads through the warm root -- and it
/// does not claim. Only a DATA write does (`writable`, the one provisioning
/// trigger). A VIEW's define is refused at the door, before anything is
/// queued. Mutants: define claiming (define-provisions), a data write not
/// claiming, the view branch falling through to a write.
#[test]
fn define_never_provisions_and_a_views_define_queues_nothing() {
    let d = body_of("define");
    assert!(!d.contains("self.writable(") && !d.contains("self.claim_own("), "`define` provisions: opening an app would make the user's tree:\n{d}");
    let door = d.find("MayWrite::No").unwrap_or_else(|| panic!("`define` has no view door:\n{d}"));
    let write = d.find("self.db.define(").unwrap_or_else(|| panic!("`define` no longer writes the schema:\n{d}"));
    assert!(door < write, "a view's define reaches the write before its door:\n{d}");
    let view_branch = &d[door..write];
    assert!(view_branch.contains("return"), "the view branch does not return before the write:\n{d}");
    for m in ["put", "create_at", "update", "delete"] {
        let b = body_of(m);
        assert!(b.contains("self.writable("), "`{m}` does not go through the provisioning trigger:\n{b}");
    }
}

/// `openOwn` resolves at once: nothing waits on a provisioning that, for a
/// user with no tree yet, only their first write starts.
#[test]
fn open_own_in_javascript_waits_on_no_provisioning() {
    let at = JS.find("const openOwn =").expect("js/session.js has no `openOwn`");
    let line = &JS[at..at + JS[at..].find('\n').expect("one line")];
    assert!(!line.contains("untilProvisioned"), "`openOwn` waits on provisioning at open: {line}");
    assert!(line.contains("claimOwn()"), "`openOwn` no longer asks the session: {line}");
}

/// THE CONTROL: the reader finds real bodies (a gate that reads nothing
/// passes everything).
#[test]
fn control_the_reader_finds_the_bodies() {
    assert!(body_of("open_own").contains("can_write"), "open_own's body was not read");
    assert!(body_of("define").contains("self.db.schema("), "define's body was not read");
}
