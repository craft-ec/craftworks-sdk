//! A BURST OF WRITES REACHES THE NETWORK, INSTEAD OF VANISHING.
//!
//! The engine takes one commit at a time and refuses a write that arrives
//! while one is in flight — `Busy`, terminal, nothing applied. Its own
//! comment says where the buffering goes:
//!
//! > Buffering belongs to the client, which has a page and an outbox; the
//! > delegate has neither.
//!
//! That outbox did not exist. `Busy` set a flag and nothing re-sent, so a
//! burst was accepted into the copy, refused by the engine, and left until
//! `time_out` rolled it back — at which point the rows VANISHED from the
//! screen, a minute after the person made them, with nothing saying why.
//!
//! Measured on a real node before the fix: 20 writes in 10 ms, 10 published,
//! 12 frozen at PENDING through 30 s and gone by 70 (sdk#106).

use craftworks_sdk::store::Store as _;
use craftworks_sdk::CachedStore;
use testkit::page_node::{PageConn, PageNode};

struct Page {
    store: CachedStore,
    _node: PageNode,
    conn: PageConn,
    /// Every `Write` this page put on the wire, in the order it sent them.
    sent: Vec<u64>,
    /// Every write the ENGINE ACCEPTED, in the order it accepted them.
    ///
    /// This, not `sent`, is the order that reaches the tree. A write refused
    /// `Busy` and re-sent later appears twice in `sent` and once here, so
    /// `sent` is not sorted and should not be: the claim is about what was
    /// APPLIED.
    accepted: Vec<u64>,
}

impl Page {
    fn new() -> Page {
        // THE SHARED FIXTURE, not forty hand-rolled lines: the page path's
        // node (`page::Server` over a scripted client API), which reads its
        // head on `Identity` and feeds its own answers back (sdk#101).
        let node = PageNode::new();
        let conn = node.connect();
        let mut p = Page {
            store: testkit::cached_store().0,
            _node: node,
            conn,
            sent: Vec::new(),
            accepted: Vec::new(),
        };
        p.store.client.send(&protocol::Request::Identity);
        p.pump();
        p
    }

    /// Send what is queued and feed every reply back, to a standstill.
    ///
    /// EVERY WAITING FRAME IN ONE CALL, which is the whole point. Delivering
    /// them one at a time let each commit run to completion before the next
    /// write arrived, so no write ever met a commit in flight and the
    /// contention this file exists to test never happened — the first version
    /// of this harness passed with the fix removed.
    ///
    /// A burst made faster than a round trip really does land together, and
    /// the page's Server takes every frame before any node answer.
    fn pump(&mut self) {
        for _ in 0..200 {
            let frames = self.store.take_outbound();
            if frames.is_empty() {
                return;
            }
            let mut inbound = Vec::new();
            for frame in frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(&frame) {
                    if let protocol::Request::Write { write_id, .. } = env.body {
                        self.sent.push(write_id);
                    }
                }
                inbound.push(frame);
            }
            for reply in self.conn.frames(&inbound) {
                if let Some((write_id, state)) = protocol::decode_reply(&reply)
                    .ok()
                    .and_then(|r| self.store.client.own_write_state(&r))
                {
                    if state == protocol::WriteState::Accepted {
                        self.accepted.push(write_id);
                    }
                }
                self.store.on_inbound(&reply);
            }
        }
        panic!("the page never reached a standstill: it is sending for ever");
    }

    fn write(&mut self, n: usize) {
        let key = format!("d/notes/{n:06}").into_bytes();
        self.store.put(&key, format!("body {n}").as_bytes()).expect("the store took the write");
    }

    /// Writes still outstanding: (count, bytes).
    ///
    /// Asked of the COPY rather than of a row, because a row's state needs
    /// its range loaded and a write to a range nobody has read is still a
    /// write. What is being claimed is that none are left waiting.
    fn outstanding(&mut self) -> (usize, usize) {
        self.store.copy.pending()
    }
}

/// **THE DEFECT, AS A TEST.** Twenty writes in a burst all reach the network.
#[test]
fn a_burst_of_writes_all_reach_the_network() {
    const N: usize = 20;
    let mut p = Page::new();

    // A BURST: every write made before any verdict comes back, which is what
    // a person typing quickly — or an import — actually does.
    for i in 0..N {
        p.write(i);
    }

    // Then let the exchange run to a standstill. Nothing here sends a write
    // again by hand: if the outbox does not do it, nothing does.
    p.pump();

    let (left, bytes) = p.outstanding();
    assert_eq!(
        (left, p.store.queued_count()),
        (0, 0),
        "{left} of {N} writes never reached the network ({bytes} B still held, \
         {} still queued).\n\
         The engine refuses a write that arrives while a commit is in flight — \
         `Busy`, terminal, nothing applied — and says the client must re-submit it. \
         Nothing did, so these sat in the copy until `time_out` rolled them back and \
         their rows vanished from the screen a minute after they were made.",
        p.store.queued_count()
    );

    // AND NOTHING WAS ROLLED BACK. "Nothing outstanding" would also be true
    // of a copy that gave up on all of them.
    let told = p.store.tick();
    assert!(
        told.rolled_back.is_empty(),
        "{} write(s) were rolled back: {:?}. Reaching the network and being \
         abandoned both leave the copy empty, and only one of them is success.",
        told.rolled_back.len(),
        told.rolled_back
    );
}

/// THE CONTROL: with nothing queued, the drain sends nothing.
///
/// Without this, a drain that re-sent unconditionally would pass the test
/// above and put a duplicate of every write on the wire for ever.
#[test]
fn control_an_idle_outbox_sends_nothing() {
    let mut p = Page::new();
    p.write(0);
    p.pump();
    assert_eq!(p.outstanding().0, 0, "the single write never settled");
    assert_eq!(p.store.queued_count(), 0, "nothing should be queued");

    // Drain repeatedly against an empty outbox: it must produce no traffic.
    for _ in 0..5 {
        p.store.drain_queued();
    }
    assert!(
        p.store.take_outbound().is_empty(),
        "the drain sent something with nothing queued — every tick would put \
         a duplicate write on the wire"
    );
}

/// THE CONTROL: writes land in the ORDER they were made.
///
/// The outbox sends one at a time, oldest first, because a later edit to a
/// key arriving before an earlier one leaves the network holding the older
/// value — a corruption the row would never show.
#[test]
fn control_a_key_written_twice_keeps_the_later_value() {
    let mut p = Page::new();
    let key = b"d/notes/000000".to_vec();
    p.store.put(&key, b"first").expect("the store took the write");
    p.store.put(&key, b"second").expect("the store took the write");
    p.store.put(&key, b"third").expect("the store took the write");
    p.pump();

    assert_eq!(p.outstanding().0, 0, "the key never settled");

    // APPLIED in the order they were made. `sent` is deliberately not the
    // measure: a write refused `Busy` and re-sent later appears twice there,
    // so it is not sorted and should not be. What must be ordered is what the
    // engine ACCEPTED, because that is what reaches the tree — a later edit
    // applied before an earlier one leaves the network holding the OLDER
    // value, and the row would never show it.
    let mut ascending = p.accepted.clone();
    ascending.sort_unstable();
    assert_eq!(
        p.accepted, ascending,
        "the engine applied the writes in the order {:?}, which is not the \
         order they were made",
        p.accepted
    );
    assert_eq!(
        p.accepted.len(),
        3,
        "only {} of 3 writes were ever accepted: {:?}",
        p.accepted.len(),
        p.accepted
    );
}
