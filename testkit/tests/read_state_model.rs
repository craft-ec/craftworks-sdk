//! READ-STATE's MODEL TEST (craftworks-docs docs/design/READ-STATE.md,
//! § The model test). Written BEFORE slice R's code, against today's read
//! path, and red there on the defects it exists to see.
//!
//! A seeded PRNG (splitmix64) drives ONE node and two tabs of one person:
//! tab x WRITES (so the head keeps moving) and tab y READS random ranges while
//! it adopts x's heads. The ORACLE is the node's own tree at the head y has
//! adopted — read from the node, never from y. A read is right when it equals
//! the tree at the head y stood on when the read began, or at any head y
//! adopted while it ran (a read may finish after one more adoption).
//!
//! Checked on every read: it equals the oracle (inv. 2), and it ENDS — an
//! answer within a bounded number of attempts, never a ticketless refusal or a
//! loop (inv. 8). Checked at rest: once x stops and y has adopted the final
//! head, y's full read equals the node's final tree.
//!
//! THE READER IS BEHIND A TRAIT, so the same model runs today's path (a row
//! copy + range loads, driven the way `web::Session` routes them) and slice R's
//! (a walk from the engine's head) without changing a check. The test does
//! not change when the code does.
//!
//! THE LOOP RULE (as `write_path_model.rs`): every loop that waits advances a
//! clock or is bounded, and fails by name.

use craftworks_sdk::loads::Page;
use craftworks_sdk::{CachedStore, DbError, Loads, Outcome, Reads, StoreError};
use protocol::{Reply, Request};
use std::collections::BTreeMap;
use testkit::page_node::{PageConn, PageNode};

// ---------------------------------------------------------------- the dice

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Keys the model writes and reads: a small pool, so ranges overlap writes.
const KEYS: u64 = 48;
fn key(i: u64) -> Vec<u8> {
    format!("k/{i:03}").into_bytes()
}

/// The ranges a reader asks for: the whole space, halves, and a narrow slice —
/// wide loads and narrow reads is the shape that broke today (defect 5).
fn range(r: &mut Rng) -> (Vec<u8>, Vec<u8>) {
    match r.below(4) {
        0 => (b"k/".to_vec(), b"l/".to_vec()),
        1 => (key(0), key(KEYS / 2)),
        2 => (key(KEYS / 2), b"l/".to_vec()),
        _ => {
            let a = r.below(KEYS - 4);
            (key(a), key(a + 4))
        }
    }
}

// --------------------------------------------------------------- the reader

/// How a read ended when it did not answer.
#[derive(Debug, Clone, PartialEq)]
enum Fail {
    /// Refused with nothing to wait on (a ticketless NotLoaded).
    Ticketless(String),
    /// Still not answered after the bounded number of attempts: a loop.
    Stuck(usize),
    /// Refused for another reason.
    Other(String),
}

/// Rows in key order, as a read answers them.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// The reader under test. Today's path and slice R's implement the same four
/// verbs, and the model knows nothing else about them.
trait Reader {
    /// Read `[lo, hi)`: the rows, or how it failed to answer.
    fn read(&mut self, lo: &[u8], hi: &[u8]) -> Result<Rows, Fail>;
    /// The head this tab stands on (its adopted, published head).
    fn head(&self) -> (u64, [u8; 32]);
    /// A head move was announced (the subscription's HeadChanged): re-read the
    /// head and adopt it, as the page does.
    fn hint(&mut self, now_ms: u64);
    /// Time passes.
    fn tick(&mut self, now_ms: u64);
}

/// TODAY'S read path: a `CachedStore` row copy with range loads and stale
/// ranges, routed exactly as `web::Session` routes them (`on_inbound` → Page /
/// Delta / FullReloadRequired / Unavailable; `pump_page` → `take_adopted` →
/// `mark_stale`; reads through `parking::decide_with`).
struct Today {
    store: CachedStore,
    loads: Loads,
    conn: PageConn,
}

impl Today {
    fn open(node: &PageNode) -> Today {
        let (mut store, _clock) = testkit::cached_store();
        store.client.send(&Request::Identity);
        let mut t = Today {
            store,
            loads: Loads::new(),
            conn: node.connect(),
        };
        t.pump();
        t
    }

    fn pump(&mut self) {
        for _ in 0..400 {
            let frames = self.store.take_outbound();
            if frames.is_empty() {
                break;
            }
            for r in self.conn.frames(&frames) {
                self.on_reply(&r);
            }
        }
        // `pump_page`'s adoption hook: a head this page did not commit makes
        // every loaded range stale (sdk#266).
        if self.conn.with_server(|s| s.take_adopted()) {
            self.store.mark_stale();
        }
    }

    fn on_reply(&mut self, bytes: &[u8]) {
        match protocol::decode_reply(bytes) {
            Ok(Reply::Page {
                req_id,
                entries,
                cursor,
                at,
                ..
            }) => match self.loads.on_page(req_id, entries, cursor, at) {
                Page::More { lo, hi, after } => {
                    self.store
                        .client
                        .send(&Loads::range_request(req_id, &lo, &hi, Some(after)))
                }
                Page::Complete { lo, hi, rows, at } => self.store.on_page(&lo, &hi, rows, at.root),
                Page::Restart { lo, hi } => self
                    .store
                    .client
                    .send(&Loads::range_request(req_id, &lo, &hi, None)),
                Page::Nothing => {}
            },
            Ok(Reply::Delta {
                req_id,
                changes,
                cursor,
                new_root,
                at,
            }) => {
                self.loads.note_seq(at.seq);
                craftworks_sdk::parking::delta_for_read(
                    &mut self.loads,
                    &mut self.store,
                    req_id,
                    changes,
                    cursor,
                    new_root,
                );
            }
            Ok(Reply::FullReloadRequired { req_id, .. }) => {
                craftworks_sdk::parking::full_reload_for_read(
                    &mut self.loads,
                    &mut self.store,
                    req_id,
                );
            }
            Ok(Reply::Unavailable { req_id, .. }) => self.loads.on_unavailable(req_id),
            _ => {}
        }
        self.store.on_inbound(bytes);
    }
}

/// How many times one read may go round before it is a loop. Generous: an
/// honest read needs a schema-free range load, possibly a restart or a delta
/// re-ask — single digits. Twenty is a loop.
const READ_ROUNDS: usize = 20;

impl Reader for Today {
    fn read(&mut self, lo: &[u8], hi: &[u8]) -> Result<Rows, Fail> {
        for round in 0..READ_ROUNDS {
            let r = Reads::scan(&mut self.store, lo, hi, false, usize::MAX).map_err(|e| match e {
                StoreError::NotLoaded => DbError::NotLoaded {
                    lo: lo.to_vec(),
                    hi: hi.to_vec(),
                },
                other => DbError::Refused(other.to_string()),
            });
            match craftworks_sdk::decide_with(
                &mut self.loads,
                &mut self.store,
                r,
                round as u64,
                None,
            ) {
                Outcome::Done(rows) => return Ok(rows),
                Outcome::Told(DbError::NotLoaded { .. }) => {
                    return Err(Fail::Ticketless(format!("round {round}")))
                }
                Outcome::Told(e) => return Err(Fail::Other(e.to_string())),
                Outcome::Wait(_, _) => self.pump(),
            }
        }
        Err(Fail::Stuck(READ_ROUNDS))
    }

    fn head(&self) -> (u64, [u8; 32]) {
        self.conn.with_server(|s| s.page.published())
    }

    fn hint(&mut self, now_ms: u64) {
        self.conn.with_server(|s| s.head_hint());
        self.tick(now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        for r in self.conn.tick_at(now_ms) {
            self.on_reply(&r);
        }
        self.pump();
    }
}

// ------------------------------------------------------------------ the run

#[derive(Debug)]
struct Finding {
    class: &'static str,
    detail: String,
}

/// One seeded run. Returns what the model found.
fn run<R: Reader>(seed: u64, open: impl Fn(&PageNode) -> R, steps: usize) -> Vec<Finding> {
    let mut rng = Rng(seed);
    let node = PageNode::new();
    let mut x = node.connect();
    x.client(&Request::Identity);
    let mut y = open(&node);
    let mut found = Vec::new();
    let mut now = 1_000u64;
    let mut write_id = 0u64;
    for step in 0..steps {
        now += 250;
        match rng.below(10) {
            // x WRITES: the head moves. One to three keys, puts and deletes.
            0..=3 => {
                write_id += 1;
                let n = 1 + rng.below(3);
                let ops = (0..n)
                    .map(|_| {
                        let k = key(rng.below(KEYS));
                        if rng.below(5) == 0 {
                            protocol::Op::Delete(k)
                        } else {
                            protocol::Op::Put(k, format!("v{step}").into_bytes())
                        }
                    })
                    .collect();
                x.client(&Request::Write { write_id, ops });
                x.tick_at(now);
            }
            // y is told the head moved, and adopts it.
            4..=5 => y.hint(now),
            // time passes for y.
            6 => y.tick(now),
            // y READS.
            _ => {
                let (lo, hi) = range(&mut rng);
                let before = y.head();
                let got = y.read(&lo, &hi);
                let after = y.head();
                match got {
                    Err(f) => found.push(Finding {
                        class: "READ DID NOT ANSWER",
                        detail: format!(
                            "seed {seed} step {step}: [{}, {}) — {f:?}",
                            show(&lo),
                            show(&hi)
                        ),
                    }),
                    Ok(rows) => {
                        // The oracle: the node's tree at the head y stood on
                        // when the read began, or at the head it had adopted
                        // when the read ended.
                        // A head at seq 0 is the EMPTY tree: nothing is on the
                        // network for it, so the node holds no tree to read —
                        // that is empty, not unanswerable. Any OTHER root the
                        // node cannot read is the harness's fault, named.
                        let want = |(seq, root): (u64, [u8; 32])| -> Option<Rows> {
                            if seq == 0 {
                                return Some(Vec::new());
                            }
                            let tree = node.tree(&root)?;
                            Some(
                                tree.range(lo.clone()..hi.clone())
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            )
                        };
                        if want(before).is_none() && want(after).is_none() {
                            found.push(Finding {
                                class: "HARNESS: THE ORACLE CANNOT READ Y'S HEAD",
                                detail: format!("seed {seed} step {step}: seq {}", before.0),
                            });
                            continue;
                        }
                        let ok = [before, after]
                            .iter()
                            .any(|h| want(*h).as_ref() == Some(&rows));
                        if !ok {
                            let w = want(before).map(|v| v.len());
                            found.push(Finding {
                                class: "READ IS NOT THE TREE AT ITS HEAD",
                                detail: format!("seed {seed} step {step}: [{}, {}) got {} rows, the tree at y's head has {w:?}", show(&lo), show(&hi), rows.len()),
                            });
                        }
                    }
                }
            }
        }
    }
    // AT REST: x stops; y adopts until its head is the node's, then reads all.
    let target = node.head().map(|h| h.1);
    for _ in 0..60 {
        now += 1_000;
        x.tick_at(now);
        y.hint(now);
        if Some(y.head().1) == node.head().map(|h| h.1) {
            break;
        }
    }
    if Some(y.head().1) != target.or(node.head().map(|h| h.1)) {
        found.push(Finding {
            class: "Y NEVER ADOPTED THE FINAL HEAD",
            detail: format!("seed {seed}"),
        });
    } else {
        match y.read(b"k/", b"l/") {
            Err(f) => found.push(Finding {
                class: "READ DID NOT ANSWER",
                detail: format!("seed {seed} at rest: {f:?}"),
            }),
            Ok(rows) => {
                let tree: Vec<(Vec<u8>, Vec<u8>)> = node
                    .tree(&y.head().1)
                    .map(|t| t.into_iter().collect())
                    .unwrap_or_default();
                if rows != tree {
                    found.push(Finding {
                        class: "AT REST, Y'S READ IS NOT THE FINAL TREE",
                        detail: format!("seed {seed}: {} rows vs {}", rows.len(), tree.len()),
                    });
                }
            }
        }
    }
    found
}

fn show(k: &[u8]) -> String {
    String::from_utf8_lossy(k).into_owned()
}

fn sweep<R: Reader>(
    seeds: std::ops::Range<u64>,
    open: impl Fn(&PageNode) -> R + Copy,
    steps: usize,
) -> BTreeMap<&'static str, (usize, String)> {
    let mut by: BTreeMap<&'static str, (usize, String)> = BTreeMap::new();
    for seed in seeds {
        let mut seen = std::collections::BTreeSet::new();
        for f in run(seed, open, steps) {
            if seen.insert(f.class) {
                let e = by.entry(f.class).or_insert((0, f.detail.clone()));
                e.0 += 1;
            }
        }
    }
    by
}

/// TODAY'S PATH IS RED — by design, and pinned. The model exists to see the
/// defects READ-STATE.md names (4: a full load leaves a stale range stale → a
/// read loops; 5: a wide stale range survives a narrow delta / a reset keeps
/// the stale list; 6: the copy-wide wipe on any root change under continuous
/// writes). If this stops finding them on today's path, the model has gone
/// blind — not the code fixed. Slice R's reader must run this SAME model green
/// (`slice_r_reader_is_green`, added with R).
#[test]
fn todays_read_path_is_red_where_read_state_says() {
    let found = sweep(0..40, Today::open, 120);
    for (class, (runs, first)) in &found {
        println!("  {class}: {runs} of 40 seeds — first: {first}");
    }
    assert!(
        !found.is_empty(),
        "the model found NOTHING on today's read path, which READ-STATE.md shows loops and wipes under exactly this load — the model is blind"
    );
}
