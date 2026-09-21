//! v5 is decodable before this engine implements it (craftworks-sdk#183,
//! build step 2). Until then a v5 frame is answered exactly as it was before
//! v5 was known: `Unsupported`, naming the versions THIS ENGINE serves —
//! never acted on half-way, never dropped silently, never a panic.

use engine_delegate::serve::{serve, Served};
use protocol::{Reply, Request};

const S: u64 = 0x0000_1234_5678_9abc;

#[test]
fn a_v5_frame_is_answered_unsupported_naming_what_this_engine_serves() {
    let write_from = Request::WriteFrom { write_id: 1, floor: 1, ops: vec![protocol::Op::Delete(b"k".to_vec())] };
    for body in [write_from, Request::Flush, Request::Tick { now: 1 }] {
        let bytes = protocol::encode_v5_request(S, 1_790_000_000_000, 1, &body).expect("encodes");
        match serve(&bytes) {
            Served::Answer(Reply::Unsupported { got, known }) => {
                assert_eq!(got, 5);
                assert_eq!(known, vec![1, 2, 3, 4], "the engine claimed a version it does not serve");
            }
            Served::Answer(other) => panic!("{body:?}: answered {other:?}"),
            Served::Do(r, v, _) => panic!("{body:?}: a v{v} request was ACTED ON before the engine implements v5: {r:?}"),
        }
    }
}

/// THE CONTROL: v4 is still served, so the refusal above is about v5.
#[test]
fn control_a_v4_frame_is_still_served() {
    let bytes = protocol::encode_session_request(4, S, &Request::Flush).expect("encodes");
    assert!(matches!(serve(&bytes), Served::Do(Request::Flush, 4, s) if s == S));
}
