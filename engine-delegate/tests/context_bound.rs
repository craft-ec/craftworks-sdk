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
use protocol::{Reply, Request};
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
    Inbound::Client(protocol::encode_session_request(protocol::CURRENT, protocol::mint_session(0x5e55_1017), r).expect("encodes"))
}

/// What each call told, by request: `w<id>` a write state, `r<id>` a read's
/// answer (a page or unavailable).
fn heard(replies: &[Vec<u8>], into: &mut BTreeMap<String, Vec<String>>) {
    for b in replies {
        match protocol::decode_reply(b) {
            // Both shapes: this client is sessioned (sdk#194).
            Ok(Reply::WriteState { write_id, state } | Reply::SessionWriteState { write_id, state, .. }) => into
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

/// THE SHELL'S SHARE, at the same time as the engine at its limit (sdk#162
/// review). The engine keeps itself under `max_context_bytes` less
/// `shell_context_reserve`; the shell's own state rides on top, and the WHOLE
/// is what must save. Here the shell carries read-backs for a whole commit
/// whose puts were acknowledged but whose blocks the delegate has not seen
/// (a slow node never answers the read-backs), and cold reads are parked
/// until the engine sheds. Every call must save. And the shed ORDER within
/// one client is pinned: the reads refused are exactly the NEWEST ones.
#[test]
fn the_shell_at_its_worst_beside_the_engine_at_its_limit_still_saves_and_sheds_the_newest() {
    let p = Params::default();
    let node = Store::default();
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
    let cold = Store::default();
    let unsaved = std::cell::Cell::new(0usize);
    let most_awaiting = std::cell::Cell::new(0usize);
    let told: RefCell<BTreeMap<String, Vec<String>>> = RefCell::new(BTreeMap::new());
    let mut put_ids = std::collections::BTreeSet::new();
    let call = |ctx: &mut Vec<u8>, inbound: Vec<Inbound>| -> Vec<engine_delegate::schedule::Op> {
        let mut s: Shell<Store> =
            Shell::resume_with(ctx, p, cold.clone(), StoreFacts::provisioned());
        s.limits.max_gets = 1000; // the parked write's path; see the first test
        let out = s.handle(inbound);
        most_awaiting.set(most_awaiting.get().max(s.awaiting()));
        match s.to_context() {
            Some(c) => *ctx = c,
            None => unsaved.set(unsaved.get() + 1),
        }
        heard(&out.replies, &mut told.borrow_mut());
        out.ops
    };
    // 1. A write of ~60 blocks (under the parked-write cap, so it parks and
    //    applies), on the cold path: its path GETs are
    //    answered, its PUTs acknowledged -- but the delegate never sees the
    //    blocks (the read-backs are the slow node's to answer, and it does not).
    let w = Request::Write {
        write_id: 2,
        ops: (0..60u32)
            .map(|i| {
                let mut v = vec![0u8; 2048];
                v[..4].copy_from_slice(&i.to_le_bytes());
                protocol::Op::Put(format!("w/{i:04}").into_bytes(), v)
            })
            .collect(),
    };
    let mut queue: Vec<Inbound> = vec![frame(&w)];
    for _ in 0..400 {
        if queue.is_empty() {
            break;
        }
        let ops = call(&mut ctx, std::mem::take(&mut queue));
        for op in ops {
            use engine_delegate::schedule::Op;
            match op {
                Op::Put { id, bytes } => {
                    node.put(id, &bytes);
                    put_ids.insert(id);
                    queue.push(Inbound::PutAcked { id, ok: true });
                }
                // A read-back of the commit's own block: never answered.
                Op::Get { id, .. } if put_ids.contains(&id) => {}
                Op::Get { id, .. } => {
                    let bytes = node.get(&id).map(|b| b.to_vec());
                    if let Some(b) = &bytes {
                        cold.put(id, b);
                    }
                    queue.push(Inbound::GotState { id, bytes });
                }
                _ => {}
            }
        }
    }
    let most_awaiting = most_awaiting.get();
    assert!(
        most_awaiting >= 40,
        "the shell carried only {most_awaiting} read-backs: not at its worst"
    );
    // 2. Cold reads, never answered, until the engine sheds.
    let pad = |c: u8| vec![c; 900];
    for i in 0..400u64 {
        let mut lo = b"h/0000".to_vec();
        lo.extend(pad(b'a'));
        let mut hi = b"h/9999".to_vec();
        hi.extend(pad(b'z'));
        let _ = call(
            &mut ctx,
            vec![frame(&Request::Range {
                req_id: 10_000 + i,
                lo: protocol::Bound::Included(lo),
                hi: protocol::Bound::Excluded(hi),
                reverse: false,
                after: None,
                max_entries: 10,
            })],
        );
    }
    let unsaved = unsaved.get();
    let told = told.into_inner();
    let refused: Vec<u64> = told
        .iter()
        .filter(|(_, v)| v.iter().any(|s| s == "Unavailable"))
        .filter_map(|(k, _)| k.strip_prefix('r')?.parse().ok())
        .collect();
    let kept_max = (10_000..10_400u64).filter(|r| !refused.contains(r)).max();
    println!(
        "  shell read-backs {most_awaiting}; unsaved {unsaved}; {} reads refused, the oldest refused {:?}, the newest kept {kept_max:?}",
        refused.len(),
        refused.iter().min()
    );
    assert_eq!(
        unsaved, 0,
        "{unsaved} call(s) could not save: the engine left the shell no room"
    );
    assert!(
        !refused.is_empty(),
        "no read was refused: the engine never reached its limit"
    );
    assert!(
        refused.iter().min() > kept_max.as_ref(),
        "an OLDER read was refused while a newer one was kept"
    );
}

/// The shell's read-backs follow what the engine still WAITS ON (sdk#187
/// review). A commit too large to carry has its puts acknowledged but never
/// seen by the delegate, so it holds read-backs; settled from fact, it is
/// released Lost. Its read-backs must go with it -- left behind, they
/// outlived every Lost commit and the shell's share grew without bound.
#[test]
fn a_lost_commit_leaves_no_read_backs_in_the_shell() {
    let p = Params::default();
    let node = Store::default();
    let cold = Store::default(); // the delegate never sees what it put
    let mut ctx: Vec<u8> = Vec::new();
    let run = |ctx: &mut Vec<u8>,
                   inbound: Vec<Inbound>|
     -> (usize, Vec<Vec<u8>>, Vec<engine_delegate::schedule::Op>) {
        let mut s: Shell<Store> =
            Shell::resume_with(ctx, p, cold.clone(), StoreFacts::provisioned());
        let out = s.handle(inbound);
        *ctx = s.to_context().expect("saves");
        (s.awaiting(), out.replies, out.ops)
    };
    let _ = run(&mut ctx, vec![frame(&Request::Tick { now: 1_790_000_000 })]);
    // 30 KiB in one value: too large to carry, so a silent commit is Lost.
    let (_, _, ops) = run(
        &mut ctx,
        vec![frame(&Request::Write {
            write_id: 1,
            ops: vec![protocol::Op::Put(b"k/big".to_vec(), vec![9u8; 30 * 1024])],
        })],
    );
    let mut acks = Vec::new();
    for op in ops {
        if let engine_delegate::schedule::Op::Put { id, bytes } = op {
            node.put(id, &bytes);
            acks.push(Inbound::PutAcked { id, ok: true });
        }
    }
    let mut most = 0;
    for a in acks {
        let (n, _, _) = run(&mut ctx, vec![a]); // read-backs issued, never answered
        most = most.max(n);
    }
    assert!(most > 0, "no read-back was held: the test is empty");
    let mut lost = false;
    let mut left = most;
    for k in 1..=40u64 {
        let (n, replies, _) = run(
            &mut ctx,
            vec![frame(&Request::Tick {
                now: 1_790_000_000 + k,
            })],
        );
        left = n;
        lost |= replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .any(|r| {
                matches!(
                    r,
                    Reply::WriteState { write_id: 1, state: protocol::WriteState::Lost }
                        | Reply::SessionWriteState { write_id: 1, state: protocol::WriteState::Lost, .. }
                )
            });
        if lost {
            break;
        }
    }
    assert!(lost, "the silent commit was never released Lost");
    assert_eq!(left, 0, "{left} read-back(s) outlived the Lost commit");
}
