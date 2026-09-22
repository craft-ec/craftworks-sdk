//! sdk#266: a tab that ADOPTS another tab's head reads data at least as new
//! as that head. "Read when needed" is not "read what I happened to cache":
//! a range loaded before the head moved is BEHIND it, and answering from it
//! is answering with a tree the page itself has stopped standing on.
//!
//! The fix is a DELTA, not a reload: the rows are still here and the copy
//! knows the root it read them at, so the next read of a stale range asks
//! what changed since that root, under its own ticket.

use craftworks_sdk::{CachedStore, DbError, Loads, Outcome, Reads, Store};
use protocol::Request;
use testkit::PageNode;

/// One tab: a store, its page, and the read bookkeeping the Session holds.
struct Tab {
    store: CachedStore,
    conn: testkit::PageConn,
    loads: Loads,
}

impl Tab {
    fn open(node: &PageNode) -> Tab {
        let (mut store, _clock) = testkit::cached_store();
        let conn = node.connect();
        store.client.send(&Request::Identity);
        let mut t = Tab { store, conn, loads: Loads::new() };
        t.pump();
        t
    }

    /// Everything the store queued goes out; every reply comes back — and a
    /// reply that answers a READ's ticket is routed as the Session routes it.
    fn pump(&mut self) {
        for _ in 0..100 {
            let frames = self.store.take_outbound();
            if std::env::var("T266").is_ok() {
                for f in &frames {
                    if let protocol::Incoming::Ok(env) = protocol::decode_request(f) {
                        println!("    sent {}", format!("{:?}", env.body).chars().take(90).collect::<String>());
                    }
                }
            }
            if frames.is_empty() {
                return;
            }
            for r in self.conn.frames(&frames) {
                self.on_reply(&r);
            }
        }
    }

    fn on_reply(&mut self, bytes: &[u8]) {
        if std::env::var("T266").is_ok() {
            if let Ok(r) = protocol::decode_reply(bytes) {
                let s = format!("{r:?}");
                println!("    reply {}", s.chars().take(90).collect::<String>());
            }
        }
        match protocol::decode_reply(bytes) {
            Ok(protocol::Reply::Page { req_id, entries, cursor, at, .. }) => {
                if let craftworks_sdk::loads::Page::Complete { lo, hi, rows, at } = self.loads.on_page(req_id, entries, cursor, at) {
                    self.store.on_page(&lo, &hi, rows, at.root);
                }
            }
            Ok(protocol::Reply::Delta { req_id, changes, cursor, new_root, .. }) => {
                craftworks_sdk::parking::delta_for_read(&mut self.loads, &mut self.store, req_id, changes, cursor, new_root);
            }
            Ok(protocol::Reply::FullReloadRequired { req_id, .. }) => {
                craftworks_sdk::parking::full_reload_for_read(&mut self.loads, &mut self.store, req_id);
            }
            _ => {}
        }
        self.store.on_inbound(bytes);
    }

    /// A READ the app made, driven to an answer exactly as the Session drives
    /// it: `decide_with` turns a `NotLoaded` into a request and a ticket.
    fn read(&mut self, lo: &[u8], hi: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        for _ in 0..8 {
            let r = Reads::scan(&mut self.store, lo, hi, false, usize::MAX).map_err(|e| match e {
                craftworks_sdk::StoreError::NotLoaded => DbError::NotLoaded { lo: lo.to_vec(), hi: hi.to_vec() },
                other => DbError::Refused(other.to_string()),
            });
            match craftworks_sdk::decide_with(&mut self.loads, &mut self.store, r, self.conn.now_ms(), None) {
                Outcome::Done(rows) => return rows,
                Outcome::Told(e) => panic!("the read was refused: {e}"),
                // Time passes while a read is parked, as it does in a page:
                // the engine's own work (reading the head it has adopted) is
                // driven by the tick.
                Outcome::Wait(_, _) => {
                    self.pump();
                    let now = self.conn.now_ms() + 1_000;
                    for r in self.conn.tick_at(now) {
                        self.on_reply(&r);
                    }
                    self.pump();
                }
            }
        }
        panic!("the read never came back");
    }

    /// What the page reports and the Session acts on: a head adopted from
    /// somebody else makes every loaded range stale.
    fn adopt(&mut self) -> bool {
        let mut adopted = false;
        for t in 1..=20u64 {
            let now = self.conn.now_ms() + t * 1_000;
            for r in self.conn.tick_at(now) {
                self.on_reply(&r);
            }
            self.pump();
            adopted |= self.conn.with_server(|s| s.take_adopted());
        }
        // What the Session does with it.
        if adopted {
            self.store.mark_stale();
        }
        adopted
    }

    /// Frames this tab sent since the last call, as their request names.
    fn asked(&mut self) -> Vec<&'static str> {
        self.store
            .take_outbound()
            .iter()
            .filter_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(env) => Some(match env.body {
                    Request::Range { .. } => "Range",
                    Request::ChangesSince { .. } => "ChangesSince",
                    Request::Get { .. } => "Get",
                    _ => "other",
                }),
                _ => None,
            })
            .collect()
    }
}

/// THE CASE (sdk#266, the architect's test): y has read the range, x writes a
/// row, y adopts x's head — and y's next explicit read shows x's row. No
/// reload, no binding, no subscription: a read, answered with data at least
/// as new as the head y stands on. The re-ask is a DELTA.
#[test]
fn a_tab_that_adopted_another_s_head_reads_its_row_without_a_reload() {
    let node = PageNode::new();
    let (mut x, mut y) = (Tab::open(&node), Tab::open(&node));
    // y has the range loaded and sees nothing in it.
    assert_eq!(y.read(b"r/", b"s/"), vec![], "the range did not start empty");
    let _ = y.asked();
    // x writes a row and publishes it.
    x.store.put(b"r/1", b"x's row").expect("x takes the write");
    x.pump();
    for t in 1..=5u64 {
        for r in x.conn.tick_at(x.conn.now_ms() + t * 1_000) {
            x.on_reply(&r);
        }
        x.pump();
    }
    assert!(node.tree(&node.head().expect("a head").1).expect("the tree").contains_key(b"r/1".as_slice()), "x's row never reached the tree");

    // y adopts that head. THE CONTROL: adopting asks for NOTHING by itself —
    // a stale range is re-asked when something READS it, never on the head
    // move ("not live = read when needed", the owner's rule).
    assert!(y.adopt(), "y never adopted x's head, so this run says nothing about reading at it");
    assert_eq!(y.asked(), Vec::<&str>::new(), "adopting a head sent a read nobody asked for");

    let rows = y.read(b"r/", b"s/");
    assert_eq!(
        rows.iter().map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), String::from_utf8_lossy(v).into_owned())).collect::<Vec<_>>(),
        vec![("r/1".to_string(), "x's row".to_string())],
        "y's own read did not show the row x wrote at the head y has adopted (sdk#266)"
    );
}

/// The other half of the ruling: a page's OWN commit must not make its
/// ranges stale. The copy already holds what it wrote, and re-asking after
/// every write would double the read traffic of ordinary writing.
#[test]
fn a_tab_s_own_commit_does_not_make_its_ranges_stale() {
    let node = PageNode::new();
    let mut x = Tab::open(&node);
    assert_eq!(x.read(b"r/", b"s/"), vec![], "the range did not start empty");
    x.store.put(b"r/1", b"mine").expect("taken");
    x.pump();
    for t in 1..=5u64 {
        for r in x.conn.tick_at(x.conn.now_ms() + t * 1_000) {
            x.on_reply(&r);
        }
        x.pump();
    }
    assert!(!x.conn.with_server(|s| s.take_adopted()), "a page called its OWN commit an adoption: every write would re-ask every range");
    let _ = x.asked();
    let rows = x.read(b"r/", b"s/");
    assert_eq!(rows.len(), 1, "the tab cannot see its own row: {rows:?}");
    assert_eq!(x.asked(), Vec::<&str>::new(), "reading after its own commit went to the node");
}
