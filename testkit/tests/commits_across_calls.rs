//! sdk#150: a commit that spans delegate calls must publish.
//!
//! On a real node each answer to the delegate's PUT is its OWN call, so a
//! commit with more than one block finishes over several calls. Measured
//! live: the head bump was emitted in a later call than the block it waits
//! on was confirmed, the per-call scheduler (which starts every call knowing
//! nothing) held it, it was STRANDED -- lost at the end of the call -- and the
//! write never published, on every revision back to #85. `FullNode` handed a
//! call's answers back together, so no native commit ever spanned calls and
//! every native test passed.

use engine_delegate::shell::Inbound;
use protocol::{Op, Reply, Request, WriteState};
use testkit::full_node::FullNode;

fn states(replies: &[Vec<u8>]) -> Vec<WriteState> {
    replies
        .iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|r| match r {
            Reply::WriteState { state, .. } => Some(state),
            _ => None,
        })
        .collect()
}

/// One small key and one 30 KB value: two blocks, so the commit spans calls.
fn two_block_write() -> Request {
    let mut big = vec![0u8; 30 * 1024];
    big[..4].copy_from_slice(b"fill");
    Request::Write {
        write_id: 1,
        ops: vec![
            Op::Put(b"k/fill".to_vec(), big),
            Op::Put(b"k/small".to_vec(), b"small".to_vec()),
        ],
    }
}

#[test]
fn a_commit_spanning_calls_publishes_and_strands_nothing() {
    let node = FullNode::new();
    let mut c = node.connect();
    c.one_answer_per_call();
    let _ = c.step(vec![Inbound::Client(protocol::encode_request(
        protocol::CURRENT,
        &Request::Identity,
    ))]);
    let st = states(&c.client(&two_block_write()));
    println!(
        "one answer per call: {st:?}, max stranded {}",
        c.max_stranded()
    );
    assert_eq!(
        c.max_stranded(),
        0,
        "a call stranded effects -- they are lost when it ends ({st:?})"
    );
    assert!(
        st.contains(&WriteState::Published),
        "a commit spanning calls never published: {st:?}"
    );
}

/// CONTROL: the same write with the answers handed back together -- the
/// fixture's old behaviour -- publishes, so the write itself is fine.
#[test]
fn control_the_same_write_publishes_when_answers_arrive_together() {
    let node = FullNode::new();
    let mut c = node.connect();
    c.answers_together();
    let _ = c.step(vec![Inbound::Client(protocol::encode_request(
        protocol::CURRENT,
        &Request::Identity,
    ))]);
    let st = states(&c.client(&two_block_write()));
    assert!(st.contains(&WriteState::Published), "{st:?}");
    assert_eq!(c.max_stranded(), 0);
}
