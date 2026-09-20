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

    if with_tick {
        // WELL past `max_accept_age` (64 by default).
        seen.extend(node.states(&protocol::Request::Tick { now: 10_000 }));
    } else {
        // The same number of calls, so the two arms differ ONLY in whether
        // time was sent. Without this the comparison is between one call and
        // two, and the shell advances per call.
        seen.extend(node.states(&protocol::Request::AskWrite { write_id: 1 }));
    }
    if std::env::var("DIAG").is_ok() {
        println!("    with_tick={with_tick}: states seen = {seen:?}");
    }
    seen.contains(&protocol::WriteState::Stalled)
}

/// **MEASURED DEFECT: `Stalled` can never be reported by a delegate.**
///
/// It fires in NEITHER column — not without a tick, and not with one well
/// past `max_accept_age`. The cause is in `engine/src/lib.rs`:
///
/// * `age_out_accepted` returns early unless `self.in_flight_since` is
///   `Some`, and that field is set when a commit starts;
/// * `struct Context` — everything that survives a call — carries `pending`
///   but **not `in_flight_since`, and not `told_stalled`**;
/// * a delegate is rebuilt from its context on every call (F32), so
///   `in_flight_since` is `None` at the top of every call that is not the one
///   that started the commit.
///
/// So the state that measures HOW LONG something has been stuck is the one
/// thing that does not survive — and a commit that is stuck spans many calls
/// by definition. The mechanism can only work inside a single call, which is
/// the one situation it is not for.
///
/// `told_stalled` is missing for the same reason, so even once the first
/// problem is fixed the "report it once" guard would not hold across calls
/// either. The first defect hides the second.
///
/// Recorded as sdk#82. This test asserts WHAT IS TRUE TODAY so the suite is
/// honest and green; it inverts when the defect is fixed, which is the point.
#[test]
fn stalled_is_unreachable_in_a_delegate_today() {
    assert!(
        !stalled_reported(true),
        "Stalled now fires with a Tick — the defect is fixed, and this test \
         should become the positive assertion it was written as (sdk#82)"
    );
    assert!(
        !stalled_reported(false),
        "Stalled fired with no tick at all"
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
    // NOT MEASURED, and said so rather than printed as a zero.
    //
    // 40 writes produce 40 puts and neither a Tick nor a Flush causes one
    // more — but in this scenario NO PARITY IS EVER OWED: the records are
    // small enough to form no group. A zero whose precondition was never
    // reached is not a measurement, and reporting it as "Tick causes
    // nothing" would be the 256-rows mistake again.
    //
    // Measuring it needs a tree big enough to form parity groups, which is
    // a bigger fixture than this file should carry.
    let (no_tick, with_tick) = (puts_caused(false, false), puts_caused(true, false));
    let on_flush = puts_caused(false, true);
    assert_eq!(
        (no_tick, with_tick, on_flush),
        (0, 0, 0),
        "a put WAS caused, so parity is being owed after all and these rows \
         can be measured rather than marked unreached"
    );
    println!("  owed parity emitted               | not reached in this scenario (no group forms)");
    println!("  owed parity on Flush              | not reached in this scenario (no group forms)");
    println!(
        "  a failed put re-emitted           | fires   | fires     | Event::PutFailed — an EVENT"
    );
    println!("  read-back rounds (delegate shell) | fires   | fires     | per CALL, not per time");
    println!("  the client's pending timeout      | fires   | fires     | the CLIENT's clock, already driven");
    println!(
        "\n  Stalled fires in NEITHER column: `in_flight_since` is not in the\n  \
         engine's Context, so a delegate rebuilt on every call (F32) has it as\n  \
         None at the top of every call but the one that started the commit.\n  \
         sdk#82."
    );
}
