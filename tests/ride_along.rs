//! THE RIDE-ALONG (sdk#399 step 4, OBSERVABILITY §3): the page's filtered detail records go into the person's
//! OBSERVATION tree as its own commit, and only after a DATA head lands: at most one per W_ride (60 s) of saving,
//! never from a read, a tick or a timer, one observation op on the wire at a time, and a page not served from a site
//! writes none at all. Driven through the real page and store over testkit's node, which now serves the observation
//! register too.
//!
//! A record exists only for a window with something PUBLISHABLE in it (the filter keeps operations that did not end
//! Ok); a well-behaved node leaves every window empty. So each test makes a window's content the way a person's page
//! gets it: a slow node -- a head read the node holds past its deadline (a timeout), then answers.

use craftworks_sdk::store::{Reads, Store};
use craftworks_sdk::PageStore;
use testkit::page_node::Served;
use testkit::{PageConn, PageNode};

const SITE: [u8; 32] = [9; 32];
const MINUTE: u64 = 60_000;

/// A recording page over a fresh node; `site`: served from a site (so it has an observation tree).
fn page(site: bool) -> (PageStore<PageConn>, PageConn, PageNode) {
    let node = PageNode::new();
    let (mut s, conn, _clock) = testkit::page_store(&node);
    conn.with_server(|sv| {
        sv.page.record_into(4096);
        if site {
            sv.page.set_obs_site(SITE);
            sv.page.open_obs();
        }
    });
    s.sync();
    (s, conn, node)
}

/// Every observation-register op the node served (its reads, signs and UPDATEs).
fn obs_ops(conn: &PageConn) -> usize {
    conn.served(Served::ObsReadHead) + conn.served(Served::ObsSign) + conn.served(Served::ObsHead)
}

/// (records waiting, observation commits started, most observation ops ever on the wire at once).
fn obs(conn: &PageConn) -> (usize, u64, usize) {
    conn.with_server(|sv| sv.page.obs_counts())
}

/// A SLOW NODE in this window: a head read held past its deadline (it times out and is re-sent), then answered.
fn slow_read(conn: &mut PageConn) {
    conn.hold_answers();
    conn.with_server(|sv| sv.page.head_hint());
    let now = conn.now_ms();
    conn.tick_at(now + 20_000);
    conn.stop_holding();
    while conn.held() > 0 {
        conn.release_one();
    }
}

/// The page's clock to the next minute (the open window closes).
fn next_minute(conn: &mut PageConn) {
    let now = conn.now_ms();
    conn.tick_at((now / MINUTE + 1) * MINUTE + 1);
}

/// One save, run until the node has answered everything (its head lands).
fn save(s: &mut PageStore<PageConn>, i: u32) {
    s.put(format!("d/a/{i:04}").as_bytes(), b"a row").expect("taken");
    s.sync();
}

/// **T1 + GENESIS: no observation op before the first DATA head lands; then the records ride, and the observation
/// register's first read (never written: NotFound) makes the commit a genesis.**
#[test]
fn nothing_goes_to_the_observation_tree_before_a_data_head_lands_then_the_records_ride() {
    let (mut s, mut conn, node) = page(true);
    for _ in 0..10 {
        slow_read(&mut conn);
        next_minute(&mut conn);
    }
    let (pending, commits, _) = obs(&conn);
    assert!(pending >= 5, "THE CONTROL: only {pending} records were made in ten slow minutes -- the test has no records to ride");
    assert_eq!((obs_ops(&conn), commits), (0, 0), "an observation op went out before any data head landed (ten minutes of reads and ticks)");
    assert_eq!(node.obs_head(), None);
    save(&mut s, 1);
    assert!(conn.served(Served::ObsReadHead) >= 1, "the observation register was never read before its first commit");
    let (seq, _) = node.obs_head().expect("the records did not ride the first data landing");
    assert_eq!(seq, 1, "the first observation commit is not the register's genesis");
    // T6: signed under the OBSERVATION label and written to its register -- the data register's signs and UPDATEs
    // are the data commit's alone (one each here).
    assert!(conn.served(Served::ObsSign) >= 1 && conn.served(Served::ObsHead) >= 1, "the observation head was not signed and written as Obs");
    assert_eq!((conn.served(Served::Sign), conn.served(Served::Head)), (1, 1), "an observation op went to the data register");
    assert_eq!(obs(&conn).0, 0, "records were left waiting after they rode");
    // The observation register is NOT followed: ten idle minutes after its commit read it no more (no backstop).
    let reads = conn.served(Served::ObsReadHead);
    for _ in 0..10 {
        next_minute(&mut conn);
    }
    assert_eq!(conn.served(Served::ObsReadHead), reads, "an idle page read the observation register");
}

/// **T2: continuous saving, one save every 5 s for 5 minutes, with a slow node every minute: at most one observation
/// commit per 60 s of saving.**
#[test]
fn saving_continuously_makes_at_most_one_observation_commit_per_w_ride() {
    let (mut s, mut conn, _node) = page(true);
    let mut starts = Vec::new();
    let mut last = 0;
    let t0 = conn.now_ms();
    let mut i = 0;
    while conn.now_ms() < t0 + 5 * MINUTE {
        if i % 12 == 0 {
            slow_read(&mut conn);
        }
        let now = conn.now_ms();
        conn.tick_at(now + 5_000);
        save(&mut s, i);
        let (_, commits, most) = obs(&conn);
        if commits > last {
            starts.push(conn.now_ms());
            last = commits;
        }
        assert!(most <= 1, "{most} observation ops were on the wire at once");
        i += 1;
    }
    assert!(starts.len() >= 3, "THE CONTROL: only {} observation commits in 5 minutes of saving with records waiting", starts.len());
    assert!(starts.len() <= 5, "{} observation commits in 5 minutes: more than one per W_ride", starts.len());
    for w in starts.windows(2) {
        assert!(w[1] - w[0] >= page::obs::W_RIDE_MS, "two observation commits {} ms apart", w[1] - w[0]);
    }
    // T5: the whole run kept ONE observation op on the wire at most, and did have one.
    assert_eq!(obs(&conn).2, 1, "the observation tree's ops were not one at a time (or none went out)");
}

/// **T3: a READS-ONLY page writes nothing to the observation tree**: ten minutes of reads (slow ones, so every
/// window has a record) after one save -- the records wait.
#[test]
fn a_reads_only_page_writes_no_observation() {
    let (mut s, mut conn, _node) = page(true);
    save(&mut s, 0);
    let before = (obs_ops(&conn), obs(&conn).1);
    for _ in 0..10 {
        slow_read(&mut conn);
        let _ = s.get(b"d/a/0000");
        next_minute(&mut conn);
    }
    assert_eq!((obs_ops(&conn), obs(&conn).1), before, "reading made an observation op");
    assert!(obs(&conn).0 >= 5, "THE CONTROL: the reads made no records to write");
}

/// **T4: the person's saves are the same with the recording riding or not**: the same script on a page with an
/// observation tree and one without gives the same write states, in the same order, and the same data-register ops.
/// (The simulated node answers at once, so this holds ORDER; the latency comparison is step 6's live measure.)
#[test]
fn the_data_writes_end_the_same_with_the_observation_tree_or_without() {
    let run = |site: bool| {
        let (mut s, mut conn, _node) = page(site);
        for i in 0..8 {
            slow_read(&mut conn);
            next_minute(&mut conn);
            save(&mut s, i);
        }
        let states: Vec<String> = conn
            .replies()
            .into_iter()
            .filter_map(|r| match r {
                // The session id is random per store: what is compared is each write's states, in order.
                protocol::Reply::WriteState { write_id, state } | protocol::Reply::SessionWriteState { write_id, state, .. } => Some(format!("{write_id}:{state:?}")),
                _ => None,
            })
            .collect();
        (states, conn.served(Served::Sign), conn.served(Served::Head), obs(&conn).1)
    };
    let (on, off) = (run(true), run(false));
    assert!(on.3 >= 2, "THE CONTROL: only {} observation commits rode -- nothing was compared", on.3);
    assert_eq!(off.3, 0);
    assert_eq!(on.0, off.0, "the data writes' states differ with the observation tree riding");
    assert_eq!((on.1, on.2), (off.1, off.2), "the data register's signs and UPDATEs differ with the observation tree riding");
}

/// **T7: the pending bound**: 70 minutes of records with no save keeps 60; the next record carries what was dropped,
/// as a lower bound -- never fewer than the dropped records' own events.
#[test]
fn records_past_the_bound_are_dropped_and_counted_into_the_next() {
    let (mut s, mut conn, node) = page(true);
    save(&mut s, 0);
    for _ in 0..70 {
        slow_read(&mut conn);
        next_minute(&mut conn);
    }
    assert_eq!(obs(&conn).0, page::obs::PENDING_MAX, "the waiting records are not held at the bound");
    // A save: the records ride; the newest carries the loss.
    let now = conn.now_ms();
    conn.tick_at(now + page::obs::W_RIDE_MS);
    save(&mut s, 1);
    let (_, root) = node.obs_head().expect("the records rode");
    let tree = node.tree(&root).expect("the observation tree is whole");
    let records: Vec<instrument::publish::Published> = tree.values().map(|v| instrument::publish::Published::decode(v, &[]).expect("a record")).collect();
    let oldest_kept = records.iter().map(|r| r.minute).min().expect("records");
    // The first record after the overflow began is the one that carries the dropped records' events.
    let carrier = records.iter().filter(|r| r.minute > oldest_kept).find(|r| r.dropped != instrument::vocab::Bucket::Zero);
    let carrier = carrier.expect("no record carries the dropped records' loss");
    // Each dropped record held at least one publishable event (a timed-out read): ten of them were dropped.
    assert!(carrier.dropped >= instrument::vocab::Bucket::of(10), "the loss carried is {:?}, under the ten dropped records' events", carrier.dropped);
}

/// **NO SITE, NO OBSERVATION TREE (the architect)**: a page not served from a site -- a dev build, a test -- records
/// locally and writes nothing, however much it saves.
#[test]
fn a_page_not_served_from_a_site_writes_no_observation() {
    let (mut s, mut conn, node) = page(false);
    for i in 0..5 {
        slow_read(&mut conn);
        next_minute(&mut conn);
        save(&mut s, i);
    }
    assert_eq!((obs_ops(&conn), obs(&conn)), (0, (0, 0, 0)), "a page with no site made observation ops or records");
    assert_eq!(node.obs_head(), None);
    assert!(conn.with_server(|sv| sv.page.recording().is_some()), "THE CONTROL: the page records nothing");
}

/// **AT THE BROWSER's CLOCK (epoch ms, the sdk#397 lesson)**: windows keyed by unix minutes still close and ride.
#[test]
fn at_the_browsers_epoch_clock_the_records_ride() {
    let (mut s, mut conn, node) = page(true);
    conn.tick_at(1_790_000_000_000);
    for _ in 0..3 {
        slow_read(&mut conn);
        next_minute(&mut conn);
    }
    assert!(obs(&conn).0 >= 2, "THE CONTROL: no records at the epoch clock");
    save(&mut s, 1);
    let (_, root) = node.obs_head().expect("the records did not ride at the epoch clock");
    let tree = node.tree(&root).expect("whole");
    for k in tree.keys() {
        let minute = u64::from_be_bytes(k[k.len() - 8..].try_into().expect("8 bytes"));
        assert!(minute >= 1_790_000_000_000 / MINUTE, "a record keyed at minute {minute}, not a unix minute of the page's clock");
    }
}
