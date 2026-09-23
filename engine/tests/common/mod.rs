//! The node's block store, as a test sees it.
//!
//! `Blocks::get` hands out `Option<&[u8]>` tied to `&self`, so a store that
//! can be written to WHILE an engine holds it cannot hand out borrows of
//! anything it might later move. This one keeps each block's bytes at a fixed
//! address for the lifetime of the process and hands out `&'static [u8]`.
//!
//! That is a leak, and it is deliberate and test-only: a test process writes a
//! bounded number of blocks and then exits. The alternative is threading a
//! lifetime through every engine and harness, which would shape the production
//! type around a convenience of the tests.

#![allow(dead_code)]

use engine::{ClientId, Effect, Engine, Event, Op, Params, State, WriteId};
use freenet_prolly::build::TreeBuilder;
use freenet_prolly::store::Blocks;
use freenet_prolly::store::MemBlocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

thread_local! {
    /// One store per test thread.
    ///
    /// `Store::default()` hands back a handle to it, so a test and the engine
    /// it built share the node's blocks without the test threading a value
    /// through every helper. Thread-local, so tests running in parallel do
    /// not see each other's blocks — the isolation a global would lose.
    ///
    /// This is test scaffolding. The engine itself has no global state, which
    /// `no_global_state_in_the_engine` asserts.
    static THREAD_STORE: Inner = Inner::default();
}

#[derive(Clone, Default)]
struct Inner {
    blocks: Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>,
    forgotten: Rc<RefCell<std::collections::BTreeSet<Cid>>>,
    reads: Rc<std::cell::Cell<usize>>,
}

#[derive(Clone)]
pub struct Store {
    inner: Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>,
    /// Blocks this store pretends not to have, for the forgetful case.
    forgotten: Rc<RefCell<std::collections::BTreeSet<Cid>>>,
    /// Every `Blocks::get`, counted. A cost claim about a walk that is
    /// supposed to read NOTHING can only be checked by something that would
    /// see it if it read one block.
    reads: Rc<std::cell::Cell<usize>>,
}

impl Default for Store {
    fn default() -> Self {
        THREAD_STORE.with(|i| Store {
            inner: i.blocks.clone(),
            forgotten: i.forgotten.clone(),
            reads: i.reads.clone(),
        })
    }
}

/// Step the engine and let the node absorb what it emitted.
///
/// The engine keeps no blocks, so a test that only calls `step` leaves the
/// tree nowhere: the next call reads through `Blocks` and finds nothing. On a
/// real node the puts land and the node holds them; here the store does it.
/// Every converted test goes through this rather than `step` directly.
/// A macro rather than a function so it works whether the engine is an owned
/// binding or a `&mut` parameter — the call sites are both, and a function
/// would need each one to spell its own borrow.
#[macro_export]
macro_rules! stepped {
    ($e:expr, $ev:expr) => {{
        let out = $e.step($ev);
        // Into the engine's OWN store: a test that builds an engine over an
        // isolated store would otherwise absorb into the thread's, and every
        // block it emitted would be unreadable to the engine that emitted it.
        $e.blocks().absorb(&out);
        out
    }};
}

/// `Engine::new` with the thread's store, so a converted test reads as it did.
///
/// The engine is told the tree is NEW (`HeadMissing`), because a write made
/// before the head is recovered waits for it (sdk#223) and these tests write
/// straight away. A test about recovery itself sends `Start` and its own
/// answer, which resets this.
pub fn new_store_params(params: Params) -> Engine<Store> {
    let mut e = Engine::new(params, Store::default());
    let _ = e.step(Event::HeadMissing);
    e
}

/// A COLD READER of `root`: an engine that has adopted it and holds none of
/// its blocks, on a store of its own (returned, for what the page keeps).
pub fn cold_reader(root: Cid, params: Params) -> (Engine<Store>, Store) {
    let store = Store::fresh();
    let mut e = Engine::new(params, store.clone());
    e.adopt_root_for_test(root);
    (e, store)
}

impl Store {
    /// A store of its very own, for a test that wants two.
    pub fn fresh() -> Self {
        Store {
            inner: Rc::new(RefCell::new(BTreeMap::new())),
            forgotten: Rc::new(RefCell::new(std::collections::BTreeSet::new())),
            reads: Rc::new(std::cell::Cell::new(0)),
        }
    }

    /// Blocks handed out since this store was made.
    pub fn reads(&self) -> usize {
        self.reads.get()
    }
}

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        // Counted BEFORE the forgotten check: an ASK is a read whether or not
        // the store chooses to answer it, and counting only the hits would
        // make a walk over blocks nobody holds look free.
        self.reads.set(self.reads.get() + 1);
        if self.forgotten.borrow().contains(cid) {
            return None;
        }
        self.inner.borrow().get(cid).copied()
    }
}

impl Store {
    pub fn put(&self, id: Cid, bytes: &[u8]) {
        // Re-putting the same block is normal on a network, and the leak is
        // per call, so dedupe: a fuzz loop puts the same id thousands of times.
        if self.inner.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.inner.borrow_mut().insert(id, leaked);
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Absorb what a commit emitted, the way a node does once its puts land.
    ///
    /// A pack is unpacked: what the node ends up holding is the blocks inside
    /// it, under their own ids, which is the whole point of the format.
    pub fn absorb(&self, effects: &[Effect]) {
        for f in effects {
            match f {
                Effect::PutPack { id, bytes, .. } => {
                    for (mid, mbytes) in engine::pack::members(bytes) {
                        self.put(mid, &mbytes);
                    }
                    self.put(*id, bytes);
                }
                // A queued write's warm-apply block (R-b): the page keeps it.
                Effect::PutBlock { id, bytes, .. } | Effect::PutParity { id, bytes, .. } | Effect::Keep { id, bytes } => {
                    self.put(*id, bytes);
                }
                _ => {}
            }
        }
    }

    /// Forget a block: the node evicted it.
    ///
    /// F33: a sync read does NOT refresh hosting, so a block a descent read on
    /// one call can be gone on the next, and nothing the engine does prevents
    /// it. A store that cannot express that cannot test for it.
    pub fn forget(&self, cid: Cid) {
        self.forgotten.borrow_mut().insert(cid);
    }

    pub fn remember(&self, cid: &Cid) {
        self.forgotten.borrow_mut().remove(cid);
    }
}

/// How the harness drives the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One engine value for the whole run — how slices 1-3 were written.
    Live,
    /// The engine is DROPPED and rebuilt from its context between every step,
    /// which is what a delegate actually does. Both modes must agree.
    Rehydrate,
}

/// An engine driven in one of the two modes, so a test can be written once.
pub struct Harness {
    mode: Mode,
    params: Params,
    pub store: Store,
    live: Option<Engine<Store>>,
    ctx: Vec<u8>,
    /// Contexts written, and the largest seen — the budget's own evidence.
    pub max_context: usize,
    /// What the engine shed, summed over every step (sdk#162).
    pub shed: engine::Shed,
}

impl Harness {
    pub fn new(mode: Mode, params: Params, store: Store) -> Self {
        let mut e = Engine::new(params, store.clone());
        // A NEW tree, and the engine is told so: a write before recovery
        // waits for it (sdk#223). A test that reads a head (`Start` then
        // `HeadRead`) resets and redoes this, as `Start` does.
        let _ = e.step(Event::HeadMissing);
        let ctx = e.to_context().expect("a fresh engine has a context");
        Harness {
            mode,
            params,
            store,
            live: (mode == Mode::Live).then_some(e),
            ctx,
            max_context: 0,
            shed: engine::Shed::default(),
        }
    }

    pub fn step(&mut self, ev: Event) -> Vec<Effect> {
        let out = match self.mode {
            Mode::Live => {
                let e = self.live.as_mut().expect("a live engine");
                let out = e.step(ev);
                let s = e.take_shed();
                add(&mut self.shed, s);
                out
            }
            Mode::Rehydrate => {
                let mut e = Engine::from_context(&self.ctx, self.params, self.store.clone())
                    .expect("the engine's own context must read back");
                let out = e.step(ev);
                add(&mut self.shed, e.take_shed());
                // `context_len` sizes the live state field by field; it must be
                // exactly what `to_context` writes (sdk#162), in every state any
                // engine test reaches.
                if let Ok(c) = e.to_context() {
                    assert_eq!(
                        e.context_len(),
                        c.len(),
                        "context_len drifted from to_context"
                    );
                }
                self.ctx = e.to_context().expect("a context after every step");
                self.max_context = self.max_context.max(self.ctx.len());
                out
            }
        };
        // The node holds what the commit put, whichever mode this is.
        self.store.absorb(&out);
        out
    }

    /// Owed parity groups, as the engine stands (rebuilt from its context in
    /// Rehydrate mode, as a delegate would be).
    pub fn owed_groups(&self) -> usize {
        match self.mode {
            Mode::Live => self.live.as_ref().expect("a live engine").owed_groups(),
            Mode::Rehydrate => Engine::from_context(&self.ctx, self.params, self.store.clone())
                .expect("its own context")
                .owed_groups(),
        }
    }

    /// The context's size as last written (Rehydrate mode).
    pub fn context_len(&self) -> usize {
        self.ctx.len()
    }

    /// The root the engine would report. In rehydrate mode that means
    /// rebuilding it, which is the point: nothing is remembered outside the
    /// context.
    pub fn root(&self) -> Cid {
        match self.mode {
            Mode::Live => self.live.as_ref().expect("a live engine").root(),
            Mode::Rehydrate => Engine::from_context(&self.ctx, self.params, self.store.clone())
                .expect("its own context")
                .root(),
        }
    }

    pub fn published_root(&self) -> Cid {
        match self.mode {
            Mode::Live => self.live.as_ref().expect("a live engine").published_root(),
            Mode::Rehydrate => Engine::from_context(&self.ctx, self.params, self.store.clone())
                .expect("its own context")
                .published_root(),
        }
    }
}

/// A writer engine holding everything, and the blocks it produced.
///
/// A reader is given NONE of them: it starts from a published root and must
/// fetch its way to an answer, which is the situation a cold reader is
/// actually in.
///
/// Built by driving a real engine, so the fixture is the tree this code
/// actually writes rather than one a second builder agrees it should be.
/// Put every internal node's PARITY on the network too, as a writer's race put does (rule 10): a fixture read with
/// racing on (sdk#303) then asks real groups, not ids the network never had.
#[allow(dead_code)]
pub fn publish_parity(root: Cid, all: &mut MemBlocks) {
    let mut at = vec![root];
    while let Some(id) = at.pop() {
        let bytes = all.get(&id).expect("a tree block").to_vec();
        let Ok(n) = freenet_prolly::node::Node::parse(&bytes) else { continue };
        // A leaf's groups are its values stored by reference.
        for (pid, pb) in freenet_prolly::parity::blocks_of(&n, &*all).expect("every member held") {
            all.insert(pid, &pb);
        }
        if !n.is_leaf() {
            at.extend((0..n.len()).map(|i| n.child(i).0));
        }
    }
}

pub fn tree(records: &BTreeMap<Vec<u8>, Vec<u8>>) -> (Cid, MemBlocks) {
    // The writer gets a store of ITS OWN. Sharing the thread's store would put
    // the whole tree where the reader can see it, and every "cold" read below
    // would be a warm one — which is exactly what happened: five tests failed
    // with "0 fetches" because nothing was ever cold.
    let ws = Store::fresh();
    // The fixture writer is not subject to `max_commit_blocks`.
    //
    // That cap exists because a pack's bytes do not survive the `process()`
    // return that made them, so a live commit must fit in ONE return. This
    // writer has no returns: it builds a tree in process, to BE the fixture
    // the tests then read. Capping it would mean no test could use a tree
    // bigger than one live commit, which is the opposite of what a read test
    // wants — a deep tree is the whole point.
    //
    // A bulk import on a real node is the same shape and gets the same
    // answer the cap gives: split it, which is the client's outbox's job.
    let mut w = Engine::new(
        Params {
            max_commit_blocks: usize::MAX,
            // Never saves a context: an in-process writer has no bound to keep.
            max_context_bytes: usize::MAX,
            ..Params::default()
        },
        ws.clone(),
    );
    let ops: Vec<(Vec<u8>, Op)> = records
        .iter()
        .map(|(k, v)| (k.clone(), Op::Put(v.clone())))
        .collect();
    // A NEW tree: there is no head to recover, and the engine is told so. A
    // write before recovery waits for it (sdk#223), so this writer says it.
    let _ = w.step(Event::HeadMissing);
    let mut queue = {
        let out = w.step(Event::forced_write(ClientId(1), WriteId(1), ops));
        ws.absorb(&out);
        out
    };
    let mut all = MemBlocks::default();
    let mut guard = 0;
    while let Some(f) = queue.pop() {
        guard += 1;
        assert!(guard < 100_000, "the writer did not settle");
        match f {
            Effect::PutPack { id, bytes, .. } => {
                // A pack is a TRANSPORT. What the network ends up holding is
                // the blocks inside it, under their own ids — that is the
                // whole point of the format, and a fixture that kept only the
                // pack would model a network no reader could read.
                for (mid, mbytes) in engine::pack::members(&bytes) {
                    all.insert(mid, &mbytes);
                }
                all.insert(id, &bytes);
                let o = w.step(Event::PutConfirmed(id));
                ws.absorb(&o);
                queue.extend(o);
            }
            Effect::PutBlock { id, bytes, .. } => {
                all.insert(id, &bytes);
                let o = w.step(Event::PutConfirmed(id));
                ws.absorb(&o);
                queue.extend(o);
            }
            Effect::UpdateHead { seq, .. } => {
                let o = w.step(Event::HeadConfirmed(seq));
                ws.absorb(&o);
                queue.extend(o);
            }
            Effect::PutParity { id, bytes, .. } => {
                all.insert(id, &bytes);
                let o = w.step(Event::PutConfirmed(id));
                ws.absorb(&o);
                queue.extend(o);
            }
            Effect::Notify {
                state: State::Published,
                ..
            } => {}
            _ => {}
        }
    }
    (w.published_root(), all)
}

/// The root a from-scratch build of the same records produces.
///
/// This is the oracle that matters: the pipeline may reorder, fold, retry and
/// coalesce as much as it likes, and the tree it ends up naming must be the
/// one the library would have built from the final records in one pass. It is
/// the INDEPENDENT side of the differential -- `tree` above descends from the
/// engine, and two sides that both did would only prove the engine agrees
/// with itself.
pub fn rebuild(records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Cid {
    let mut t = TreeBuilder::new(|_, _: &[u8]| {});
    for (k, v) in records {
        t.push_bytes(k, v).unwrap();
    }
    t.finish().unwrap()
}

fn add(total: &mut engine::Shed, s: engine::Shed) {
    total.reads += s.reads;
    total.writes += s.writes;
    total.waits += s.waits;
    total.uncoded += s.uncoded;
}
