//! The SDK talking to a real engine, with no node and no browser.
//!
//! The transport is a loopback into an actual `Shell` driven against an
//! in-memory store: the same protocol bytes, the same shell, the same core.
//! What is missing is only the network — which is the part the live run
//! covers and the part that cannot be made to answer `Busy` on demand.

use craftworks_sdk::{EngineStore, Transport};
use engine_delegate::shell::{Inbound, Shell};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use protocol::{Bound, WriteState};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The node's block store.
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

/// A loopback to a real shell, rebuilt from its context between every
/// exchange — which is what a delegate does.
struct Loop {
    store: Store,
    ctx: Vec<u8>,
    /// Answer the next N writes `Busy` before letting any through, so the
    /// outbox is actually exercised rather than merely present.
    busy_for: usize,
}

impl Loop {
    fn new(busy_for: usize) -> Loop {
        Loop {
            store: Store::default(),
            ctx: Vec::new(),
            busy_for,
        }
    }

    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let mut shell: Shell<Store> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.store.clone(),
            true,
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
        assert_eq!(
            out.stranded, 0,
            "the shell stranded effects, which are lost"
        );
        // The node does what the ops ask.
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.store.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = self.store.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    next.push(Inbound::GotHead { seq, root })
                }
                engine_delegate::schedule::Op::ReadHead { .. } => next.push(Inbound::NoHead),
            }
        }
        let mut replies = out.replies;
        if !next.is_empty() {
            replies.extend(self.step(next));
        }
        replies
    }
}

impl Transport for Loop {
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        // Refuse the first `busy_for` writes, whatever they are. A real
        // engine does this whenever a commit is already in flight.
        if self.busy_for > 0 {
            if let protocol::Incoming::Ok(env) = protocol::decode_request(request) {
                if let protocol::Request::Write { write_id, .. } = env.body {
                    self.busy_for -= 1;
                    return vec![protocol::encode_reply(&protocol::Reply::WriteState {
                        write_id,
                        state: WriteState::Busy,
                    })];
                }
            }
        }
        self.step(vec![Inbound::Client(request.to_vec())])
    }
}

fn started(busy_for: usize) -> EngineStore<Loop> {
    let mut s = EngineStore::new(Loop::new(busy_for));
    s.identity().expect("the engine answers who it is");
    s
}

/// A write through the SDK reaches the engine and reads back.
#[test]
fn a_write_through_the_sdk_is_readable_through_the_sdk() {
    use craftworks_sdk::Store as _;
    let mut db = started(0);
    db.put(b"k/one", b"first");
    assert_eq!(
        db.read_mut(b"k/one").as_deref(),
        Some(&b"first"[..]),
        "a write the SDK made is not readable through the same engine"
    );
    assert_eq!(db.waiting(), 0, "the write never left the outbox");
}

/// Writes refused `Busy` still arrive, in order, exactly once — through the
/// SDK, against a real shell.
///
/// The unit tests prove the outbox's logic. This proves it is WIRED: a
/// correct outbox nobody calls behaves exactly like no outbox at all.
#[test]
fn writes_refused_busy_still_arrive_through_the_sdk() {
    use craftworks_sdk::Store as _;
    let mut db = started(6);
    for i in 0..4u32 {
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes());
    }
    assert!(
        db.resubmits > 0,
        "nothing was ever re-submitted, so the engine never said Busy and \
         this test is not about the outbox"
    );
    for i in 0..4u32 {
        assert_eq!(
            db.read_mut(format!("k/{i:02}").as_bytes()).as_deref(),
            Some(format!("v{i}").as_bytes()),
            "k/{i:02} did not survive being refused Busy"
        );
    }
    assert_eq!(db.waiting(), 0, "writes are still queued");
    println!(
        "  4 writes through {} Busy answers, all readable",
        db.resubmits
    );
}

/// A LIST over the new connection — what a table or list component does.
#[test]
fn a_range_reads_back_every_row_in_order_and_both_ways() {
    use craftworks_sdk::Store as _;
    let mut db = started(0);
    for i in 0..12u32 {
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes());
    }

    let rows = db.list(Bound::Unbounded, Bound::Unbounded, false);
    let keys: Vec<String> = rows
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    let expect: Vec<String> = (0..12).map(|i| format!("k/{i:02}")).collect();
    assert_eq!(
        keys, expect,
        "a forward range did not return every row in order"
    );

    let back = db.list(Bound::Unbounded, Bound::Unbounded, true);
    let back_keys: Vec<String> = back
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    let mut reversed = expect.clone();
    reversed.reverse();
    assert_eq!(
        back_keys, reversed,
        "a reverse range is not the forward one backwards"
    );

    // A bounded range.
    let some = db.list(
        Bound::Included(b"k/03".to_vec()),
        Bound::Excluded(b"k/07".to_vec()),
        false,
    );
    assert_eq!(
        some.len(),
        4,
        "a bounded range returned {} rows",
        some.len()
    );
    println!("  12 rows listed forward, backward, and bounded");
}

/// The page size is CLAMPED, and the clamp is reported.
///
/// A caller that asked for more than a page can bear and got fewer rows must
/// be able to tell a short page from the end of the range — otherwise a list
/// silently truncates.
#[test]
fn an_oversized_page_request_is_clamped_and_says_so() {
    use craftworks_sdk::Store as _;
    let mut db = started(0);
    for i in 0..12u32 {
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes());
    }
    let asked = 100_000u32;
    let p = db.page(Bound::Unbounded, Bound::Unbounded, false, None, asked);
    assert!(
        p.page_size_used < asked,
        "a request for {asked} rows reported {} as used, so nothing was \
         clamped and a caller cannot learn what it will actually get",
        p.page_size_used
    );
    assert!(p.page_size_used > 0);
    // The control: a SMALL request is not clamped, so the clamp is the size
    // doing it rather than a constant reply.
    let small = db.page(Bound::Unbounded, Bound::Unbounded, false, None, 3);
    assert_eq!(
        small.page_size_used, 3,
        "a request for 3 rows was also clamped, so the report is not about \
         the request at all"
    );
    assert_eq!(
        small.entries.len(),
        3,
        "the clamp was reported but not applied"
    );
    assert!(
        small.cursor.is_some(),
        "a partial page gave no cursor to continue from"
    );
    println!(
        "  asked {asked} -> used {}; asked 3 -> used 3",
        p.page_size_used
    );
}
