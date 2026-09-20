//! A COLD WRITE, against a REAL engine.
//!
//! The JavaScript test for this drives a FAKE session. It is worth having —
//! it is the path an app takes — but it exercises the retry LOOP and never
//! the parking underneath it, which is the half that decides whether there is
//! anything to retry.
//!
//! Measured, in review: with the recovery inline in `web/src/session.rs`, the
//! sdk#89 defect could be put straight back — `define` returning a ticketless
//! `NotLoaded` — and the whole suite stayed green. `build` succeeded,
//! `engine-db.test.mjs` passed, rc 0. The defect that cost five acceptance
//! runs was reintroducible with nothing to show for it.
//!
//! So the decision moved into `craftworks_sdk::parking`, where this can drive
//! it against a real `Shell` over a real block store — the same reason
//! `Refresh` lives in this crate rather than in the session.
//!
//! What a cold write IS: the app publishes, the page has read nothing, and
//! the very first thing it does is `define` a domain. That define must read
//! the existing schema before it can check the new one against it, and there
//! is nothing loaded to read.

use craftworks_sdk::{decide, CachedStore, Loads, Outcome};
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

const AT: protocol::At = protocol::At {
    seq: 1,
    root: [1u8; 32],
};

#[derive(Clone, Default)]
struct Node(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Node {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Node {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

#[derive(Clone, Default)]
struct Head(Rc<RefCell<Option<(u64, Cid)>>>);

/// The delegate, rebuilt from its context on every call (F32).
struct Conn {
    node: Node,
    head: Head,
    ctx: Vec<u8>,
}

impl Conn {
    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let mut shell: Shell<Node> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.node.clone(),
            StoreFacts::provisioned(),
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.node.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = self.node.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    let mut h = self.head.0.borrow_mut();
                    if h.is_none_or(|(s, _)| seq > s) {
                        *h = Some((seq, root));
                    }
                    let (seq, root) = h.expect("just written");
                    drop(h);
                    next.push(Inbound::GotHead { seq, root });
                }
                engine_delegate::schedule::Op::ReadHead { .. } => match *self.head.0.borrow() {
                    Some((seq, root)) => next.push(Inbound::GotHead { seq, root }),
                    None => next.push(Inbound::NoHead),
                },
            }
        }
        let mut replies = out.replies;
        if !next.is_empty() {
            replies.extend(self.step(next));
        }
        replies
    }
}

type Db = craftworks_sdk::Db<CachedStore, craftworks_sdk::SystemEnv>;

/// A page with nothing loaded, as it is the instant after a publish.
struct Page {
    db: Db,
    loads: Loads,
    conn: Conn,
    /// Range requests this page has sent. The measurement: a parked call must
    /// ASK for what it is waiting on, or the wait is for ever.
    requests: usize,
}

impl Page {
    fn new() -> Page {
        let mut p = Page {
            db: craftworks_sdk::Db::new(
                testkit::cached_store().0,
                craftworks_sdk::SystemEnv,
                [7u8; 4],
            ),
            loads: Loads::new(),
            conn: Conn {
                node: Node::default(),
                head: Head::default(),
                ctx: Vec::new(),
            },
            requests: 0,
        };
        // START THE ENGINE, exactly as a session does: `Identity` is what
        // makes the delegate read its head. Without it this page's engine
        // stands on nothing and every answer is empty for a reason that has
        // nothing to do with the code under test.
        p.db.store_mut().client.send(&protocol::Request::Identity);
        p.pump();
        p
    }

    /// Send what is queued, feed every reply back, and finish any load the
    /// replies complete. Returns how many range requests went out.
    fn pump(&mut self) -> usize {
        let mut sent = 0;
        loop {
            let frames = self.db.store_mut().take_outbound();
            if frames.is_empty() {
                return sent;
            }
            for frame in frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(&frame) {
                    if matches!(env.body, protocol::Request::Range { .. }) {
                        sent += 1;
                        self.requests += 1;
                    }
                }
                for reply in self.conn.step(vec![Inbound::Client(frame)]) {
                    self.db.store_mut().on_inbound(&reply);
                    self.take_page(&reply);
                }
            }
        }
    }

    /// A page reply reaches the copy AND the loads, which is what ends the
    /// ticket a parked call is waiting on.
    fn take_page(&mut self, reply: &[u8]) {
        let Ok(protocol::Reply::Page {
            req_id,
            entries,
            cursor,
            ..
        }) = protocol::decode_reply(reply)
        else {
            return;
        };
        match self.loads.on_page(req_id, entries, cursor, AT) {
            craftworks_sdk::loads::Page::More { lo, hi, after } => {
                self.db.store_mut().client.send(&Loads::range_request(
                    req_id,
                    &lo,
                    &hi,
                    Some(after),
                ));
            }
            craftworks_sdk::loads::Page::Complete { lo, hi, rows } => {
                self.db.store_mut().on_page(&lo, &hi, rows, [0u8; 32]);
            }
            craftworks_sdk::loads::Page::Restart { lo, hi } => {
                self.db
                    .store_mut()
                    .client
                    .send(&Loads::range_request(req_id, &lo, &hi, None));
            }
            craftworks_sdk::loads::Page::Nothing => {}
        }
    }

    /// One attempt at a write, through the decision a session makes.
    fn try_define(&mut self, domain: &str, schema: &craftworks_sdk::Schema) -> Outcome<()> {
        let r = self.db.define(domain, schema);
        let (loads, store) = (&mut self.loads, self.db.store_mut());
        decide(loads, store, r, 0)
    }
}

fn schema() -> craftworks_sdk::Schema {
    serde_json::from_str(r#"{"type":"Task","fields":[{"name":"title","kind":"text"}]}"#)
        .expect("a schema this build can read")
}

/// **THE DEFECT, AS A TEST.** A cold define parks, asks, and then APPLIES.
#[test]
fn a_cold_define_parks_asks_and_then_applies() {
    let mut p = Page::new();
    let before = p.requests;

    // FIRST ATTEMPT: nothing is loaded, so it cannot read the existing schema.
    let first = p.try_define("tasks", &schema());
    let ticket = match first {
        Outcome::Wait(_, t) => t,
        Outcome::Told(e) => panic!(
            "a cold define was TOLD `{e:?}` with no ticket. That is sdk#89: nothing \
             can retry it, so the app keeps a form on screen, a button saying \
             Published, and refuses every write with `has no schema; define it first`"
        ),
        Outcome::Done(()) => panic!(
            "a cold define succeeded with nothing loaded, so this page is not cold \
             and the test measures a warm one"
        ),
    };
    assert!(ticket > 0, "the ticket is not one anything can wait on");

    // AND IT ASKED. A ticket nobody requested a load for waits for ever.
    p.pump();
    assert!(
        p.requests > before,
        "the define parked without requesting the range it is waiting on"
    );

    // SECOND ATTEMPT, once the page has arrived.
    match p.try_define("tasks", &schema()) {
        Outcome::Done(()) => {}
        other => panic!("the define did not apply after its load completed: {other:?}"),
    }

    // AND THE TREE HOLDS IT. "It stopped erroring" is not the claim.
    let got =
        p.db.schema("tasks")
            .expect("the schema read is answerable now");
    assert_eq!(
        got.as_ref().map(|s| s.type_name.as_str()),
        Some(schema().type_name.as_str()),
        "the define reported success and the domain has no schema"
    );
}

/// The same for the writes that go through `need_schema`.
#[test]
fn a_cold_put_update_and_delete_park_too() {
    for what in ["put", "update", "delete"] {
        let mut p = Page::new();
        let mut fields = serde_json::Map::new();
        fields.insert("title".into(), serde_json::json!("x"));
        let r = match what {
            "put" => p.db.put("tasks", &fields).map(|_| ()),
            "update" => p.db.update("tasks", &[9u8; 16], &fields).map(|_| ()),
            _ => p.db.delete("tasks", &[9u8; 16]).map(|_| ()),
        };
        let (loads, store) = (&mut p.loads, p.db.store_mut());
        match decide(loads, store, r, 0) {
            Outcome::Wait(_, t) => assert!(t > 0, "{what}: parked with an unusable ticket"),
            Outcome::Told(e) => panic!(
                "a cold {what} was TOLD `{e:?}` with no ticket. It reads `need_schema` \
                 before it applies, so a cold one is 'I could not look', not 'invalid'"
            ),
            Outcome::Done(()) => panic!("{what}: succeeded on a page with nothing loaded"),
        }
    }
}

/// **THE CONTROL.** An INVALID write is refused, with no ticket and no load.
#[test]
fn control_an_invalid_write_is_refused_and_asks_for_nothing() {
    let mut p = Page::new();
    let before = p.requests;
    // A domain name no load can make legal.
    let bad = "";
    match p.try_define(bad, &schema()) {
        Outcome::Told(_) => {}
        Outcome::Wait(e, t) => panic!(
            "an invalid define was parked on ticket {t} for `{e:?}`. No load makes a \
             bad domain name good, so this waits for something that cannot help"
        ),
        Outcome::Done(()) => panic!("an invalid define was accepted"),
    }
    assert_eq!(
        p.requests, before,
        "it requested a range for a write no range can fix"
    );
}

/// **THE CONTROL nothing tested**: a span already loaded that still cannot
/// answer is TOLD, not sent round.
///
/// This is `Loads::want` returning `None`. Without it, a decision that parked
/// unconditionally would pass every other test here and loop an app for ever
/// on a range that has already been fetched and does not contain the answer.
#[test]
fn control_a_span_already_loaded_is_told_not_sent_round() {
    let mut p = Page::new();

    // Park once, and complete the load. THE SPAN COMES FROM THE ERROR the
    // code produced, not from a second copy of the key layout written here:
    // a define parks on the SCHEMA key, which is not the record range, and a
    // hand-written range would have tested a span nothing ever loaded.
    let Outcome::Wait(e, _) = p.try_define("tasks", &schema()) else {
        panic!("the first attempt did not park, so there is no loaded span to test");
    };
    let (lo, hi) = e
        .needs()
        .map(|(l, h)| (l.to_vec(), h.to_vec()))
        .expect("a parked error names its range");
    p.pump();

    // Now ask for that same range, with a call that still cannot be answered
    // from it. `want` refuses to queue a span it has already loaded.
    assert!(
        p.loads.want(&lo, &hi, 0).is_none(),
        "the span is not recorded as loaded, so this test is not exercising the arm it names"
    );

    let before = p.requests;
    let told = decide(
        &mut p.loads,
        p.db.store_mut(),
        Err::<(), _>(craftworks_sdk::DbError::NotLoaded {
            lo: lo.clone(),
            hi: hi.clone(),
        }),
        0,
    );
    match told {
        Outcome::Told(_) => {}
        Outcome::Wait(_, t) => panic!(
            "a span already loaded was queued AGAIN on ticket {t}. Loading it a second \
             time answers exactly as the first did, so an app asking in a loop never stops"
        ),
        Outcome::Done(()) => unreachable!("the input was an error"),
    }
    assert_eq!(
        p.requests, before,
        "it sent a second request for a range it had already loaded"
    );
}
