//! RACE GET on the page (sdk#303): what the engine WITHDRAWS, the page stops asking.
//!
//! A reader page reads the keys of one member the node never answers. Its race rebuilds that member from the
//! group; one of the group's parity blocks is silent too, and the rebuild did not need it. Before withdrawal the
//! page re-sent that parity block's GET on the RTO for ever, for a read that was answered long ago.

use engine::{ClientId, Effect, Event, Op as WriteOp, Params, WriteId};
use freenet_prolly::node::Node;
use freenet_prolly::Cid;
use page::{Answer, Ms, Op, Page, PutPath};
use std::collections::{BTreeMap, BTreeSet};

/// The node: every block PUT to it, and the head.
#[derive(Default)]
struct Node_ {
    blocks: BTreeMap<Cid, Vec<u8>>,
    head: Option<(u64, Cid)>,
}

/// Drive `p` against `node` until nothing is waiting (or `for_ms` passes), answering after 5 ms; `silent` blocks
/// are never answered. Returns the GETs sent per block, with the time of each.
fn drive(
    p: &mut Page,
    node: &mut Node_,
    silent: &BTreeSet<Cid>,
    now: &mut u64,
    for_ms: u64,
    gets: &mut Vec<(u64, Cid)>,
) {
    let end = *now + for_ms;
    let mut due: Vec<(u64, Answer)> = Vec::new();
    while *now < end {
        for op in p.take_ops() {
            let a = match op {
                Op::Put { id, bytes } => {
                    node.blocks.insert(id, bytes);
                    Some(Answer::PutOk(id))
                }
                Op::Get { id } => {
                    gets.push((*now, id));
                    if silent.contains(&id) {
                        None
                    } else {
                        Some(match node.blocks.get(&id) {
                            Some(b) => Answer::Got {
                                id,
                                bytes: b.clone(),
                            },
                            None => Answer::GetMissed(id),
                        })
                    }
                }
                Op::ReadHead => Some(Answer::Head(node.head.map(Into::into))),
                Op::Sign { id, seq, root, .. } => {
                    node.head = Some((seq, root));
                    let record = [seq.to_le_bytes().as_slice(), &root].concat();
                    Some(Answer::Signer {
                        id,
                        answer: signer_proto::Answer::Signed(record),
                    })
                }
                Op::Update { .. } => Some(Answer::Updated),
                Op::AskHeld { id } => Some(Answer::Held {
                    id,
                    present: node.blocks.contains_key(&id),
                }),
                Op::PutApp { key } => Some(Answer::AppPutOk(key)),
            };
            if let Some(a) = a {
                due.push((*now + 5, a));
            }
        }
        *now += 1;
        let (ready, later): (Vec<_>, Vec<_>) = due.into_iter().partition(|(t, _)| *t <= *now);
        due = later;
        for (_, a) in ready {
            p.answer(a, Ms(*now));
        }
        p.tick(Ms(*now));
    }
}

/// A level-1 node's largest group, `(members, parity)`.
fn a_leaf_group(blocks: &BTreeMap<Cid, Vec<u8>>, root: Cid) -> (Vec<Cid>, Vec<Cid>) {
    let mut at = root;
    loop {
        let n = Node::parse(&blocks[&at]).expect("a node");
        assert!(!n.is_leaf(), "the tree is one leaf: no groups");
        if n.level() == 1 {
            let ids: Vec<Cid> = n.parity().collect();
            let groups = freenet_prolly::parity::group_members(&n);
            let m = ids.len() / groups.len();
            let (g, (_, members)) = groups
                .into_iter()
                .enumerate()
                .max_by_key(|(_, (_, m))| m.len())
                .expect("a group");
            return (members, ids[m * g..m * g + m].to_vec());
        }
        at = n.child(0).0;
    }
}

#[test]
fn a_withdrawn_get_is_not_asked_again() {
    // The writer: a real tree, with its parity, on the node.
    let mut node = Node_::default();
    let mut now = 1_000u64;
    let mut gets = Vec::new();
    let mut w = Page::new(Params::default(), PutPath::Page);
    for batch in 0..12u64 {
        let ops = (0..500u64)
            .map(|i| {
                (
                    format!("k/{:06}", batch * 500 + i).into_bytes(),
                    WriteOp::Put(format!("value {i}").into_bytes()),
                )
            })
            .collect();
        w.write(ClientId(1), WriteId(batch + 1), ops);
        drive(
            &mut w,
            &mut node,
            &BTreeSet::new(),
            &mut now,
            3_000,
            &mut gets,
        );
    }
    let (_, root) = node.head.expect("the writer published");
    let (members, parity) = a_leaf_group(&node.blocks, root);
    assert!(
        parity.iter().all(|p| node.blocks.contains_key(p)),
        "the writer's parity is not on the node"
    );
    let (member, spare) = (members[members.len() / 2], parity[0]);
    let silent: BTreeSet<Cid> = [member, spare].into_iter().collect();
    let leaf = Node::parse(&node.blocks[&member]).expect("a leaf");
    let keys: Vec<Vec<u8>> = (0..leaf.len()).map(|i| leaf.key(i)).collect();

    // The reader, cold.
    let mut r = Page::new(Params::default(), PutPath::Page);
    let mut gets = Vec::new();
    drive(&mut r, &mut node, &silent, &mut now, 500, &mut gets);
    for (n, key) in keys.iter().enumerate() {
        r.event(Event::Get {
            client: ClientId(2),
            req_id: engine::read::ReqId(n as u64),
            key: key.clone(),
        });
    }
    let mut answered = 0;
    let mut answered_at = None;
    for _ in 0..60 {
        drive(&mut r, &mut node, &silent, &mut now, 1_000, &mut gets);
        answered += r
            .take_client()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        client: ClientId(2),
                        ..
                    }
                )
            })
            .count();
        if answered == keys.len() && answered_at.is_none() {
            answered_at = Some(now);
        }
    }
    let answered_at = answered_at
        .unwrap_or_else(|| panic!("{answered} of {} reads answered in 60 s", keys.len()));
    let spare_gets = gets.iter().filter(|(_, id)| *id == spare).count();
    // A GET the RTO had already timed out may go once more before the engine withdrew it; after that, none.
    let late: Vec<u64> = gets
        .iter()
        .filter(|(t, id)| *id == spare && *t > answered_at + 1_000)
        .map(|(t, _)| t - answered_at)
        .collect();
    println!("  {} reads answered; the unneeded silent parity block GET {spare_gets} times, {} of them after the reads were answered: {late:?} ms", keys.len(), late.len());
    assert!(
        late.is_empty(),
        "a withdrawn GET was asked again {} times after its race finished: {late:?} ms after",
        late.len()
    );
}
