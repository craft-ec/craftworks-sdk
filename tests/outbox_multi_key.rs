//! craftworks-sdk#139: the outbox never drained again once a MULTI-KEY write
//! was queued.
//!
//! `drain_queued` sends the next queued write only when no write is awaiting
//! a verdict, and it decided that by comparing `copy.pending().0` with
//! `copy.queued_count()`. The first counted (key, write) ENTRIES; the second
//! counted distinct WRITES. With single-key writes the two agree — which is
//! all #109's burst test ever used — so the comparison could not be seen to
//! be wrong. A 2-key write refused an ordinary, transient `Busy` made it
//! `2 > 1` for ever: the outbox stopped, and every write behind it was rolled
//! back at `pending_timeout_ms`. Measured before the fix: the batch sent once
//! and never again, 0 of 10 writes behind it published, 12 keys rolled back.

use craftworks_sdk::store::{Delta, Edit, MemStore, Read, Reads, Store};
use craftworks_sdk::{id::loc_from_hex, CachedStore, Db, SystemEnv};
use engine_delegate::shell::Inbound;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use testkit::full_node::{Conn, FullNode};

struct Page {
    store: CachedStore,
    clock: testkit::Clock,
    _node: FullNode,
    conn: Conn,
    sent: BTreeMap<u64, usize>,
    verdicts: BTreeMap<u64, Vec<protocol::WriteState>>,
}

impl Page {
    fn new() -> Page {
        let node = FullNode::new();
        let conn = node.connect();
        let (store, clock) = testkit::cached_store();
        let mut p = Page {
            store,
            clock,
            _node: node,
            conn,
            sent: BTreeMap::new(),
            verdicts: BTreeMap::new(),
        };
        p.store.client.send(&protocol::Request::Identity);
        p.pump();
        p
    }

    /// Deliver what is waiting, all of it together, and feed the replies back.
    /// One exchange; `pump` repeats it to a standstill.
    fn exchange(&mut self) -> bool {
        let frames = self.store.take_outbound();
        if frames.is_empty() {
            return false;
        }
        let mut inbound = Vec::new();
        for frame in frames {
            if let protocol::Incoming::Ok(env) = protocol::decode_request(&frame) {
                if let protocol::Request::Write { write_id, .. } = env.body {
                    *self.sent.entry(write_id).or_default() += 1;
                }
            }
            inbound.push(Inbound::Client(frame));
        }
        for reply in self.conn.step(inbound) {
            if let Ok(protocol::Reply::WriteState { write_id, state }) =
                protocol::decode_reply(&reply)
            {
                self.verdicts.entry(write_id).or_default().push(state);
            }
            self.store.on_inbound(&reply);
        }
        true
    }

    fn pump(&mut self) {
        for _ in 0..200 {
            if !self.exchange() {
                return;
            }
        }
        panic!("the page never reached a standstill");
    }

    fn published(&self, id: u64) -> bool {
        use protocol::WriteState as W;
        self.verdicts.get(&id).is_some_and(|v| {
            v.iter()
                .any(|s| matches!(s, W::Published | W::ParityComplete))
        })
    }

    fn told_busy(&self, id: u64) -> bool {
        self.verdicts
            .get(&id)
            .is_some_and(|v| v.contains(&protocol::WriteState::Busy))
    }

    /// A tick a second, as the page sends time, to past `pending_timeout_ms`.
    /// Returns the keys rolled back on the way.
    fn run_past_the_timeout(&mut self) -> usize {
        let mut rolled = 0;
        for _ in 0..61 {
            self.clock.advance(1000);
            rolled += self.store.tick().rolled_back_keys.len();
            self.pump();
        }
        rolled
    }

    fn pair(&mut self, tag: &str) -> u64 {
        let id = self.store.next_write_id();
        self.store.apply_batch(&[
            (
                format!("d/pair/{tag}/a").into_bytes(),
                Edit::Put(b"1".to_vec()),
            ),
            (
                format!("d/pair/{tag}/b").into_bytes(),
                Edit::Put(b"2".to_vec()),
            ),
        ]);
        id
    }

    fn one(&mut self, tag: &str) -> u64 {
        let id = self.store.next_write_id();
        self.store.put(format!("d/notes/{tag}").as_bytes(), b"x");
        id
    }
}

/// THE DEFECT: an ordinary 2-key write refused a TRANSIENT `Busy` — nothing
/// about it is too large — and N ordinary writes behind it. All of them must
/// publish, as a COUNT, and nothing may be rolled back.
#[test]
fn writes_behind_a_multi_key_write_refused_busy_all_publish() {
    const N: usize = 10;
    let mut p = Page::new();
    // One ordinary write first, so a commit is in flight when the pair lands.
    p.one("first");
    let pair = p.pair("p");
    let behind: Vec<u64> = (0..N).map(|i| p.one(&format!("{i:03}"))).collect();
    p.pump();
    assert!(
        p.told_busy(pair),
        "the pair never met a commit in flight — this test measures nothing"
    );
    let rolled = p.run_past_the_timeout();
    let published = behind.iter().filter(|id| p.published(**id)).count();
    println!(
        "2-key write told Busy, sent {}x, published {} · {published} of {N} behind it published · {rolled} keys rolled back",
        p.sent[&pair],
        p.published(pair)
    );
    assert!(
        p.published(pair),
        "the 2-key write refused Busy was never published (sent {}x)",
        p.sent[&pair]
    );
    assert_eq!(
        published, N,
        "only {published} of {N} writes behind a multi-key Busy published"
    );
    assert_eq!(rolled, 0, "{rolled} keys were rolled back");
}

/// A MIXED burst — single-key and multi-key interleaved — so a fix that only
/// special-cases one shape fails on the other.
#[test]
fn a_mixed_burst_of_single_and_multi_key_writes_all_publish() {
    let mut p = Page::new();
    let mut ids = Vec::new();
    for i in 0..12 {
        ids.push(if i % 3 == 1 {
            p.pair(&format!("{i}"))
        } else {
            p.one(&format!("{i:03}"))
        });
    }
    p.pump();
    let busy = ids.iter().filter(|id| p.told_busy(**id)).count();
    assert!(
        busy >= 4,
        "only {busy} writes met a commit in flight — the burst did not contend"
    );
    let multi_busy = ids
        .iter()
        .enumerate()
        .filter(|(i, id)| i % 3 == 1 && p.told_busy(**id))
        .count();
    assert!(
        multi_busy >= 1,
        "no multi-key write was refused Busy — the mix tests only one shape"
    );
    let rolled = p.run_past_the_timeout();
    let published = ids.iter().filter(|id| p.published(**id)).count();
    println!("mixed burst: {busy} told Busy ({multi_busy} multi-key) · {published} of {} published · {rolled} rolled back", ids.len());
    assert_eq!((published, rolled), (ids.len(), 0));
}

/// THE CONTROL THE FIX MUST NOT BREAK: while a write is awaiting its verdict,
/// the drain sends NOTHING — the engine takes one commit at a time, so a
/// second write would only be refused again. A fix that simply removed the
/// gate passes both tests above and fails this.
/// A page where one write is ACCEPTED and still awaiting its publish, and a
/// 2-key write behind it was refused `Busy` and is queued.
fn a_pair_queued_behind_a_write_in_flight() -> Page {
    let mut p = Page::new();
    // Two writes, the second a pair, delivered TOGETHER and answered once:
    // the first is accepted (and awaits its publish), the pair is Busy.
    let first = p.one("first");
    let pair = p.pair("p");
    let frames = p.store.take_outbound();
    let inbound: Vec<Inbound> = frames.into_iter().map(Inbound::Client).collect();
    let mut accepted_first = false;
    for reply in p.conn.step(inbound) {
        if let Ok(protocol::Reply::WriteState { write_id, state }) = protocol::decode_reply(&reply)
        {
            p.verdicts.entry(write_id).or_default().push(state);
            // Hold back the first write's PUBLISH, so it stays awaiting.
            if write_id == first
                && matches!(
                    state,
                    protocol::WriteState::Published | protocol::WriteState::ParityComplete
                )
            {
                continue;
            }
            if write_id == first && state == protocol::WriteState::Accepted {
                accepted_first = true;
            }
        }
        p.store.on_inbound(&reply);
    }
    assert!(
        accepted_first && p.told_busy(pair),
        "setup: want the first accepted and the pair Busy"
    );
    p
}

#[test]
fn control_the_drain_holds_while_a_write_awaits_its_verdict() {
    let mut p = a_pair_queued_behind_a_write_in_flight();
    assert_eq!(p.store.queued_count(), 1, "setup: the pair is queued");
    p.store.drain_queued();
    assert!(
        p.store.take_outbound().is_empty(),
        "the drain sent a write while another awaited its verdict — it would only be refused again"
    );
}

/// THE BLAST RADIUS, measured: how many keys does each `Db` operation write?
///
/// If one record `put` wrote several keys (the record and its index entries),
/// every contended `put` would have jammed the outbox, not just explicit
/// batches. Today each is ONE key — so the defect needed `apply_batch`. The
/// SDK's own comment says index entries will join the record's write
/// (`Db::write`); when they do, this count moves, and the fix above is what
/// keeps an ordinary `put` from stalling every write behind it.
/// Counts the keys in every write a `Db` hands its store — at the store
/// boundary, which is the one every backend shares.
struct Counting<S> {
    inner: S,
    writes: Vec<usize>,
}
impl<S: Store> Store for Counting<S> {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.writes.push(1);
        self.inner.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> bool {
        self.writes.push(1);
        self.inner.delete(key)
    }
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) {
        self.writes.push(edits.len());
        self.inner.apply_batch(edits)
    }
}
impl<S: Reads> Reads for Counting<S> {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan(lo, hi, reverse, limit)
    }
    fn root(&mut self) -> Read<[u8; 32]> {
        self.inner.root()
    }
    fn changes_since(
        &mut self,
        from: [u8; 32],
        lo: &[u8],
        hi: &[u8],
        max_entries: u32,
    ) -> Read<Delta> {
        self.inner.changes_since(from, lo, hi, max_entries)
    }
}

#[test]
fn how_many_keys_each_db_operation_writes() {
    let mut db = Db::new(
        Counting {
            inner: MemStore::default(),
            writes: vec![],
        },
        SystemEnv,
        [0u8; 4],
    );
    let schema = |parent: Option<&str>| {
        serde_json::from_value::<craftworks_sdk::Schema>(json!({
            "type": "Note",
            "fields": [{"name": "title", "kind": "text"}, {"name": "owner", "kind": "text", "required": parent.is_some()}],
            "parent": parent,
        }))
        .unwrap()
    };
    let mut table: Vec<(String, Vec<usize>)> = Vec::new();
    let take = |db: &mut Db<Counting<MemStore>, SystemEnv>,
                op: String,
                table: &mut Vec<(String, Vec<usize>)>| {
        table.push((op, std::mem::take(&mut db.store_mut().writes)));
    };
    for (label, domain, parent) in [
        ("plain", "notes", None),
        ("parent-keyed", "kids", Some("owner")),
    ] {
        db.define(domain, &schema(parent)).unwrap();
        take(&mut db, format!("{label} define"), &mut table);
        let mut f = Map::new();
        f.insert("title".into(), Value::from("t"));
        f.insert(
            "owner".into(),
            Value::from("00112233445566778899aabbccddeeff"),
        );
        let rec = db.put(domain, &f).unwrap();
        take(&mut db, format!("{label} put"), &mut table);
        let loc = loc_from_hex(&rec.id).unwrap();
        let mut patch = Map::new();
        patch.insert("title".into(), Value::from("u"));
        db.update(domain, loc, &patch).unwrap();
        take(&mut db, format!("{label} update"), &mut table);
        db.delete(domain, loc).unwrap();
        take(&mut db, format!("{label} delete"), &mut table);
    }
    for (op, writes) in &table {
        println!("  {op:<22} keys per write: {writes:?}");
    }
    assert_eq!(table.len(), 8, "every operation was counted");
    for (op, writes) in &table {
        assert_eq!(
            writes,
            &vec![1],
            "{op}: expected ONE write of ONE key, got {writes:?} — see this test's doc"
        );
    }
}

/// THE UNITS, asserted rather than described: one 2-key write is TWO entries
/// and ONE write. The gate compares writes with writes; a comparison of either
/// with the other is the defect.
#[test]
fn a_two_key_write_is_two_entries_and_one_write() {
    let p = a_pair_queued_behind_a_write_in_flight();
    // Held: the single-key write in flight (1 entry) and the queued pair (2).
    let c = &p.store.copy;
    assert_eq!(
        c.pending().0,
        3,
        "pending() counts (key, write) ENTRIES: 1 + 2"
    );
    assert_eq!(
        c.pending_writes(),
        2,
        "pending_writes() counts WRITES: the one in flight and the pair"
    );
    assert_eq!(
        c.queued_count(),
        1,
        "queued_count() counts WRITES: the pair"
    );
}
