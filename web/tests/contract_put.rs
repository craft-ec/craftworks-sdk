//! A PUT of a contract the APP names (builder#104): the ONE property of its
//! wiring that no behaviour test can see.
//!
//! Its behaviour is tested on the real `Session` in
//! `tests/js/contract-put.test.mjs`: the frames sent, `put_status` through
//! none → pending → put / refused, a dropped socket, and page mode.
//!
//! What that cannot see: in page mode the PUT must be framed BY page-io, on
//! page-io's stream counter, not by the Session on its own. Both produce the
//! same frame for the same PUT; they differ only in the chunk STREAM ID, and
//! two chunked requests on one stream id are reassembled into each other at
//! the node. That needs two >512 KiB requests interleaved on one socket and a
//! node that reassembles — a runtime path no test here has. So this reads the
//! source, and says so.

const SRC: &str = include_str!("../src/session.rs");

fn body_of(name: &str) -> &'static str {
    let start = SRC.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &SRC[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn in_page_mode_the_put_is_framed_by_page_io() {
    let put = body_of("put_contract");
    let page = &put[put.find("if self.page_mode {").expect("a page-mode branch")..put.find("} else {").expect("an else")];
    assert!(
        page.contains("p.put_contract(contract, state)") && !page.contains("frame_put") && !page.contains("self.out"),
        "page mode frames the PUT outside page-io:\n{page}"
    );
}

/// THE CONTROL: the reader finds the real body, and the negative can fire.
#[test]
fn control_the_reader_finds_the_body() {
    let put = body_of("put_contract");
    assert!(put.contains("wire::puts::contract(&code, &params, &state)"));
    assert!(put.contains("wire::frame_put("), "the delegate-mode branch this reader must exclude is gone");
}
