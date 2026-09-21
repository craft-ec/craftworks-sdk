//! sdk#162: a context over its bound was SILENTLY NOT SAVED -- the node kept
//! the previous call's context while this call's effects had already gone,
//! and the request was told nothing, ever (the architect's executed probes).
//!
//! Here: default params, a real `Shell` per call, entry.rs's save rule, and a
//! slow node (nothing answered until the end). The context is driven TO its
//! bound with parked cold reads, then one more request arrives. It must be
//! refused BY NAME, the context must save on every call, and once the node
//! answers, a trailing write must publish.
use engine::Params;
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use protocol::{Reply, Request, WriteState};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone, Default)]
struct Store(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Store {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

fn answer(node: &Store, ops: Vec<engine_delegate::schedule::Op>) -> Vec<Inbound> {
    use engine_delegate::schedule::Op;
    ops.into_iter()
        .map(|op| match op {
            Op::Put { id, bytes } => {
                node.put(id, &bytes);
                Inbound::PutAcked { id, ok: true }
            }
            Op::Get { id, .. } => Inbound::GotState {
                id,
                bytes: node.get(&id).map(|b| b.to_vec()),
            },
            Op::Head { seq, root } => Inbound::GotHead { seq, root },
            Op::ReadHead { .. } => Inbound::NoHead,
        })
        .collect()
}

fn frame(r: &Request) -> Inbound {
    Inbound::Client(protocol::encode_request(protocol::CURRENT, r).expect("encodes"))
}

/// What each call told, by request: `w<id>` a write state, `r<id>` a read's
/// answer (a page or unavailable).
fn heard(replies: &[Vec<u8>], into: &mut BTreeMap<String, Vec<String>>) {
    for b in replies {
        match protocol::decode_reply(b) {
            Ok(Reply::WriteState { write_id, state }) => into
                .entry(format!("w{write_id}"))
                .or_default()
                .push(format!("{state:?}")),
            Ok(Reply::Page { req_id, .. }) => into
                .entry(format!("r{req_id}"))
                .or_default()
                .push("Page".into()),
            Ok(Reply::Unavailable { req_id, .. }) => into
                .entry(format!("r{req_id}"))
                .or_default()
                .push("Unavailable".into()),
            _ => {}
        }
    }
}

#[test]
fn a_request_that_would_overflow_the_context_is_refused_by_name_and_every_call_saves() {
    let p = Params::default();
    let node = Store::default();
    // A published history, so a cold delegate has something to be cold about.
    let mut ctx: Vec<u8> = Vec::new();
    let mut inbound = vec![frame(&Request::Write {
        write_id: 1,
        ops: (0..400u32)
            .map(|i| protocol::Op::Put(format!("h/{i:04}").into_bytes(), vec![i as u8; 200]))
            .collect(),
    })];
    for _ in 0..400 {
        let mut s: Shell<Store> =
            Shell::resume_with(&ctx, p, node.clone(), StoreFacts::provisioned());
        let out = s.handle(std::mem::take(&mut inbound));
        ctx = s.to_context().expect("the history saves");
        inbound = answer(&node, out.ops);
        if inbound.is_empty() {
            break;
        }
    }

    // THE DELEGATE, COLD, behind a SLOW node: nothing is answered until the end.
    let cold = Store::default();
    let mut owed: Vec<Inbound> = Vec::new();
    let mut unsaved = 0;
    let mut biggest = 0;
    let mut told: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut call = |ctx: &mut Vec<u8>, inbound: Vec<Inbound>, owed: &mut Vec<Inbound>| {
        let mut s: Shell<Store> =
            Shell::resume_with(ctx, p, cold.clone(), StoreFacts::provisioned());
        // A parked write asks for its whole path at once, which strands past
        // max_gets (W3's defect, sdk#174/PR 5's). Room for it here, so this
        // sees only the context bound -- as the architect's probe did.
        s.limits.max_gets = 1000;
        let out = s.handle(inbound);
        match s.to_context() {
            Some(c) => {
                biggest = biggest.max(c.len());
                *ctx = c;
            }
            // entry.rs: NOT saved means the OLD context stays.
            None => unsaved += 1,
        }
        heard(&out.replies, &mut told);
        owed.extend(answer(&node, out.ops));
    };

    // 1. Park cold reads with long bounds -- 900-byte keys, the architect's
    //    reach -- far more than fit.
    let pad = |c: u8| vec![c; 900];
    for i in 0..400u64 {
        let mut lo = b"h/0000".to_vec();
        lo.extend(pad(b'a'));
        let mut hi = b"h/9999".to_vec();
        hi.extend(pad(b'z'));
        call(
            &mut ctx,
            vec![frame(&Request::Range {
                req_id: 10_000 + i,
                lo: protocol::Bound::Included(lo),
                hi: protocol::Bound::Excluded(hi),
                reverse: false,
                after: None,
                max_entries: 10,
            })],
            &mut Vec::new(), // the slow node: these are never answered
        );
    }
    // 2. One more request: a write that parks on the cold path.
    let w = Request::Write {
        write_id: 2,
        ops: (0..60u32)
            .map(|i| {
                let mut v = vec![0u8; 1000];
                v[..4].copy_from_slice(&i.to_le_bytes());
                protocol::Op::Put(format!("h/{:04}x", i * 6).into_bytes(), v)
            })
            .collect(),
    };
    call(&mut ctx, vec![frame(&w)], &mut owed);
    // 3. The node answers what was asked, and time passes.
    let mut rounds = 0;
    while !owed.is_empty() && rounds < 2000 {
        let one = owed.remove(0);
        if let Inbound::GotState { id, bytes: Some(b) } = &one {
            cold.put(*id, b);
        }
        call(&mut ctx, vec![one], &mut owed);
        rounds += 1;
    }
    for k in 1..=70u64 {
        call(
            &mut ctx,
            vec![frame(&Request::Tick {
                now: 1_790_000_000 + k,
            })],
            &mut owed,
        );
    }
    // 4. A trailing write: the engine is open.
    let trailing = Request::Write {
        write_id: 3,
        ops: vec![protocol::Op::Put(b"z/after".to_vec(), b"x".to_vec())],
    };
    call(&mut ctx, vec![frame(&trailing)], &mut owed);
    while !owed.is_empty() && rounds < 4000 {
        let one = owed.remove(0);
        call(&mut ctx, vec![one], &mut owed);
        rounds += 1;
    }

    let refused_reads = told
        .values()
        .filter(|v| v.iter().any(|s| s == "Unavailable"))
        .count();
    println!(
        "  unsaved calls {unsaved}; biggest context {biggest} of {} B; reads refused Unavailable {refused_reads}; write 2 told {:?}; write 3 told {:?}",
        p.max_context_bytes,
        told.get("w2"),
        told.get("w3")
    );
    assert_eq!(
        unsaved, 0,
        "{unsaved} call(s) ended with a context that could not be saved"
    );
    assert!(biggest <= p.max_context_bytes);
    assert!(
        refused_reads > 0,
        "no read was refused: the context was never driven to its bound"
    );
    let w2 = told.get("w2").cloned().unwrap_or_default();
    assert!(
        !w2.is_empty(),
        "the write that met a full context was told NOTHING"
    );
    let w3 = told.get("w3").cloned().unwrap_or_default();
    assert!(
        w3.iter().any(|s| s == "Published"),
        "the trailing write did not publish: {w3:?}"
    );
}
