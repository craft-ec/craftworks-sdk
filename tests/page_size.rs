//! How many round trips a range costs, COUNTED on the wire.
//!
//! `max_entries: 0` reads like "no limit" and is not one: the delegate shell
//! clamps with `clamp(1, MAX_PAGE_ENTRIES)`, so 0 becomes ONE entry and a
//! domain of N rows loads in N round trips of a single row. At roughly 15 ms
//! per delegate call (F32) a thousand rows is fifteen seconds.
//!
//! **The first version of this file could not see that.** It took the page
//! size as a parameter, applied its OWN copy of the shell's clamp, and
//! hand-fed `Loads::on_page`. It was arithmetic that agreed with itself: put
//! both loaders back to `0` and it stayed green. A test that re-implements
//! what it checks agrees with itself.
//!
//! So this drives the REAL path — a `CachedStore` whose loads go through
//! `Loads::range_request`, against a real `Shell` over an in-memory store —
//! and counts the `Request::Range` messages that actually crossed.

use craftworks_sdk::store::Store as _;
use craftworks_sdk::{CachedStore, Loads};
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// One head for every page: these tests do not move the tree, and a
/// constant says so rather than leaving it to be inferred.
const AT: protocol::At = protocol::At {
    seq: 1,
    root: [1u8; 32],
};

#[derive(Clone, Default)]
struct BlockStore(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for BlockStore {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl BlockStore {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

/// A real shell, rebuilt from its context between every exchange — which is
/// what a delegate does — plus a COUNT of the range requests it was asked.
struct Node {
    store: BlockStore,
    ctx: Vec<u8>,
    /// Every `Request::Range` that crossed. This is the measurement.
    ranges: usize,
}

impl Node {
    fn new() -> Node {
        Node {
            store: BlockStore::default(),
            ctx: Vec::new(),
            ranges: 0,
        }
    }

    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let mut shell: Shell<BlockStore> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.store.clone(),
            StoreFacts::provisioned(),
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
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

    /// Hand the shell one client request, counting it if it is a range.
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        if let protocol::Incoming::Ok(env) = protocol::decode_request(request) {
            if matches!(env.body, protocol::Request::Range { .. }) {
                self.ranges += 1;
            }
        }
        self.step(vec![Inbound::Client(request.to_vec())])
    }
}

/// Load `[lo, hi)` into a `CachedStore` the way the browser session does,
/// and return how many range requests it took.
///
/// The loop is the sessions: ask, follow the cursor under the same ticket,
/// stop when the range is exhausted.
fn round_trips_to_load(rows: usize) -> (usize, usize) {
    let mut node = Node::new();
    let mut store = CachedStore::new(Box::new(|| 0));

    // Put `rows` records in the node's tree, through the real engine.
    for i in 0..rows {
        let key = format!("d\0note\0{i:06}").into_bytes();
        store.put(&key, b"v");
        for frame in store.take_outbound() {
            for reply in node.exchange(&frame) {
                store.on_inbound(&reply);
            }
        }
    }
    // Did every write actually land? A load measured over a truncated tree
    // is a measurement of a different range than the one this test names.
    println!(
        "  wrote {rows}, refused {}, pending {:?}",
        store.refused.len(),
        store.copy.pending()
    );
    node.ranges = 0;

    // A fresh client: nothing loaded, so the range must be fetched.
    let mut fresh = CachedStore::new(Box::new(|| 0));
    let mut loads = Loads::new();
    let (lo, hi) = (b"d\0note\0".to_vec(), b"d\0note\x01".to_vec());
    let (id, _) = loads.want(&lo, &hi, 0).expect("a fresh span");

    let send = |s: &mut CachedStore, req: &protocol::Request, node: &mut Node| {
        s.client.send(req);
        let mut replies = Vec::new();
        for frame in s.take_outbound() {
            replies.extend(node.exchange(&frame));
        }
        replies
    };

    let mut pending = Some(Loads::range_request(id, &lo, &hi, None));
    let mut delivered = 0usize;
    let mut guard = 0;
    while let Some(req) = pending.take() {
        guard += 1;
        assert!(guard < 5_000, "it did not converge");
        let replies = send(&mut fresh, &req, &mut node);
        for reply in replies {
            if let Ok(protocol::Reply::Page {
                req_id,
                entries,
                cursor,
                ..
            }) = protocol::decode_reply(&reply)
            {
                match loads.on_page(req_id, entries, cursor, AT) {
                    craftworks_sdk::loads::Page::More { lo, hi, after } => {
                        pending = Some(Loads::range_request(req_id, &lo, &hi, Some(after)));
                    }
                    craftworks_sdk::loads::Page::Complete { lo, hi, rows } => {
                        delivered = rows.len();
                        fresh.on_page(&lo, &hi, rows, [0u8; 32]);
                    }
                    // The tree moved under this load: ask again from the
                    // top of the range.
                    craftworks_sdk::loads::Page::Restart { lo, hi } => {
                        pending = Some(Loads::range_request(req_id, &lo, &hi, None));
                    }
                    craftworks_sdk::loads::Page::Nothing => {}
                }
            }
        }
    }
    (node.ranges, delivered)
}

/// **A 300-row domain loads in 2 pages**, counted on the wire.
#[test]
fn a_domain_of_300_rows_loads_in_two_round_trips() {
    let (trips, delivered) = round_trips_to_load(300);
    // THE ROWS ACTUALLY ARRIVED. Without this, "2 round trips" is also what
    // a load that fetched a fraction of the domain would report, and the
    // test's name would be a claim about a number it never checked.
    assert_eq!(
        delivered, 300,
        "the load delivered {delivered} of 300 rows, so the trip count is \
         about a different range than the one this test names"
    );
    assert_eq!(
        trips,
        2,
        "a 300-row domain took {trips} round trips. At ~15 ms per delegate \
         call that is {} ms of waiting for data that fits in two pages",
        trips * 15
    );
}

/// THE CONTROL: the loader really is what decides this.
///
/// The same range, asked for with `0` — which is what both loaders used to
/// send — costs one round trip per row. Without this, "2 pages" is a number
/// with nothing to compare it to, and a loader put back to `0` would leave
/// the test above green.
#[test]
fn control_asking_with_zero_costs_one_round_trip_per_row() {
    let mut node = Node::new();
    let mut store = CachedStore::new(Box::new(|| 0));
    for i in 0..20 {
        let key = format!("d\0note\0{i:06}").into_bytes();
        store.put(&key, b"v");
        for frame in store.take_outbound() {
            for reply in node.exchange(&frame) {
                store.on_inbound(&reply);
            }
        }
    }
    node.ranges = 0;

    let (lo, hi) = (b"d\0note\0".to_vec(), b"d\0note\x01".to_vec());
    let mut after: Option<Vec<u8>> = None;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 200, "it did not converge");
        // `0`, deliberately: the value the loaders used to send.
        let req = protocol::Request::Range {
            req_id: 1,
            lo: protocol::Bound::Included(lo.clone()),
            hi: protocol::Bound::Excluded(hi.clone()),
            reverse: false,
            after: after.clone(),
            max_entries: 0,
        };
        store.client.send(&req);
        let mut done = true;
        for frame in store.take_outbound() {
            for reply in node.exchange(&frame) {
                if let Ok(protocol::Reply::Page {
                    cursor: Some(c), ..
                }) = protocol::decode_reply(&reply)
                {
                    after = Some(c);
                    done = false;
                }
            }
        }
        if done {
            break;
        }
    }
    // TWENTY-ONE for twenty rows, and the extra one is not noise: with a
    // page size of one every page is FULL, so the engine keeps returning a
    // cursor until an empty page confirms the end. Asking for a real page
    // avoids that too — 300 rows take 2 trips and not 3, because the second
    // page is short and a short page IS the end.
    println!("  ranges with max_entries=0 over 20 rows: {}", node.ranges);
    assert_eq!(
        node.ranges, 21,
        "`0` no longer costs a round trip per row; if the shell's clamp \
         changed, this test and MAX_PAGE_ENTRIES' documentation both need \
         revisiting"
    );
}

/// The constant the loaders send survives the SHELL'S OWN clamp.
///
/// Asked through the shell rather than through a copy of its arithmetic: a
/// test that re-implements what it checks agrees with itself.
#[test]
fn the_page_constant_is_not_clamped_down_by_the_real_shell() {
    let mut node = Node::new();
    let mut store = CachedStore::new(Box::new(|| 0));
    // One row is enough: the reply reports the page size the shell USED.
    store.put(b"d\x00note\x00000001", b"v");
    for frame in store.take_outbound() {
        for reply in node.exchange(&frame) {
            store.on_inbound(&reply);
        }
    }
    let (lo, hi) = (b"d\0note\0".to_vec(), b"d\0note\x01".to_vec());
    store.client.send(&Loads::range_request(1, &lo, &hi, None));
    let mut used = None;
    for frame in store.take_outbound() {
        for reply in node.exchange(&frame) {
            if let Ok(protocol::Reply::Page { max_entries, .. }) = protocol::decode_reply(&reply) {
                used = Some(max_entries);
            }
        }
    }
    assert_eq!(
        used,
        Some(protocol::MAX_PAGE_ENTRIES),
        "the shell used a different page size than the loaders ask for, so \
         the two disagree about what a full page is"
    );
}
