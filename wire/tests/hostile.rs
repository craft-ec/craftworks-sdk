//! `unframe` as a trust boundary: nothing a stranger sends may panic, allocate
//! what it chose, or complete a message it did not send.
//!
//! A gateway is not the user's node. Everything here is input somebody else
//! picked, and this crate compiles to wasm with `panic = abort`, where a panic
//! is a dead instance rather than an error an app can catch.

use wire::{unframe, Incoming, Reassembler, Unusable};

fn r() -> Reassembler {
    Reassembler::new()
}

/// Garbage, truncation and emptiness are ANSWERS.
#[test]
fn nothing_malformed_panics_and_every_case_is_said() {
    let mut rs = r();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("one byte", vec![0]),
        ("all ones", vec![0xFF; 64]),
        ("text", b"this is not a host response".to_vec()),
        ("a long run of zeroes", vec![0u8; 4096]),
        // A plausible bincode prefix, then nothing. The dangerous shape: a
        // decoder that accepts a prefix turns one message into another.
        ("a truncated variant tag", vec![0, 0, 0, 0]),
    ];
    for (what, bytes) in cases {
        let got = unframe(&mut rs, &bytes);
        assert!(
            matches!(got, Incoming::Unusable(_) | Incoming::Refused(_)),
            "{what}: expected an answer, got {got:?}"
        );
    }
}

/// A frame larger than this build will decode is refused BEFORE it is decoded.
#[test]
fn an_oversized_frame_is_refused_before_it_is_read() {
    let mut rs = r();
    let huge = vec![0u8; wire::MAX_FRAME + 1];
    assert_eq!(
        unframe(&mut rs, &huge),
        Incoming::Unusable(Unusable::TooLarge)
    );
}

/// An absurd `total` is refused at the FIRST chunk, before the slots for it
/// are allocated. The sender picks that number.
#[test]
fn an_absurd_total_is_refused_at_the_first_chunk() {
    let mut rs = r();
    for total in [0u32, 257, 100_000, u32::MAX] {
        assert_eq!(
            rs.chunk(1, 0, total, vec![1, 2, 3]),
            Err(Unusable::BadStream),
            "total {total} was accepted"
        );
    }
    assert_eq!(rs.in_flight(), 0, "a refused stream was still recorded");
    assert_eq!(rs.bytes(), 0, "a refused stream held bytes");
}

/// An index outside its own total is refused.
#[test]
fn an_index_past_its_own_total_is_refused() {
    let mut rs = r();
    assert_eq!(rs.chunk(1, 5, 3, vec![1]), Err(Unusable::BadStream));
    assert_eq!(rs.chunk(1, 3, 3, vec![1]), Err(Unusable::BadStream));
    assert_eq!(
        rs.chunk(1, 2, 3, vec![1]),
        Ok(None),
        "a valid index must work"
    );
}

/// A sender cannot re-shape a stream in flight by changing its total.
#[test]
fn a_second_total_for_one_stream_is_refused() {
    let mut rs = r();
    assert_eq!(rs.chunk(7, 0, 3, vec![1]), Ok(None));
    assert_eq!(
        rs.chunk(7, 1, 9, vec![2]),
        Err(Unusable::BadStream),
        "a chunk claiming a different total was taken as part of the stream"
    );
}

/// Interleaved streams complete independently and do not mix.
#[test]
fn interleaved_streams_do_not_contaminate_each_other() {
    let mut rs = r();
    assert_eq!(rs.chunk(1, 0, 2, b"aa".to_vec()), Ok(None));
    assert_eq!(rs.chunk(2, 0, 2, b"bb".to_vec()), Ok(None));
    assert_eq!(
        rs.chunk(2, 1, 2, b"BB".to_vec()),
        Ok(Some(b"bbBB".to_vec()))
    );
    assert_eq!(
        rs.chunk(1, 1, 2, b"AA".to_vec()),
        Ok(Some(b"aaAA".to_vec()))
    );
    assert_eq!(rs.in_flight(), 0);
}

/// A duplicate chunk REPLACES rather than accumulating.
///
/// Otherwise a sender grows a stream past its own declared size simply by
/// re-sending, which is an allocation it chose after the bound was checked.
#[test]
fn a_duplicate_chunk_replaces_and_does_not_grow_the_stream() {
    let mut rs = r();
    assert_eq!(rs.chunk(1, 0, 2, vec![b'x'; 100]), Ok(None));
    let after_one = rs.bytes();
    for _ in 0..50 {
        assert_eq!(rs.chunk(1, 0, 2, vec![b'x'; 100]), Ok(None));
    }
    assert_eq!(
        rs.bytes(),
        after_one,
        "fifty duplicates of one chunk grew the stream"
    );
    assert_eq!(
        rs.chunk(1, 1, 2, vec![b'y'; 100]),
        Ok(Some([vec![b'x'; 100], vec![b'y'; 100]].concat()))
    );
}

/// A message that never completes does not wedge reassembly.
///
/// stdlib's own buffer evicts stale streams — but `evict_stale` is
/// `#[cfg(not(target_family = "wasm"))]`, so in a BROWSER nothing ages one
/// out, and eight abandoned streams would fill the cap permanently. That is
/// something a node could do on purpose and a flaky connection could do by
/// accident.
#[test]
fn abandoned_streams_never_wedge_reassembly() {
    let mut rs = r();
    // Nine streams started, none finished — one more than the cap.
    for id in 0..9u32 {
        assert_eq!(rs.chunk(id, 0, 2, vec![id as u8; 10]), Ok(None));
    }
    // A new stream still works, which is the whole point.
    assert_eq!(rs.chunk(100, 0, 2, b"aa".to_vec()), Ok(None));
    assert_eq!(
        rs.chunk(100, 1, 2, b"AA".to_vec()),
        Ok(Some(b"aaAA".to_vec())),
        "a fresh stream could not complete because abandoned ones held every \
         slot — replies would simply stop arriving"
    );
}

/// The bytes held in flight are bounded too, because a count is not a byte
/// budget.
#[test]
fn the_bytes_held_in_flight_are_bounded() {
    let mut rs = r();
    rs.max_bytes = 1000;
    for id in 0..4u32 {
        let _ = rs.chunk(id, 0, 4, vec![b'x'; 400]);
    }
    assert!(
        rs.bytes() <= 1000,
        "held {} B against a cap of 1000",
        rs.bytes()
    );
}

/// A reconnect clears what was in flight.
///
/// Stream ids restart at zero on a new connection — stdlib's `next_stream_id`
/// lives on the client — so a half-finished stream from the old connection
/// would be reassembled with chunks of a DIFFERENT message under the same id.
/// Silently, and into something that then fails to decode.
#[test]
fn a_reconnect_clears_what_was_in_flight() {
    let mut rs = r();
    assert_eq!(rs.chunk(0, 0, 2, b"old".to_vec()), Ok(None));
    rs.reset();
    assert_eq!(rs.in_flight(), 0);
    assert_eq!(rs.bytes(), 0);
    // The same id, a new connection, a different message: it starts clean.
    assert_eq!(rs.chunk(0, 0, 2, b"nw".to_vec()), Ok(None));
    assert_eq!(
        rs.chunk(0, 1, 2, b"ok".to_vec()),
        Ok(Some(b"nwok".to_vec()))
    );
}

/// A refusal may drive a bounded retry and may NEVER ask for a credential.
///
/// The node chooses the reason, so it steers client control flow. "Refused:
/// bad key → ask the person to re-enter their passphrase" is a phishing
/// channel delivered through the protocol. This is a function rather than a
/// comment so a future caller finds an answer instead of inventing one.
#[test]
fn no_refusal_may_ever_lead_to_a_credential_prompt() {
    for said in [
        "bad key",
        "unauthorized",
        "your session expired, sign in again",
        "please re-enter your passphrase",
    ] {
        let r = wire::Refused { said: said.into() };
        assert!(
            !r.may_prompt_for_credentials(),
            "a node-supplied reason ({said}) was allowed to prompt for a credential"
        );
    }
}

/// The URL carries `encodingProtocol=native`, and that is not cosmetic.
///
/// The node falls back to Flatbuffers when the parameter is absent
/// (`client_events/websocket.rs:904` at `ea1ff5f`) while everything here
/// speaks bincode. Built in Rust because it is a protocol fact — the page's
/// first socket omitted it, and the failure would have been a connection that
/// opens and then never understands anything.
#[test]
fn the_url_asks_for_the_encoding_this_crate_actually_speaks() {
    let url = wire::ws_url("127.0.0.1", 7711);
    assert!(
        url.contains("encodingProtocol=native"),
        "the URL does not ask for native encoding: {url}"
    );
    assert!(url.starts_with("ws://127.0.0.1:7711/"), "{url}");
}
