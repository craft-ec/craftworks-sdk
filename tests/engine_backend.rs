//! The SDK talking to a real engine, with no node and no browser.
//!
//! The transport is a loopback into an actual `Shell` driven against an
//! in-memory store: the same protocol bytes, the same shell, the same core.
//! What is missing is only the network — which is the part the live run
//! covers and the part that cannot be made to answer `Busy` on demand.

use craftworks_sdk::{EngineStore, Transport};
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
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
            StoreFacts::provisioned(),
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
                    // Named with the writer's session, as the delegate names a v4
                    // writer's verdict (sdk#146): an unnamed one is another tab's.
                    return vec![protocol::encode_reply(&protocol::Reply::SessionWriteState {
                        session: env.session,
                        write_id,
                        state: WriteState::Busy,
                    })
                    .expect("encodes")];
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

mod support;
use support::chaos::Chaos;

/// The same engine, behind a connection that reorders and duplicates.
fn rough(busy_for: usize, seed: u64) -> EngineStore<Chaos<Loop>> {
    let mut s = EngineStore::new(Chaos::new(Loop::new(busy_for), seed));
    s.identity().expect("the engine answers who it is");
    s
}

/// The same engine, behind a connection that perturbs NOTHING. The control.
fn calm(busy_for: usize) -> EngineStore<Chaos<Loop>> {
    let mut s = EngineStore::new(Chaos::calm(Loop::new(busy_for)));
    s.identity().expect("the engine answers who it is");
    s
}

/// A write through the SDK reaches the engine and reads back.
#[test]
fn a_write_through_the_sdk_is_readable_through_the_sdk() {
    use craftworks_sdk::Store as _;
    let mut db = started(0);
    db.put(b"k/one", b"first").expect("the store took the write");
    assert_eq!(
        db.read_mut(b"k/one").unwrap().as_deref(),
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
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes()).expect("the store took the write");
    }
    assert!(
        db.resubmits > 0,
        "nothing was ever re-submitted, so the engine never said Busy and \
         this test is not about the outbox"
    );
    for i in 0..4u32 {
        assert_eq!(
            db.read_mut(format!("k/{i:02}").as_bytes())
                .unwrap()
                .as_deref(),
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
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes()).expect("the store took the write");
    }

    let rows = db.list(Bound::Unbounded, Bound::Unbounded, false).unwrap();
    let keys: Vec<String> = rows
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    let expect: Vec<String> = (0..12).map(|i| format!("k/{i:02}")).collect();
    assert_eq!(
        keys, expect,
        "a forward range did not return every row in order"
    );

    let back = db.list(Bound::Unbounded, Bound::Unbounded, true).unwrap();
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
    let some = db
        .list(
            Bound::Included(b"k/03".to_vec()),
            Bound::Excluded(b"k/07".to_vec()),
            false,
        )
        .unwrap();
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
        db.put(format!("k/{i:02}").as_bytes(), format!("v{i}").as_bytes()).expect("the store took the write");
    }
    let asked = 100_000u32;
    let p = db
        .page(Bound::Unbounded, Bound::Unbounded, false, None, asked)
        .unwrap();
    assert!(
        p.page_size_used < asked,
        "a request for {asked} rows reported {} as used, so nothing was \
         clamped and a caller cannot learn what it will actually get",
        p.page_size_used
    );
    assert!(p.page_size_used > 0);
    // The control: a SMALL request is not clamped, so the clamp is the size
    // doing it rather than a constant reply.
    let small = db
        .page(Bound::Unbounded, Bound::Unbounded, false, None, 3)
        .unwrap();
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

/// One surface, both backends: `Reads` over the engine answers what `Reads`
/// over the in-memory tree answers.
///
/// This replaces a pair of tests that asserted the `&self` reads REFUSED by
/// name. They described a door that no longer exists: reads take `&mut self`
/// now, on every backend, so there is nothing to refuse and nothing for a
/// caller to reach through by mistake. Keeping tests that assert a refusal
/// would have been keeping tests that assert an implementation detail was
/// still wrong in the same way.
///
/// What is worth pinning is what the change bought: an app written against
/// this trait does not change when it moves from memory onto a node.
#[test]
fn the_engine_and_the_memory_backend_answer_the_same_reads() {
    use craftworks_sdk::{MemStore, Reads, Store as SdkStore};
    let mut engine = started(0);
    let mut mem = MemStore::default();

    for i in 0..12u32 {
        let (k, v) = (format!("k/{i:02}"), format!("v{i}"));
        SdkStore::put(&mut engine, k.as_bytes(), v.as_bytes()).expect("the store took the write");
        SdkStore::put(&mut mem, k.as_bytes(), v.as_bytes()).expect("the store took the write");
    }

    for i in 0..14u32 {
        let k = format!("k/{i:02}");
        assert_eq!(
            Reads::get(&mut engine, k.as_bytes()),
            Reads::get(&mut mem, k.as_bytes()),
            "the two backends disagree about {k}"
        );
    }
    assert_eq!(
        Reads::scan(&mut engine, b"k/03", b"k/07", false, usize::MAX),
        Reads::scan(&mut mem, b"k/03", b"k/07", false, usize::MAX),
        "the two backends disagree about a bounded range"
    );
    // The control: they are not agreeing by both being empty.
    assert_eq!(
        Reads::scan(&mut mem, b"k/03", b"k/07", false, usize::MAX)
            .unwrap()
            .len(),
        4,
        "the fixture produced no rows, so the agreement above is vacuous"
    );
    // And a root is a root on both, which is what a binding compares.
    assert!(Reads::root(&mut engine).is_ok());
    assert!(Reads::root(&mut mem).is_ok());
}

/// A read the engine COULD NOT ANSWER is not an absent key.
///
/// The two are one byte apart in a reply and a world apart in what an app
/// does with them: `Ok(None)` means "this key does not exist", and an app
/// shows an empty page or writes over the top. The error means "ask again".
#[test]
fn an_unanswerable_read_is_an_error_and_not_an_absence() {
    use craftworks_sdk::{Reads, StoreError};
    /// A transport that answers every read `Unavailable`, which is what an
    /// engine says when a block it needs is not to be had.
    struct Unreachable;
    impl Transport for Unreachable {
        fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
            match protocol::decode_request(request) {
                protocol::Incoming::Ok(env) => match env.body {
                    protocol::Request::Get { req_id, .. }
                    | protocol::Request::Range { req_id, .. } => {
                        vec![protocol::encode_reply(&protocol::Reply::Unavailable {
                            req_id,
                            blocked_on: [7u8; 32],
                        })
                        .expect("encodes")]
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            }
        }
    }
    let mut db = EngineStore::new(Unreachable);
    assert_eq!(
        Reads::get(&mut db, b"k/one"),
        Err(StoreError::Unavailable),
        "an unanswerable read came back as an absent key; an app told that \
         will show nothing and believe it"
    );
    assert_eq!(
        Reads::scan(&mut db, b"a", b"z", false, 10),
        Err(StoreError::Unavailable),
        "an unanswerable range came back EMPTY, which is a wrong answer with \
         exactly the shape of a right one"
    );
    let msg = StoreError::Unavailable.to_string();
    assert!(msg.contains("may well exist"), "unhelpful: {msg}");
    println!("  unanswerable read -> {msg}");
}

/// A fixed clock and a fixed stream, so nothing here depends on the machine.
struct FixedEnv(u32);
impl craftworks_sdk::Env for FixedEnv {
    fn now_ms(&mut self) -> u64 {
        1_700_000_000_000
    }
    fn rand32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0
    }
}

/// A `Db` over the engine store READS, and its reads are round trips.
///
/// This replaces a test that asserted every `Db` read came back as a refusal.
/// That was the right test for the shape it described and the shape is gone:
/// `Db`'s reads take `&mut self` now, on both backends, so the thing worth
/// pinning is that they WORK — a record written through `Db` over an engine
/// is a record `Db` reads back.
#[test]
fn a_db_over_the_engine_store_reads_what_it_wrote() {
    use craftworks_sdk::{Db, Scan, Schema};
    let mut db = Db::new(started(0), FixedEnv(7), [0; 4]);
    let schema: Schema = serde_json::from_value(serde_json::json!({
        "type": "Task",
        "fields": [{"name": "title", "kind": "text", "required": true}],
    }))
    .expect("a schema");
    db.define("tasks", &schema).expect("define");

    let mut fields = serde_json::Map::new();
    fields.insert("title".into(), serde_json::json!("write it down"));
    let rec = db.put("tasks", &fields).expect("put");

    let id = craftworks_sdk::id::from_hex(&rec.id).expect("an id");
    let got = db.get("tasks", id).expect("get").expect("the record");
    assert_eq!(
        got.fields.get("title"),
        Some(&serde_json::json!("write it down")),
        "a record written through Db over an engine did not read back"
    );
    assert_eq!(db.count("tasks").expect("count"), 1);
    assert_eq!(db.scan("tasks", Scan::default()).expect("scan").len(), 1);
    assert_eq!(db.domains().expect("domains"), ["tasks"]);
    assert!(db.schema("tasks").expect("schema").is_some());
    println!("  Db over the engine store: define, put, get, scan, count, domains");
}

// ---------------------------------------------------------------------------
// The same state machine, behind a connection that misbehaves.
//
// This is the half of the pump that justifies it: one client, driven by a
// test, by the live driver and by a browser, so what a test exercises is what
// a socket runs. A test transport that only ever delivers replies in order,
// once each, exercises none of what a socket does.
// ---------------------------------------------------------------------------

/// Writes and reads survive reordering and duplication, and the CONTROL shows
/// the perturbation is real.
#[test]
fn the_client_survives_a_connection_that_reorders_and_duplicates() {
    use craftworks_sdk::Reads;
    for seed in [1u64, 7, 99, 12345] {
        let mut db = rough(0, seed);
        for i in 0..8u32 {
            craftworks_sdk::Store::put(
                &mut db,
                format!("k/{i:02}").as_bytes(),
                format!("v{i}").as_bytes(),
            )
            .expect("the store took the write");
        }
        for i in 0..8u32 {
            let k = format!("k/{i:02}");
            assert_eq!(
                Reads::get(&mut db, k.as_bytes()).unwrap().as_deref(),
                Some(format!("v{i}").as_bytes()),
                "seed {seed}: {k} did not read back through a rough connection"
            );
        }
        let rows = Reads::scan(&mut db, b"k/", b"k0", false, usize::MAX).unwrap();
        assert_eq!(rows.len(), 8, "seed {seed}: a range came back short");

        // The perturbation ACTUALLY HAPPENED. Without this the test is a
        // wrapper that forwards and a claim about tolerance nobody measured.
        let seen = db.transport().seen;
        assert!(
            seen.reordered > 0 && seen.duplicated > 0,
            "seed {seed}: nothing was perturbed ({seen:?}), so this run says \
             nothing about tolerating a real connection"
        );
        println!("  seed {seed}: {seen:?}");
    }
}

/// THE CONTROL: the same wrapper with every arm off perturbs nothing.
#[test]
fn the_chaos_wrapper_with_every_arm_off_changes_nothing() {
    use craftworks_sdk::Reads;
    let mut db = calm(0);
    craftworks_sdk::Store::put(&mut db, b"k/one", b"first").expect("the store took the write");
    assert_eq!(
        Reads::get(&mut db, b"k/one").unwrap().as_deref(),
        Some(&b"first"[..])
    );
    let seen = db.transport().seen;
    assert_eq!(
        (seen.reordered, seen.duplicated, seen.dropped),
        (0, 0, 0),
        "the wrapper perturbed a stream it was told to leave alone: {seen:?}"
    );
    assert!(
        seen.delivered > 0,
        "nothing went through the wrapper at all"
    );
}

/// A dropped reply does not hang: the read is ANSWERED, one way or another.
///
/// A client that waits for something never coming is a UI that never settles,
/// and on this platform replies really are dropped — the node's notification
/// channel drops when full (F39).
#[test]
fn a_dropped_reply_is_answered_rather_than_waited_on() {
    use craftworks_sdk::{Reads, StoreError};
    // Started over ONE calm exchange: a read to an engine that has not
    // recovered its head waits for it (sdk#223), so an unstarted engine would
    // send no reply to drop, and this test would pass on a timeout that proves
    // nothing about dropped replies.
    let mut chaos = Chaos::dropping(Loop::new(0), 42, 1);
    chaos.calm_for = 1;
    let mut db = EngineStore::new(chaos);
    db.identity().expect("the engine answers who it is");
    // From here EVERY reply dropped: the harshest case, and the one that must not hang.
    let answer = Reads::get(&mut db, b"k/one");
    assert_eq!(
        answer,
        Err(StoreError::NoAnswer),
        "a read whose replies were all dropped came back {answer:?} — it must \
         be answered, and it must not be answered with a value nobody sent"
    );
    let seen = db.transport().seen;
    assert!(seen.dropped > 0, "nothing was dropped: {seen:?}");
    assert_eq!(seen.delivered, 0, "something got through: {seen:?}");
}

/// Messages the client cannot use are counted, by reason, never ignored.
#[test]
fn an_unreadable_message_is_counted_and_not_silently_ignored() {
    let mut db = started(0);
    let before = db.dropped().len();
    // A prefix that parses is NOT a message: the wire refuses trailing bytes
    // for the same reason a decoder that accepts a prefix turns a wrapped
    // value into its wrapper's first variant.
    let mut damaged = protocol::encode_reply(&protocol::Reply::Value {
        req_id: 0,
        value: None,
    })
    .expect("encodes");
    damaged.push(0xFF);
    db.client().on_inbound(&damaged);
    db.client().on_inbound(b"not a message at all");
    let now = db.dropped();
    assert_eq!(
        now.len() - before,
        2,
        "unreadable messages were not counted: {now:?}"
    );
    assert!(
        now.contains(&protocol::Dropped::TrailingBytes),
        "a prefix with bytes after it was not reported as such: {now:?}"
    );
    println!("  dropped: {now:?}");
}
