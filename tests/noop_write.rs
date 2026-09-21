//! craftworks-sdk#160: a write that changes NOTHING, through the path an app
//! takes -- `Db` over `CachedStore` over the real engine (`FullNode`).
//!
//! `Db::define` writes the schema unconditionally, and an app defines every
//! domain on every open. Traced here before the fix, LIVE, not latent: the
//! client does not drop an identical write -- it went on the wire, was told
//! `[Accepted]` and nothing more in 90 s, and the next write on another key
//! was told `Busy` six times as the outbox re-sent it.

use craftworks_sdk::store::{Edit, Store as _};
use craftworks_sdk::*;
use engine_delegate::shell::Inbound;
use serde_json::json;
use std::collections::BTreeMap;
use testkit::full_node::{Conn, FullNode};

struct At;
impl Env for At {
    fn now_ms(&mut self) -> u64 {
        1_750_000_000_000
    }
    fn rand32(&mut self) -> u32 {
        7
    }
}

const LOAD: u64 = 1;
const EVERYTHING: [u8; 64] = [0xFF; 64];

struct Page {
    db: Db<CachedStore, At>,
    clock: testkit::Clock,
    _node: FullNode,
    conn: Conn,
    /// Per write id, how many times it went on the wire.
    sends: BTreeMap<u64, usize>,
    verdicts: BTreeMap<u64, Vec<protocol::WriteState>>,
}

impl Page {
    fn new() -> Page {
        let node = FullNode::new();
        let conn = node.connect();
        let (store, clock) = testkit::cached_store();
        let mut p = Page {
            db: Db::new(store, At, [1, 2, 3, 4]),
            clock,
            _node: node,
            conn,
            sends: BTreeMap::new(),
            verdicts: BTreeMap::new(),
        };
        p.db.store_mut().client.send(&protocol::Request::Identity);
        p.pump();
        // An opening session loads before it reads: the whole key space,
        // over the wire, so `define`'s schema read is answered, not NotLoaded.
        p.db.store_mut().request_range(LOAD, b"", &EVERYTHING, 256);
        p.pump();
        p
    }

    fn pump(&mut self) {
        for _ in 0..200 {
            let frames = self.db.store_mut().take_outbound();
            if frames.is_empty() {
                return;
            }
            for f in &frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(f) {
                    if let protocol::Request::Write { write_id, .. } = env.body {
                        *self.sends.entry(write_id).or_default() += 1;
                    }
                }
            }
            for reply in self
                .conn
                .step(frames.into_iter().map(Inbound::Client).collect())
            {
                match protocol::decode_reply(&reply) {
                    Ok(protocol::Reply::WriteState { write_id, state }) => {
                        self.verdicts.entry(write_id).or_default().push(state);
                    }
                    // What the HOST does with a page: `CachedStore` records a
                    // range as loaded only when told the interval it asked for.
                    Ok(protocol::Reply::Page {
                        req_id: LOAD,
                        entries,
                        cursor: None,
                        at,
                        ..
                    }) => self
                        .db
                        .store_mut()
                        .on_page(b"", &EVERYTHING, entries, at.root),
                    _ => {}
                }
                self.db.store_mut().on_inbound(&reply);
            }
        }
        panic!("the page never reached a standstill");
    }

    fn seconds(&mut self, n: u64) {
        for _ in 0..n {
            self.clock.advance(1000);
            let _ = self.db.store_mut().tick();
            self.pump();
        }
    }
}

fn schema() -> Schema {
    serde_json::from_value(json!({
        "type": "Task",
        "fields": [{ "name": "title", "kind": "text", "required": true }]
    }))
    .unwrap()
}

#[test]
fn defining_a_domain_twice_publishes_and_the_next_write_goes() {
    use protocol::WriteState as W;
    let mut p = Page::new();
    let w1 = p.db.store_mut().next_write_id();
    p.db.define("tasks", &schema()).expect("the first define");
    p.pump();
    p.seconds(5);
    let puts_before = p.conn.served(testkit::full_node::Served::Put);

    let w2 = p.db.store_mut().next_write_id();
    p.db.define("tasks", &schema())
        .expect("the same define again");
    p.pump();
    p.seconds(90);
    let puts_for_w2 = p.conn.served(testkit::full_node::Served::Put) - puts_before;

    let w3 = p.db.store_mut().next_write_id();
    p.db.store_mut()
        .apply_batch(&[(b"d/other".to_vec(), Edit::Put(b"x".to_vec()))]);
    p.pump();
    p.seconds(5);

    let told = |id: u64| p.verdicts.get(&id).cloned().unwrap_or_default();
    assert_eq!(told(w1), vec![W::Accepted, W::Published, W::ParityComplete]);
    assert_eq!(
        p.sends.get(&w2),
        Some(&1),
        "the identical define is not on the wire once -- this test no longer traces the engine"
    );
    assert_eq!(
        told(w2),
        vec![W::Accepted, W::Published, W::ParityComplete],
        "the identical define was not told it is published"
    );
    assert_eq!(
        puts_for_w2, 0,
        "a write that changes nothing put {puts_for_w2} block(s)"
    );
    assert_eq!(
        told(w3).first(),
        Some(&W::Accepted),
        "the write after the no-op was not accepted: {:?}",
        told(w3)
    );
    assert!(told(w3).contains(&W::Published), "{:?}", told(w3));
    assert_eq!(p.sends.get(&w3), Some(&1), "the next write was re-sent");
}
