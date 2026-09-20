//! THE TWO ROWS THE FIRST TABLE COULD NOT MEASURE.
//!
//! `what_tick_causes.rs` reports "owed parity emitted" as **not reached**,
//! because forty small records form no parity group: a zero whose
//! precondition never happened is not a measurement.
//!
//! Parity is owed per GROUP, and a group is a trio listed by a tree NODE — so
//! it needs a node with enough children (ARCHITECTURE §7: 7–12, mean ~9,
//! three parity blocks). This grows the tree until the engine actually owes
//! something, ASSERTS that it does, and only then measures what a `Tick` and
//! a `Flush` cause.

use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone, Default)]
struct Store(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Store {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

struct Node {
    store: Store,
    ctx: Vec<u8>,
    puts: usize,
}

impl Node {
    fn new() -> Node {
        Node {
            store: Store::default(),
            ctx: Vec::new(),
            puts: 0,
        }
    }

    /// How many groups the engine owes right now, read from a shell rebuilt
    /// on this context — which is how anything reads it.
    fn owed(&self) -> usize {
        let shell: Shell<Store> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.store.clone(),
            StoreFacts::provisioned(),
        );
        shell.engine.owed_groups()
    }

    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let mut shell: Shell<Store> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.store.clone(),
            StoreFacts::provisioned(),
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.puts += 1;
                    self.store.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = self.store.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    next.push(Inbound::GotHead { seq, root })
                }
                engine_delegate::schedule::Op::ReadHead { .. } => next.push(Inbound::NoHead),
            }
        }
        if !next.is_empty() {
            self.step(next);
        }
        out.replies
    }

    fn client(&mut self, r: &protocol::Request) {
        let frame = protocol::encode_request(protocol::CURRENT, r);
        self.step(vec![Inbound::Client(frame)]);
    }
}

/// Records big enough that a leaf fills and the tree grows a level — which is
/// what puts trios on a node. GENERATED, not hand-written: the point is to
/// reach a structural condition, not to pin a particular tree.
fn write(n: u64) -> protocol::Request {
    protocol::Request::Write {
        write_id: n,
        ops: vec![protocol::Op::Put(
            format!("k/{n:06}").into_bytes(),
            vec![(n % 251) as u8; 512],
        )],
    }
}

/// Grow the tree until the engine owes at least one group, or give up.
///
/// Returns how many records it took, so the cost of reaching the condition
/// is reported rather than hidden.
fn grow_until_owed(node: &mut Node, cap: u64) -> Option<u64> {
    node.client(&protocol::Request::Identity);
    for i in 1..=cap {
        node.client(&write(i));
        if node.owed() > 0 {
            return Some(i);
        }
    }
    None
}

/// **THE MEASUREMENT.** What a Tick and a Flush cause once something IS owed.
///
/// Three numbers this printed before the fixes in this branch, all of them
/// wrong in a way no existing test could see:
///
/// | | no time | with tick | with flush |
/// |-|-|-|-|
/// | before | 0 | **0** | **0** |
/// | after  | 0 | 3 | 3 |
///
/// Owed parity was put `after` the published root, and the scheduler's idea
/// of what the node holds is built fresh every call (F32) — so the put was
/// held on a dependency confirmed in an earlier call, and dropped when the
/// call ended. Every call. The redundancy the tree promises was never
/// written at all.
#[test]
fn the_parity_rows() {
    let mut node = Node::new();
    let Some(records) = grow_until_owed(&mut node, 400) else {
        panic!(
            "400 records of 512 B owed no parity group at all. The rows this \
             file exists to measure still cannot be measured, and reporting a \
             zero for them would say 'a Tick causes nothing' when the truth is \
             'nothing was owed'."
        );
    };
    let owed = node.owed();
    println!("\n  reached the condition: {records} records, {owed} group(s) owed");

    // With NO time and NO flush: how many puts does simply asking again cost?
    let mut quiet = Node::new();
    grow_until_owed(&mut quiet, 400);
    let before = quiet.puts;
    quiet.client(&protocol::Request::AskWrite { write_id: 1 });
    let no_time = quiet.puts - before;

    // With a TICK past `parity_age`.
    let mut ticked = Node::new();
    grow_until_owed(&mut ticked, 400);
    let before = ticked.puts;
    ticked.client(&protocol::Request::Tick { now: 10_000 });
    let with_tick = ticked.puts - before;

    // With a FLUSH: every group, whatever its age.
    let mut flushed = Node::new();
    grow_until_owed(&mut flushed, 400);
    let before = flushed.puts;
    flushed.client(&protocol::Request::Flush);
    let with_flush = flushed.puts - before;

    println!("  behaviour            | no time | with tick | with flush");
    println!("  ---------------------|---------|-----------|-----------");
    println!("  puts caused          | {no_time:<7} | {with_tick:<9} | {with_flush}");

    assert_eq!(
        no_time, 0,
        "owed parity went out with no time sent at all, so it does not need \
         a Tick and the table is wrong about it"
    );
    assert!(
        with_tick > 0,
        "a Tick well past parity_age caused no put, though {owed} group(s) \
         were owed: the redundancy would never be written"
    );
    assert!(
        with_flush > 0,
        "a Flush caused no put though {owed} group(s) were owed: closing a tab \
         would leave the redundancy unwritten"
    );
}

/// **AND IT STOPS.** A group that has been put is not put again.
///
/// The other half of the same row, and the one a "did anything happen"
/// assertion cannot see. Putting the parity is only right if the group then
/// SETTLES: an engine that emits three blocks on every tick for ever has the
/// redundancy, and pays for it again every second a tab is open.
///
/// Before this branch: 3 puts per tick, for ever, and `owed` never reached 0.
/// The ack arrives in a LATER call than the put, and the map from a parity
/// block to its group did not survive the call (F32), so nothing could
/// attribute the confirmation to the group it settled.
#[test]
fn a_settled_group_is_not_put_again() {
    let mut node = Node::new();
    let records = grow_until_owed(&mut node, 400).expect("a group forms");

    let before = node.puts;
    node.client(&protocol::Request::Tick { now: 10_000 });
    let first = node.puts - before;
    let owed_after_first = node.owed();

    node.client(&protocol::Request::Tick { now: 20_000 });
    let second = node.puts - before - first;
    node.client(&protocol::Request::Tick { now: 30_000 });
    let third = node.puts - before - first - second;

    println!(
        "\n  {records} records; puts per tick: {first}, {second}, {third} \
         (owed after the first: {owed_after_first})"
    );

    assert!(
        first > 0,
        "the first tick put nothing: see `the_parity_rows`"
    );
    assert_eq!(
        owed_after_first, 0,
        "the group is still owed after its parity was put AND acknowledged. \
         Nothing attributed the acks to the group, so it can never settle."
    );
    assert_eq!(
        (second, third),
        (0, 0),
        "a settled group was put again on the next tick ({second}) and the \
         one after ({third}). An open tab would re-put the same three blocks \
         for as long as it stayed open."
    );
}

/// The control: with nothing owed, the same ticks cause nothing.
///
/// Without this, every assertion above would also pass on an engine that put
/// three blocks on every tick regardless — `first > 0` would be satisfied by
/// noise, and the zeroes by a tick that does nothing at all. This pins the
/// cause to the OWED GROUP rather than to the tick.
#[test]
fn nothing_owed_puts_nothing() {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    // One small record: well short of the ~11 that fill a leaf and list a
    // trio, which `the_parity_rows` measures.
    node.client(&write(1));
    assert_eq!(node.owed(), 0, "the control needs an engine owing nothing");

    let before = node.puts;
    node.client(&protocol::Request::Tick { now: 10_000 });
    node.client(&protocol::Request::Flush);
    assert_eq!(
        node.puts - before,
        0,
        "a Tick and a Flush caused puts with no group owed, so the puts the \
         other tests count are not evidence of parity"
    );
}
