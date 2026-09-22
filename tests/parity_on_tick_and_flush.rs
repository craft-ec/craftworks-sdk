//! THE PARITY ROWS, ON THE PAGE PATH (re-stated from the Shell's
//! `what_a_flush_causes.rs`, deleted with the Shell).
//!
//! Parity is owed per GROUP, and a group is a trio listed by a tree NODE — so
//! it needs a node with enough children (ARCHITECTURE §7: 7–12, mean ~9,
//! three parity blocks). This grows the tree until the engine actually owes
//! something, ASSERTS that it does, and only then measures what a `Tick` and
//! a `Flush` cause — through a real tab (`page::Server` over the page path's
//! node), counting the block PUTs the node served.

use testkit::page_node::Served;
use testkit::{PageConn, PageNode};

/// Records big enough that a leaf fills and the tree grows a level — which is
/// what puts trios on a node. GENERATED: the point is to reach a structural
/// condition, not to pin a particular tree.
fn write(n: u64) -> protocol::Request {
    protocol::Request::forced_write(n, vec![protocol::Op::Put(format!("k/{n:06}").into_bytes(), vec![(n % 251) as u8; 512])])
}

fn owed(c: &PageConn) -> usize {
    c.with_server(|s| s.page.owed_groups())
}

fn puts(c: &PageConn) -> usize {
    c.served(Served::Put)
}

/// A tab, and the tree grown until its engine owes at least one group (or
/// `None`). Returns how many records it took, so the cost of reaching the
/// condition is reported rather than hidden.
fn grow_until_owed(cap: u64) -> (PageConn, Option<u64>) {
    let node = PageNode::new();
    let mut c = node.connect();
    c.client(&protocol::Request::Identity);
    for i in 1..=cap {
        c.client(&write(i));
        if owed(&c) > 0 {
            return (c, Some(i));
        }
    }
    (c, None)
}

/// **THE MEASUREMENT.** What a Tick and a Flush cause once something IS owed:
/// nothing with no time sent, a PUT with a tick past `parity_age`, a PUT with
/// a Flush whatever the age.
#[test]
fn the_parity_rows() {
    let (c, records) = grow_until_owed(400);
    let Some(records) = records else {
        panic!(
            "400 records of 512 B owed no parity group at all. The rows this \
             file exists to measure still cannot be measured, and reporting a \
             zero for them would say 'a Tick causes nothing' when the truth is \
             'nothing was owed'."
        );
    };
    println!("\n  reached the condition: {records} records, {} group(s) owed", owed(&c));

    // With NO time and NO flush: asking about a write again costs no put.
    let (mut quiet, _) = grow_until_owed(400);
    let before = puts(&quiet);
    quiet.client(&protocol::Request::AskWrite { write_id: 1 });
    let no_time = puts(&quiet) - before;

    // With a TICK well past `parity_age`: the page's clock AND the SDK's Tick.
    let (mut ticked, _) = grow_until_owed(400);
    let before = puts(&ticked);
    let t = ticked.now_ms() + 10_000_000;
    ticked.tick_at(t);
    let with_tick = puts(&ticked) - before;

    // With a FLUSH: every group, whatever its age.
    let (mut flushed, _) = grow_until_owed(400);
    let before = puts(&flushed);
    flushed.client(&protocol::Request::Flush);
    let with_flush = puts(&flushed) - before;

    println!("  behaviour            | no time | with tick | with flush");
    println!("  ---------------------|---------|-----------|-----------");
    println!("  puts caused          | {no_time:<7} | {with_tick:<9} | {with_flush}");

    assert_eq!(no_time, 0, "owed parity went out with no time sent at all, so it does not need a Tick and the table is wrong about it");
    assert!(with_tick > 0, "a Tick well past parity_age caused no put, though a group was owed: the redundancy would never be written");
    assert!(with_flush > 0, "a Flush caused no put though a group was owed: closing a tab would leave the redundancy unwritten");
}

/// **AND IT STOPS.** A group that has been put is not put again: after its
/// parity is put and acknowledged it is no longer owed, and the next ticks put
/// nothing.
#[test]
fn a_settled_group_is_not_put_again() {
    let (mut c, records) = grow_until_owed(400);
    let records = records.expect("a group forms");
    let t0 = c.now_ms() + 10_000_000;
    let before = puts(&c);
    c.tick_at(t0);
    let first = puts(&c) - before;
    let owed_after_first = owed(&c);
    c.tick_at(t0 + 10_000_000);
    let second = puts(&c) - before - first;
    c.tick_at(t0 + 20_000_000);
    let third = puts(&c) - before - first - second;
    println!("\n  {records} records; puts per tick: {first}, {second}, {third} (owed after the first: {owed_after_first})");
    assert!(first > 0, "the first tick put nothing: see `the_parity_rows`");
    assert_eq!(owed_after_first, 0, "the group is still owed after its parity was put AND acknowledged: it can never settle");
    assert_eq!((second, third), (0, 0), "a settled group was put again on the next tick ({second}) and the one after ({third})");
}

/// The control: with nothing owed, the same tick and flush cause nothing. This
/// pins the cause to the OWED GROUP rather than to the tick.
#[test]
fn nothing_owed_puts_nothing() {
    let node = PageNode::new();
    let mut c = node.connect();
    c.client(&protocol::Request::Identity);
    c.client(&write(1));
    assert_eq!(owed(&c), 0, "the control needs an engine owing nothing");
    let before = puts(&c);
    let t = c.now_ms() + 10_000_000;
    c.tick_at(t);
    c.client(&protocol::Request::Flush);
    assert_eq!(puts(&c) - before, 0, "a Tick and a Flush caused puts with no group owed, so the puts the other tests count are not evidence of parity");
}
