//! M2 on the DELEGATE path (sdk#148's SDK half): every `Db` write says what it
//! READ, the Shell hands the reads to the engine, and the engine refuses a
//! write whose read no longer holds. The client is told `Failed` here — the
//! delegate path says `Conflict` only once v5 is current (main's ruling: an
//! older v4 build would drop the new reply and time out), and the Shell NEVER
//! sends `Conflicted`. Two sessions on one node.

use craftworks_sdk::store::{Reads, RowState};
use craftworks_sdk::*;
use engine_delegate::shell::Inbound;
use serde_json::json;
use std::collections::BTreeMap;
use testkit::full_node::{Conn, FullNode};

struct At(u32);
impl Env for At {
    fn now_ms(&mut self) -> u64 {
        1_750_000_000_000
    }
    fn rand32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        self.0
    }
}

const LOAD: u64 = 1;
const EVERYTHING: [u8; 64] = [0xFF; 64];

struct Tab {
    db: Db<CachedStore, At>,
    clock: testkit::Clock,
    conn: Conn,
    sends: BTreeMap<u64, usize>,
    verdicts: BTreeMap<u64, Vec<protocol::WriteState>>,
    conflicted_replies: usize,
}

impl Tab {
    fn open(node: &FullNode, seed: u32) -> Tab {
        let (store, clock) = testkit::cached_store();
        let mut t = Tab {
            db: Db::new(store, At(seed), [seed as u8, 2, 3, 4]),
            clock,
            conn: node.connect(),
            sends: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            conflicted_replies: 0,
        };
        t.db.store_mut().client.send(&protocol::Request::Identity);
        t.pump();
        t.load();
        t
    }

    /// Load the whole key space again (an opening session, or a reload).
    fn load(&mut self) {
        self.db.store_mut().request_range(LOAD, b"", &EVERYTHING, 256);
        self.pump();
    }

    fn pump(&mut self) {
        for _ in 0..400 {
            let frames = self.db.store_mut().take_outbound();
            if frames.is_empty() {
                return;
            }
            for f in &frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(f) {
                    if let protocol::Request::Write { write_id, .. } | protocol::Request::Commit { write_id, .. } = env.body {
                        *self.sends.entry(write_id).or_default() += 1;
                    }
                }
            }
            for reply in self.conn.step(frames.into_iter().map(Inbound::Client).collect()) {
                let decoded = protocol::decode_reply(&reply);
                if let Some((write_id, state)) = decoded.as_ref().ok().and_then(|r| self.db.store_mut().client.own_write_state(r)) {
                    self.verdicts.entry(write_id).or_default().push(state);
                }
                if matches!(decoded, Ok(protocol::Reply::Conflicted { .. })) {
                    self.conflicted_replies += 1;
                }
                if let Ok(protocol::Reply::Page { req_id: LOAD, entries, cursor: None, at, .. }) = decoded {
                    self.db.store_mut().on_page(b"", &EVERYTHING, entries, at.root);
                }
                self.db.store_mut().on_inbound(&reply);
            }
        }
        panic!("the tab never reached a standstill");
    }

    fn seconds(&mut self, n: u64) {
        for _ in 0..n {
            self.clock.advance(1000);
            let _ = self.db.store_mut().tick();
            self.pump();
        }
    }

    fn told(&self, id: u64) -> Vec<protocol::WriteState> {
        self.verdicts.get(&id).cloned().unwrap_or_default()
    }
}

fn schema() -> Schema {
    serde_json::from_value(json!({ "type": "Task", "fields": [{ "name": "title", "kind": "text", "required": true }] })).unwrap()
}

fn fields(t: &str) -> serde_json::Map<String, serde_json::Value> {
    json!({ "title": t }).as_object().unwrap().clone()
}

fn published(v: &[protocol::WriteState]) -> bool {
    v.contains(&protocol::WriteState::Published)
}

/// B read the record, A changed it, B's update from its stale read is REFUSED:
/// nothing applied, `Failed` on this path, rolled back, sent ONCE, and A's
/// value stands for a fresh reader. The control (no A in between) publishes.
#[test]
fn a_stale_update_is_refused_rolled_back_and_never_re_sent() {
    for interfere in [false, true] {
        let node = FullNode::new();
        let mut a = Tab::open(&node, 1);
        a.db.define("tasks", &schema()).expect("define");
        a.pump();
        a.seconds(3);
        let rec = a.db.put("tasks", &fields("first")).expect("put");
        a.pump();
        a.seconds(3);
        let loc = id::loc_from_hex(&rec.id).expect("an id");
        let key = craftworks_sdk::db::record_key("tasks", loc);
        let mut b = Tab::open(&node, 2);
        if interfere {
            a.db.update("tasks", loc, &fields("A's edit")).expect("A's update");
            a.pump();
            a.seconds(3);
            let wa = a.db.store_mut().next_write_id() - 1;
            assert!(published(&a.told(wa)), "A's update did not publish");
        }
        let w = b.db.store_mut().next_write_id();
        b.db.update("tasks", loc, &fields("B's edit")).expect("B's update is made");
        b.pump();
        b.seconds(10);
        assert_eq!(b.sends.get(&w), Some(&1), "interfere {interfere}: B's write went on the wire {:?} times", b.sends.get(&w));
        assert_eq!(b.conflicted_replies, 0, "the Shell sent `Conflicted` — a reply an older v4 build cannot read");
        if interfere {
            // Refused where its read is judged (`Failed`), or — when B's
            // engine judged it on its own older view — lost to A's newer head
            // (`Lost`, R0: re-judged if ever re-sent). Either way NOT applied.
            assert!(
                matches!(b.told(w).last(), Some(protocol::WriteState::Failed | protocol::WriteState::Lost)),
                "a stale update was not refused: {:?}",
                b.told(w)
            );
            assert!(!published(&b.told(w)), "a stale update published over A's edit");
            assert_eq!(Reads::row_state(b.db.store_mut(), &key), RowState::RolledBack);
            let mut fresh = Tab::open(&node, 3);
            let got = fresh.db.get("tasks", loc).expect("read").expect("present");
            assert_eq!(got.fields["title"], json!("A's edit"), "A's edit did not stand");
        } else {
            assert!(published(&b.told(w)), "an update from a true read did not publish: {:?}", b.told(w));
        }
    }
}

/// Two sessions `create_at` ONE slot, each having read it absent: exactly ONE
/// record is made; the other is refused (sdk#149's stated residual, closed).
#[test]
fn two_create_ats_of_one_slot_make_one_record() {
    let node = FullNode::new();
    let mut a = Tab::open(&node, 1);
    a.db.define("tasks", &schema()).expect("define");
    a.pump();
    a.seconds(3);
    let mut b = Tab::open(&node, 2);
    let slot = slot_from(1_749_999_000_000, "ns", "source-1");
    let wa = a.db.store_mut().next_write_id();
    let wb = b.db.store_mut().next_write_id();
    assert!(matches!(a.db.create_at("tasks", slot, &fields("from A")).expect("A"), CreateAt::Created(_)));
    assert!(matches!(b.db.create_at("tasks", slot, &fields("from B")).expect("B"), CreateAt::Created(_)), "B read the slot absent too");
    a.pump();
    b.pump();
    a.seconds(5);
    b.seconds(5);
    let made = [published(&a.told(wa)), published(&b.told(wb))];
    assert_eq!(made.iter().filter(|m| **m).count(), 1, "A {:?}, B {:?}", a.told(wa), b.told(wb));
}

/// Two sessions define ONE schema from one absent base: BOTH succeed (a write
/// already there is not a conflict — main's ruling). The builder's two tabs do
/// this on every mount.
#[test]
fn two_sessions_defining_one_schema_both_succeed() {
    let node = FullNode::new();
    let mut a = Tab::open(&node, 1);
    let mut b = Tab::open(&node, 2);
    let (wa, wb) = (a.db.store_mut().next_write_id(), b.db.store_mut().next_write_id());
    a.db.define("tasks", &schema()).expect("A defines");
    b.db.define("tasks", &schema()).expect("B defines");
    a.pump();
    b.pump();
    a.seconds(5);
    b.seconds(5);
    assert!(published(&a.told(wa)), "A: {:?}", a.told(wa));
    assert!(published(&b.told(wb)), "B's identical define was refused: {:?}", b.told(wb));
}

/// THE SHELL'S OWN MAPPING, on a Conflict its engine decides by the READ
/// check: a stale `Commit` straight through one connection. The Shell says
/// `Failed` — never `Conflict`, never `Conflicted` — to v4 (main's ruling:
/// an older v4 build would drop either and time out).
#[test]
fn the_shell_tells_a_stale_commit_failed_never_conflict() {
    let node = FullNode::new();
    let mut conn = node.connect();
    let send = |conn: &mut Conn, r: &protocol::Request| -> Vec<protocol::Reply> {
        let f = protocol::encode_session_request(protocol::CURRENT, 0x1234_5678, r).expect("encodes");
        conn.step(vec![Inbound::Client(f)]).iter().filter_map(|b| protocol::decode_reply(b).ok()).collect()
    };
    let _ = send(&mut conn, &protocol::Request::Identity);
    let put = protocol::Request::Commit { write_id: 1, reads: vec![(b"k".to_vec(), protocol::Expect::Absent)], ops: vec![protocol::Op::Put(b"k".to_vec(), b"v1".to_vec())] };
    let _ = send(&mut conn, &put);
    for _ in 0..20 {
        let _ = send(&mut conn, &protocol::Request::Tick { now: 1_750_000_000 });
    }
    // Read Absent again: stale.
    let stale = protocol::Request::Commit { write_id: 2, reads: vec![(b"k".to_vec(), protocol::Expect::Absent)], ops: vec![protocol::Op::Put(b"k".to_vec(), b"v2".to_vec())] };
    let mut replies = send(&mut conn, &stale);
    for _ in 0..20 {
        replies.extend(send(&mut conn, &protocol::Request::Tick { now: 1_750_000_001 }));
    }
    let states: Vec<protocol::WriteState> = replies
        .iter()
        .filter_map(|r| match r {
            protocol::Reply::SessionWriteState { write_id: 2, state, .. } | protocol::Reply::WriteState { write_id: 2, state } => Some(*state),
            _ => None,
        })
        .collect();
    assert!(states.contains(&protocol::WriteState::Failed), "a stale Commit was not refused: {states:?}");
    assert!(!states.contains(&protocol::WriteState::Conflict), "the Shell told Conflict: {states:?}");
    assert!(!replies.iter().any(|r| matches!(r, protocol::Reply::Conflicted { .. })), "the Shell sent Conflicted");
}
