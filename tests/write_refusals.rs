//! craftworks-sdk#136: a write the system can NEVER take is refused ONCE, with
//! its reason, and the writes behind it go on.
//!
//! Driven through the path an app takes -- `CachedStore` over the real engine
//! (`PageNode`) -- with a tick every second past `pending_timeout_ms`.
//!
//! Measured before the fix, on this harness:
//!   * 200 x 2 KiB in one write, then 10 ordinary writes: the batch was
//!     answered `Busy` (the word for "try again") and the outbox, once #141
//!     let it drain, re-sent it oldest-first for ever;
//!   * a write exactly at the copy's 4 MiB bound encoded to an EMPTY frame,
//!     was answered "Unparseable" with no id, held the copy's whole byte
//!     budget for a minute -- every write made meanwhile refused -- and was
//!     then rolled back.

use craftworks_sdk::copy::Refused;
use craftworks_sdk::store::{Edit, Store as _};
use craftworks_sdk::CachedStore;
use std::collections::{BTreeMap, BTreeSet};
use testkit::page_node::{PageConn, PageNode};

struct Page {
    store: CachedStore,
    clock: testkit::Clock,
    _node: PageNode,
    conn: PageConn,
    /// Per write id: the keys each SEND carried, bulk keys left out.
    sends: BTreeMap<u64, Vec<Vec<String>>>,
    verdicts: BTreeMap<u64, Vec<protocol::WriteState>>,
    /// Frames the page put on the wire, of any kind, counted raw.
    frames: usize,
}

impl Page {
    fn new() -> Page {
        let node = PageNode::new();
        let conn = node.connect();
        let (store, clock) = testkit::cached_store();
        let mut p = Page {
            store,
            clock,
            _node: node,
            conn,
            sends: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            frames: 0,
        };
        p.store.client.send(&protocol::Request::Identity);
        p.pump();
        p
    }

    /// Everything waiting, delivered together, to a standstill.
    fn pump(&mut self) {
        for _ in 0..200 {
            let frames = self.store.take_outbound();
            if frames.is_empty() {
                return;
            }
            self.frames += frames.len();
            for f in &frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(f) {
                    if let protocol::Request::Write { write_id, ops } = env.body {
                        let keys = ops
                            .iter()
                            .map(|o| match o {
                                protocol::Op::Put(k, _) | protocol::Op::Delete(k) => {
                                    String::from_utf8_lossy(k).into_owned()
                                }
                            })
                            .filter(|k| !k.starts_with("d/bulk/"))
                            .collect();
                        self.sends.entry(write_id).or_default().push(keys);
                    }
                }
            }
            for reply in self
                .conn
                .frames(&frames)
            {
                if let Some((write_id, state)) = protocol::decode_reply(&reply)
                    .ok()
                    .and_then(|r| self.store.client.own_write_state(&r))
                {
                    self.verdicts.entry(write_id).or_default().push(state);
                }
                self.store.on_inbound(&reply);
            }
        }
        panic!("the page never reached a standstill");
    }

    /// A tick a second to past the timeout; the keys rolled back meanwhile.
    fn past_the_timeout(&mut self) -> BTreeSet<Vec<u8>> {
        let mut rolled = BTreeSet::new();
        for _ in 0..61 {
            self.clock.advance(1000);
            rolled.extend(self.store.tick().rolled_back_keys);
            self.pump();
        }
        rolled
    }

    fn published(&self, id: u64) -> bool {
        use protocol::WriteState as W;
        self.verdicts.get(&id).is_some_and(|v| {
            v.iter()
                .any(|s| matches!(s, W::Published | W::ParityComplete))
        })
    }

    fn one(&mut self, key: &str) -> u64 {
        let id = self.store.next_write_id();
        self.store.put(key.as_bytes(), b"x").expect("the store took the write");
        id
    }

    /// 200 DISTINCT 2 KiB values in ONE write, plus `extra` keys: over the
    /// engine's commit-block budget (206 blocks against 128, measured).
    fn bulk(&mut self, extra: &[&str]) -> u64 {
        let id = self.store.next_write_id();
        let mut edits: Vec<(Vec<u8>, Edit)> = (0..200u64)
            .map(|i| {
                let mut v = vec![0u8; 2048];
                v[..8].copy_from_slice(&i.to_le_bytes());
                (format!("d/bulk/{i:05}").into_bytes(), Edit::Put(v))
            })
            .chain(extra.iter().map(|k| {
                (
                    k.as_bytes().to_vec(),
                    Edit::Put(b"from the bulk write".to_vec()),
                )
            }))
            .collect();
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        self.store.apply_batch(&edits).expect("the store took the write");
        id
    }
}

/// THE ACCEPTANCE: the never-acceptable write is told so ONCE, is never
/// re-sent, rolls back with its reason -- and ten ordinary writes behind it
/// all publish, as a count.
#[test]
fn a_write_the_engine_can_never_accept_is_refused_once_and_the_writes_behind_it_publish() {
    const N: usize = 10;
    let mut p = Page::new();
    // An ordinary commit in flight FIRST, so the bulk write is refused `Busy`
    // and QUEUED, and the ten behind it queue behind IT -- the cascade shape:
    // when the first write publishes, the drain re-sends the bulk write, and
    // what happens next decides whether anything behind it ever goes.
    let first = p.one("d/notes/first");
    let big = p.bulk(&[]);
    let behind: Vec<u64> = (0..N).map(|i| p.one(&format!("d/notes/{i:03}"))).collect();
    p.pump();
    // BEFORE ANY TICK: the refusal itself lets the next write go. The tick
    // would drain it eventually, which is why this is asserted here and not
    // only after the timeout -- a refusal that left the queue stopped until
    // the next tick passes the later check.
    let before_any_tick = behind.iter().filter(|id| p.published(**id)).count();
    assert_eq!(before_any_tick, N, "only {before_any_tick} of {N} published before a tick: the refusal left the outbox stopped");
    let rolled = p.past_the_timeout();

    assert!(p.published(first));
    let told = p.verdicts.get(&big).cloned().unwrap_or_default();
    assert!(
        matches!(told.as_slice(), [protocol::WriteState::Busy, protocol::WriteState::TooLarge { bound: protocol::WriteBound::CommitBlocks, limit: 128, got }] if *got > 128),
        "the bulk write was told {told:?}"
    );
    assert_eq!(
        p.sends[&big].len(),
        2,
        "the bulk write was not sent exactly twice (Busy, then TooLarge -- and never again)"
    );
    assert!(
        p.store
            .refused
            .iter()
            .any(|(id, r)| *id == big && matches!(r, Refused::TooLarge { .. })),
        "the reason never reached the app: {:?}",
        p.store.refused
    );
    let published = behind.iter().filter(|id| p.published(**id)).count();
    assert_eq!(
        published, N,
        "{published} of {N} writes behind the refused one published"
    );
    assert!(
        rolled.iter().all(|k| k.starts_with(b"d/bulk/")),
        "a write that is not the refused one was rolled back: {:?}",
        rolled
            .iter()
            .filter(|k| !k.starts_with(b"d/bulk/"))
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect::<Vec<_>>()
    );
    println!("  refused once: {told:?}; {published} of {N} behind it published");
}

/// CONDITION 2: B, queued behind the refused A, writes one of A's keys.
///
/// B was made on top of A, so it rolls back WITH A -- `RolledBack::AfterFailed`
/// (`copy::fall`), the rule a `Failed` predecessor already follows -- and it
/// rolls back WHOLE. The multi-key arm is the one that was broken: B lost only
/// its entry on the shared key and was re-sent with the rest, so half of one
/// atomic write published (measured: sent `[other, shared]`, then `[other]`).
/// A write C on a key neither touches is unaffected and publishes.
#[test]
fn a_queued_write_sharing_a_key_with_a_refused_one_rolls_back_with_it_whole() {
    for multi in [false, true] {
        let mut p = Page::new();
        p.one("d/notes/first"); // a commit in flight, so A and B queue
        let a = p.bulk(&["d/shared"]);
        let b = p.store.next_write_id();
        if multi {
            p.store.apply_batch(&[
                (b"d/other".to_vec(), Edit::Put(b"from B".to_vec())),
                (b"d/shared".to_vec(), Edit::Put(b"from B".to_vec())),
            ]).expect("the store took the write");
        } else {
            p.store.put(b"d/shared", b"from B").expect("the store took the write");
        }
        let c = p.one("d/elsewhere");
        p.pump();
        let _ = p.past_the_timeout();

        assert!(
            p.verdicts[&a]
                .iter()
                .any(|s| matches!(s, protocol::WriteState::TooLarge { .. })),
            "multi={multi}: A was not refused TooLarge"
        );
        let b_sends = p.sends.get(&b).cloned().unwrap_or_default();
        assert!(
            b_sends
                .iter()
                .all(|keys| keys.iter().any(|k| k == "d/shared")),
            "multi={multi}: B was sent WITHOUT the shared key -- a partial write: {b_sends:?}"
        );
        assert!(
            !p.published(b),
            "multi={multi}: B published although it was made on top of a refused write"
        );
        assert!(
            p.store.copy.get(b"d/shared").is_none() && p.store.copy.get(b"d/other").is_none(),
            "multi={multi}: B's edit is still shown"
        );
        assert!(
            p.published(c),
            "multi={multi}: an unrelated write behind them did not publish"
        );
    }
}

/// THE WIRE BOUND, found by tracing MAX_MESSAGE: a write the copy accepts but
/// that cannot be SENT is refused on the spot, before anything is held.
#[test]
fn a_write_too_large_to_send_is_refused_before_it_is_held_or_sent() {
    let mut p = Page::new();
    let key = b"d/notes/big";
    let frames_before = p.frames;
    let id = p.store.next_write_id();
    // Exactly the copy's bound: key + value = 4 MiB, which the copy allows.
    let r = p.store.put(key, &vec![7u8; 4 * 1024 * 1024 - key.len()]);
    assert!(matches!(r, Err(Refused::TooLargeToSend { .. })), "the refusal was not returned: {r:?}");
    assert!(
        matches!(p.store.refused.last(), Some((rid, Refused::TooLargeToSend { bytes, limit })) if *rid == id && *bytes > *limit as u64),
        "not refused as too large to send: {:?}",
        p.store.refused
    );
    assert_eq!(
        p.store.copy.pending_writes(),
        0,
        "the refused write is held, taking the copy's budget"
    );
    p.pump();
    assert_eq!(
        p.frames, frames_before,
        "something was sent for a write that cannot be sent"
    );
    // The next write is not refused and publishes.
    let next = p.one("d/notes/after");
    p.pump();
    let _ = p.past_the_timeout();
    assert!(
        p.published(next),
        "the write after the refused one did not publish"
    );

    // CONTROL: 1 KiB under the WIRE limit is sent and answered -- so the
    // refusal above is the wire bound, not a refusal of big writes.
    let mut q = Page::new();
    let id = q.store.next_write_id();
    let wire_room = protocol::MAX_MESSAGE - 1024 - 64;
    q.store.put(key, &vec![7u8; wire_room]).expect("the store took the write");
    assert!(
        q.store.refused.is_empty(),
        "a write under the wire limit was refused: {:?}",
        q.store.refused
    );
    q.pump();
    assert_eq!(
        q.sends.get(&id).map(Vec::len),
        Some(1),
        "the control write was not sent"
    );
    assert!(
        q.verdicts.contains_key(&id),
        "the control write got no answer at all"
    );
    println!("  control under the wire limit: told {:?}", q.verdicts[&id]);
}
