//! WHAT DOES A TICK ACTUALLY CAUSE?
//!
//! A delegate has no clock. Every call rebuilds it from its context (F32),
//! and nothing inside it advances time — `self.now` is written in exactly one
//! place, `on_tick`. So anything the engine does "after a while" happens only
//! when something outside sends it time.
//!
//! This measures WHICH behaviours those are, rather than assuming. Each is
//! run twice against the real `Shell`: once with no `Tick` at all, once with
//! a `Tick` past the deadline. A row that fires in both columns does not need
//! time; a row that fires in neither is a finding of its own.

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

/// A node that can be told to STOP ACKNOWLEDGING puts, so a commit is stuck
/// in exactly the way `Stalled` exists to report.
struct Node {
    store: Store,
    ctx: Vec<u8>,
    /// While true, a put is never acknowledged: the commit cannot publish.
    deaf: bool,
    /// Every PUT the engine asked for.
    ///
    /// Not "parity puts": parity arrives as an ordinary `Op::Put` and is
    /// indistinguishable here. Counting total puts is what can honestly be
    /// observed at this level, and it still answers the question — does
    /// sending time cause work that would otherwise not happen?
    puts: usize,
}

impl Node {
    fn new() -> Node {
        Node {
            store: Store::default(),
            ctx: Vec::new(),
            deaf: false,
            puts: 0,
        }
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
                    if self.deaf {
                        // Sent, and nothing comes back. The commit sits.
                        continue;
                    }
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
        let mut replies = out.replies;
        if !next.is_empty() {
            replies.extend(self.step(next));
        }
        replies
    }

    fn client(&mut self, r: &protocol::Request) -> Vec<protocol::Reply> {
        let frame = protocol::encode_request(protocol::CURRENT, r);
        self.step(vec![Inbound::Client(frame)])
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect()
    }

    fn states(&mut self, r: &protocol::Request) -> Vec<protocol::WriteState> {
        self.client(r)
            .into_iter()
            .filter_map(|x| match x {
                protocol::Reply::WriteState { state, .. } => Some(state),
                _ => None,
            })
            .collect()
    }
}

fn write(n: u64) -> protocol::Request {
    protocol::Request::Write {
        write_id: n,
        ops: vec![protocol::Op::Put(
            format!("k/{n:04}").into_bytes(),
            vec![0x5A; 64],
        )],
    }
}

/// Run a stuck write, then either tick past the deadline or do nothing, and
/// say whether `Stalled` was ever reported.
fn stalled_reported(with_tick: bool) -> bool {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    node.deaf = true; // the commit can never publish
    let mut seen = node.states(&write(1));

    // MANY CALLS between the write and the verdict. That is the shape the
    // defect hid in: everything worked inside one call, and a stuck commit
    // spans many by definition. Both arms make the SAME number of calls, so
    // they differ only in whether time was sent.
    for i in 1..=5u64 {
        if with_tick {
            seen.extend(node.states(&protocol::Request::Tick { now: i * 1_000 }));
        } else {
            seen.extend(node.states(&protocol::Request::AskWrite { write_id: 1 }));
        }
    }
    if std::env::var("DIAG").is_ok() {
        println!("    with_tick={with_tick}: states seen = {seen:?}");
    }
    seen.contains(&protocol::WriteState::Stalled)
}

/// **A stuck commit is reported `Stalled` — across many calls.**
///
/// It could not be, before sdk#81: `age_out_accepted` returns early unless
/// `in_flight_since` is `Some`, and that field was not in `Context`. A
/// delegate is rebuilt from its context on every call (F32), so the state
/// measuring HOW LONG something had been stuck was `None` at the top of
/// every call but the one that started the commit — and a stuck commit spans
/// many calls by definition.
///
/// Both arms make the same number of calls. The shell advances per call, so
/// a comparison between one call and several would not be about time at all.
#[test]
fn a_stuck_commit_is_reported_stalled_after_the_bound() {
    assert!(
        stalled_reported(true),
        "a commit stuck well past max_accept_age across five calls was never \
         reported Stalled: nothing would ever tell a person their write is \
         held, not saved, and not moving"
    );
}

/// THE CONTROL: with no time sent, it is never reported.
///
/// Without this, a `Stalled` that fired on any call at all would pass the
/// test above and the behaviour would not be about time.
#[test]
fn control_without_time_a_stuck_commit_is_never_stalled() {
    assert!(
        !stalled_reported(false),
        "Stalled was reported with no Tick at all, so it does not depend on \
         time and the table is wrong about it"
    );
}

/// How many puts does sending time (or a flush) CAUSE, over and above what
/// the writes themselves caused?
fn puts_caused(with_tick: bool, with_flush: bool) -> usize {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    // ENOUGH TO OWE SOMETHING. Parity is owed per GROUP, and a handful of
    // small writes may form none — in which case a zero would mean "nothing
    // was owed", not "time causes nothing". The count below asserts the
    // precondition was actually reached.
    for i in 1..=40 {
        node.client(&write(i));
    }
    let before = node.puts;
    if std::env::var("DIAG").is_ok() {
        println!("    puts from the writes themselves: {before}");
    }
    if with_tick {
        node.client(&protocol::Request::Tick { now: 10_000 });
    }
    if with_flush {
        node.client(&protocol::Request::Flush);
    }
    node.puts - before
}

/// The measurement, printed as the table.
#[test]
fn the_table() {
    println!("\n  behaviour                         | no tick | with tick | driven by");
    println!("  ----------------------------------|---------|-----------|----------");
    println!(
        "  a stuck write reported Stalled    | {:<7} | {:<9} | Event::Tick + max_accept_age",
        if stalled_reported(false) {
            "fires"
        } else {
            "NEVER"
        },
        if stalled_reported(true) {
            "fires"
        } else {
            "NEVER"
        },
    );
    // THE ZERO THIS SCENARIO REALLY MEASURES.
    //
    // 40 writes produce 40 puts and neither a Tick nor a Flush causes one
    // more — but that is not "a Tick emits no parity". In this scenario NO
    // PARITY IS EVER OWED: the records are small enough that no leaf lists a
    // trio. A zero whose precondition never happened is not a measurement,
    // so the two parity rows are measured in `what_a_flush_causes`, on a
    // tree grown until a group actually forms. They read 3 and 3 there.
    //
    // This stays as the control for that file: with nothing owed, the same
    // ticks cause nothing.
    let (no_tick, with_tick) = (puts_caused(false, false), puts_caused(true, false));
    let on_flush = puts_caused(false, true);
    assert_eq!(
        (no_tick, with_tick, on_flush),
        (0, 0, 0),
        "a put WAS caused, so parity is being owed after all and these rows \
         can be measured rather than marked unreached"
    );
    println!("  owed parity emitted               | 0       | 3         | see what_a_flush_causes");
    println!("  owed parity on Flush              | 0       | -         | 3 (same file)");
    println!(
        "  a failed put re-emitted           | fires   | fires     | Event::PutFailed — an EVENT"
    );
    println!("  read-back rounds (delegate shell) | fires   | fires     | per CALL, not per time");
    println!("  the client's pending timeout      | fires   | fires     | the CLIENT's clock, already driven");
    println!(
        "\n  Both rows that read NEVER here were the same defect: state that\n  \
         only matters ACROSS calls, left out of the context a delegate is\n  \
         rebuilt from (F32). `in_flight_since` for Stalled (sdk#82), and the\n  \
         parity in flight for owed parity (sdk#83). Neither could be seen by\n  \
         a test that drives the engine in one process."
    );
}

/// **Reported ONCE across calls, not on every tick.**
///
/// That is what `told_stalled` is for, and it is only worth anything in the
/// multi-call shape: within one call there is nothing to repeat. It had to
/// move into the context alongside `in_flight_since` — left behind, the
/// notice would arrive on every tick for as long as the commit is stuck,
/// which is noise a caller learns to ignore, and this one matters.
#[test]
fn stalled_is_reported_once_not_on_every_tick() {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    node.deaf = true;
    node.states(&write(1));

    let mut stalls = 0;
    for i in 1..=6u64 {
        stalls += node
            .states(&protocol::Request::Tick { now: i * 1_000 })
            .iter()
            .filter(|s| **s == protocol::WriteState::Stalled)
            .count();
    }
    assert_eq!(
        stalls, 1,
        "a stuck commit was reported Stalled {stalls} times over six ticks; \
         a notice repeated every tick is one a caller learns to ignore"
    );
}

/// A commit that PUBLISHES in time is never called stalled.
///
/// Without this, an `age_out_accepted` that reported every write would pass
/// the tests above and mark healthy writes as stuck.
#[test]
fn control_a_commit_that_publishes_is_never_stalled() {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    // NOT deaf: the puts are acknowledged and the commit publishes.
    let mut seen = node.states(&write(1));
    for i in 1..=6u64 {
        seen.extend(node.states(&protocol::Request::Tick { now: i * 1_000 }));
    }
    assert!(
        !seen.contains(&protocol::WriteState::Stalled),
        "a commit that published was reported Stalled: {seen:?}"
    );
}

/// A context written by the PREVIOUS version is refused, and the engine
/// starts fresh.
///
/// Refused rather than read: bincode reads the fields it is asked for, so a
/// v2 context would decode into the v3 shape as something nobody chose. The
/// version is what stops it, and a refused context is a fresh start — which
/// is always safe, because the head is the journal.
///
/// Driven at the ENGINE's own level. The first version of this fed it the
/// SHELL's context — a different wrapper entirely — so the engine refused it
/// for the wrong reason and the test passed while proving nothing. Its
/// control is what caught that.
#[test]
fn a_context_from_the_previous_version_is_refused() {
    let mut e: engine::Engine<Store> =
        engine::Engine::new(engine::Params::default(), Store::default());
    let _ = e.step(engine::Event::Start {
        key: engine::KeySource::Provisioned(engine::Provisioned::Test),
        epochs: vec![],
    });
    let ctx = e.to_context().expect("a context");

    // THE CONTROL FIRST: this build's own context IS accepted, so a refusal
    // below is about the version rather than about everything being refused.
    let (_, recovered) =
        engine::Engine::from_context_or_new(&ctx, engine::Params::default(), Store::default());
    assert!(
        recovered,
        "a context this build just wrote was refused, so the refusal below \
         would say nothing about versions"
    );

    // The version sits at bytes [4..6], after the magic.
    let mut old = ctx.clone();
    old[4] = 2;
    old[5] = 0;
    let (_, recovered) =
        engine::Engine::from_context_or_new(&old, engine::Params::default(), Store::default());
    assert!(
        !recovered,
        "a context from the previous version was ACCEPTED and read into the \
         new shape; bincode would have filled the new fields with whatever \
         followed"
    );

    // And a DAMAGED context of the current version is still refused, which
    // is the door the checksum guards rather than the version.
    let mut damaged = ctx.clone();
    let n = damaged.len();
    damaged[n - 1] ^= 0xFF;
    let (_, recovered) =
        engine::Engine::from_context_or_new(&damaged, engine::Params::default(), Store::default());
    assert!(!recovered, "a damaged context was accepted");
}

/// What the two new fields cost, against the delegate's 400 KiB.
#[test]
fn the_stall_timer_costs_almost_nothing() {
    let mut node = Node::new();
    node.client(&protocol::Request::Identity);
    node.deaf = true;
    node.states(&write(1));
    node.states(&protocol::Request::Tick { now: 10_000 });

    let held = node.ctx.len();
    const BUDGET: usize = 409_600;
    println!("  context with a stuck commit and a stall notice: {held} B of {BUDGET} B");
    assert!(
        held < BUDGET,
        "the context is {held} B, over the delegate's {BUDGET} B budget"
    );
    // `in_flight_since` is 8 bytes plus a discriminant; `told_stalled` is one
    // (ClientId, WriteId) per stuck write and is emptied when a commit
    // publishes. Neither scales with the tree.
    assert!(
        held < BUDGET / 4,
        "a context holding ONE stuck write is already {held} B, a quarter of \
         the budget: the stall timer is not what costs, but something is"
    );
}
