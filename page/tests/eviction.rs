//! THE PAGE EVICTS (sdk#411): past `Params::max_page_block_bytes`, at the end of a page step, the least recently
//! used blocks the ONE pin rule (`Engine::pins`) does not pin are dropped. Each test runs at a 1-byte budget, so an
//! eviction pass runs after every step and only pins keep anything: a class whose pin is removed (its mutant,
//! "unpinned") loses its bytes and the test goes red.

use engine::{ClientId, Op as WriteOp, Params, WriteId};
use page::{Answer, Label, Ms, Op, Page, PutPath};

const KEY: [u8; 32] = [7u8; 32];

/// A block PUT on the wire: its id and bytes.
type Put = ([u8; 32], Vec<u8>);

fn tiny() -> Params {
    Params { max_page_block_bytes: 1, ..Params::default() }
}

/// A page (1-byte budget) with one write, driven until its Sign is answered: every block PUT is sent and NONE is
/// answered, the head read found no head. The page, and the block PUTs on the wire (id, bytes).
fn with_puts_out() -> (Page, Vec<Put>) {
    let mut p = Page::new(tiny(), PutPath::Page);
    p.write(ClientId(1), WriteId(1), vec![(b"k".to_vec(), WriteOp::Put(b"v".to_vec()))]);
    let mut puts = Vec::new();
    for _ in 0..20 {
        let ops = p.take_ops();
        if ops.is_empty() {
            break;
        }
        for op in ops {
            match op {
                Op::ReadHead { label: Label::Head } => p.answer(Answer::Head { label: Label::Head, read: None }, Ms(10)),
                Op::Put { id, bytes } => puts.push((id, bytes)),
                other => panic!("unexpected before any PUT is answered: {other:?}"),
            }
        }
    }
    assert!(!puts.is_empty(), "THE SETUP: no block PUT went out");
    (p, puts)
}

/// **C, in flight:** the pending commit's blocks, none acked, at a 1-byte budget. The node REFUSES one PARITY block
/// (permanently): the engine re-puts it FROM THE STORE (`on_failed`), so it must still be there. Mutant "C
/// unpinned" -> the bytes were evicted -> no re-put, `reput_missing` 1 -> red.
#[test]
fn an_in_flight_block_refused_is_put_again_from_the_store_after_eviction_passes() {
    let (mut p, puts) = with_puts_out();
    // A PARITY block: data blocks are also the warm apply's (W pins them too); parity is only ever C.
    let (id, bytes) = puts.iter().find(|(id, b)| freenet_prolly::block_id(freenet_prolly::kind::PARITY, b) == *id).cloned().expect("THE SETUP: no parity PUT in the first wave");
    p.answer(Answer::PutRefused { id, transient: false }, Ms(20));
    let again: Vec<Vec<u8>> = p.take_ops().into_iter().filter_map(|o| match o { Op::Put { id: x, bytes } if x == id => Some(bytes), _ => None }).collect();
    assert_eq!(p.reput_missing(), 0, "a pending commit's refused block had its bytes evicted: it can never be put again");
    assert_eq!(again, vec![bytes], "the refused block was not put again, with its bytes, from the store");
    assert!(p.blocks().stats().peak_pinned_bytes > 0, "THE SETUP: no eviction pass ran over pinned blocks, so this tested nothing");
}

fn is_parity(id: &[u8; 32], bytes: &[u8]) -> bool {
    freenet_prolly::block_id(freenet_prolly::kind::PARITY, bytes) == *id
}

/// A page (1-byte budget) whose one write is PUBLISHED with ONE first-wave parity block never acked (the
/// straggler): every other PUT acked, signed, landed, read back. The page and the straggler (id, bytes).
fn published_with_a_straggler() -> (Page, Put) {
    let (mut p, puts) = with_puts_out();
    let straggler = puts.iter().find(|(id, b)| is_parity(id, b)).cloned().expect("THE SETUP: no first-wave parity");
    for (id, _) in &puts {
        if *id != straggler.0 {
            p.answer(Answer::PutOk(*id), Ms(20));
        }
    }
    let sk = ed25519_dalek::SigningKey::from_bytes(&KEY);
    let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    let mut mine = None;
    for _ in 0..40 {
        for op in p.take_ops() {
            match op {
                Op::Sign { id, seq, root, ledger, .. } => {
                    let value = [root.as_slice(), &ledger].concat();
                    let record = contract_keys::register::head_state(&params, &sk.to_bytes(), seq, &value).expect("signs");
                    mine = page::HeadRead::from_record(&record);
                    p.answer(Answer::Signer { id, answer: signer_proto::Answer::Signed(record) }, Ms(30));
                }
                Op::Update { label: Label::Head, .. } => p.answer(Answer::Updated { label: Label::Head }, Ms(31)),
                Op::ReadHead { label: Label::Head } => p.answer(Answer::Head { label: Label::Head, read: mine.clone() }, Ms(32)),
                Op::Put { id, .. } if id != straggler.0 => p.answer(Answer::PutOk(id), Ms(33)),
                _ => {}
            }
        }
    }
    let mine = mine.expect("THE SETUP: the head was never signed");
    assert_eq!(p.published(), (mine.seq, mine.root()), "THE SETUP: the write was not published with its straggler unacked");
    (p, straggler)
}

/// **B, backing:** a PUBLISHED commit whose straggler is not yet acked (`backing.remaining`), at a 1-byte budget.
/// The node refuses the straggler: the engine re-puts it FROM THE STORE, so it must still be there -- only B holds
/// it now (no commit is pending, no write queued). Mutant "B unpinned" -> evicted -> no re-put -> red.
#[test]
fn a_published_commits_straggler_refused_is_put_again_from_the_store_after_eviction_passes() {
    let (mut p, (id, bytes)) = published_with_a_straggler();
    let _ = p.take_ops();
    p.answer(Answer::PutRefused { id, transient: false }, Ms(40));
    let again: Vec<Vec<u8>> = p.take_ops().into_iter().filter_map(|o| match o { Op::Put { id: x, bytes } if x == id => Some(bytes), _ => None }).collect();
    assert_eq!(p.reput_missing(), 0, "a backing straggler had its bytes evicted: it can never be put again");
    assert_eq!(again, vec![bytes], "the refused straggler was not put again, with its bytes, from the store");
    assert!(p.blocks().stats().evicted > 0, "THE SETUP: nothing was evicted, so no pass ran over unpinned blocks here");
}

/// **W, warm-only:** write 1's commit is pending (its PUTs unanswered) and write 2 is QUEUED behind it: write 2's
/// warm-apply blocks (`Effect::Keep`) are the only copy anywhere -- in no commit, on no node. Their reader is the
/// page's own walk of the WARM root (a key's `Saving` state, `Server::key_state`): at a 1-byte budget it still reads
/// write 2's value from them. Mutant "W unpinned" -> evicted -> the walk needs a block no node has -> red.
#[test]
fn a_queued_writes_warm_blocks_still_answer_the_warm_walk_after_eviction_passes() {
    let (mut p, _) = with_puts_out();
    p.write(ClientId(1), WriteId(2), vec![(b"k2".to_vec(), WriteOp::Put(b"v2".to_vec()))]);
    let _ = p.take_ops();
    let warm = p.warm_root();
    assert_ne!(warm, p.published().1, "THE SETUP: the warm root is the published one (write 2 did not warm-apply)");
    let walked = p.walk(&warm, &engine::read::Walk::Get(b"k2".to_vec()));
    assert_eq!(walked, engine::read::Walked::Done(engine::read::ReadResult::Value(Some(b"v2".to_vec()))), "the warm walk did not read a queued write's value from its warm-apply blocks");
    assert!(p.blocks().stats().peak_pinned_bytes > 0, "THE SETUP: no eviction pass ran over pinned blocks");
}

/// **EVICTION WORK IS BOUNDED under a store that is all pinned** (main, on the 40k-write probe that never finished):
/// a pass that cannot get under the budget re-arms only after the store grows by budget/16, so 100 steps with
/// nothing droppable scan the store about ONCE, not 100 times. Judged on the count, never on a timeout. Mutant "scan
/// every step" (no re-arm) -> ~100 x the blocks -> red.
#[test]
fn a_fully_pinned_store_over_budget_is_not_rescanned_every_step() {
    let (mut p, _) = with_puts_out();
    for w in 2..=6u64 {
        p.write(ClientId(1), WriteId(w), vec![(format!("k{w}").into_bytes(), WriteOp::Put(vec![w as u8; 2_000]))]);
    }
    let _ = p.take_ops();
    let blocks = p.blocks().len() as u64;
    let before = p.blocks().stats().scanned;
    assert!(p.blocks().bytes() > 2, "THE SETUP: the store is not over its 1-byte budget");
    for t in 1..=100u64 {
        p.tick(Ms(100 + t));
        let _ = p.take_ops();
    }
    let scanned = p.blocks().stats().scanned - before;
    println!("fully pinned: {blocks} blocks, {} bytes; 100 steps scanned {scanned} blocks; evicted {}", p.blocks().bytes(), p.blocks().stats().evicted);
    assert!(blocks > 0, "THE SETUP: an empty store");
    assert!(scanned <= 2 * blocks, "100 steps over a fully pinned store scanned {scanned} blocks ({blocks} held): eviction re-scans every step");
}
