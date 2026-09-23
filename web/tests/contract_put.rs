//! A PUT of a contract the APP names (builder#104): the ONE property of its
//! wiring that no behaviour test can see.
//!
//! Its behaviour is tested on the real `Session` in
//! `tests/js/contract-put.test.mjs`: the frames sent, `put_status` through
//! none → pending → put / refused, and re-sent by the page after a dropped
//! socket.
//!
//! What that cannot see: the PUT must be framed BY page-io, on
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

/// The rule, as a check the tests below apply to a body.
fn framed_by_page_io(body: &str) -> bool {
    body.contains("p.put_contract(contract, state,") && !body.contains("frame_put") && !body.contains("self.out")
}

#[test]
fn the_put_is_framed_by_page_io() {
    let put = body_of("put_contract");
    assert!(framed_by_page_io(put), "the PUT is framed outside page-io:\n{put}");
}

/// THE CONTROL: the reader finds the real body, and the negative can fire —
/// on a body that frames the PUT itself (the delegate path's, before the
/// switch-over removed it).
#[test]
fn control_the_reader_finds_the_body_and_the_check_can_fail() {
    let put = body_of("put_contract");
    assert!(put.contains("wire::puts::contract(&code, &params, &state)"), "the reader did not find put_contract's body");
    let delegate_era = "p.put_contract(contract, state, now)?; let frames = wire::frame_put(contract, state, stream)?; self.out.extend(frames);";
    assert!(!framed_by_page_io(delegate_era), "the check passes a body that frames the PUT on the Session's own stream");
}
