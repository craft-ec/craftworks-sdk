//! THE MODEL TEST for the engine library in the page (sdk#213).
//!
//! Two pages on ONE key (one signer) write through a SCRIPTED CLIENT API. On
//! the far side run the real rules, not models of them:
//! * the SIGNER: `signer::serve` over an in-memory host whose synchronous
//!   read sees exactly what the scripted node holds;
//! * the head REGISTER's record format (`engine_delegate::register`), and
//!   its merge as measured (F56): the higher seq wins, equal seq the lower
//!   value, and a losing UPDATE is still answered as a success.
//!
//! Faults, each on its OWN random stream (a new fault never shifts an old
//! one's draws): a PUT lost before it lands, a PUT's answer lost after it
//! landed, a transient PUT refusal (F51's queue), a GET answer lost, a sign
//! request lost, an UPDATE lost, an UPDATE answered and NOT applied, a head
//! read lost, and every answer's delay.
//!
//! The app is an outbox: a write the engine reports `Lost` (a rebase) or
//! `Busy` (one commit at a time) is submitted again, as WritePath does.
//!
//! INVARIANTS, checked on every step:
//! 1. `Published` only while the node's register holds the page's (seq, root).
//! 2. Every UPDATE carries bytes the signer returned.
//! 3. A published root is WHOLE on the node (every block it reaches is held).
//! 4. Once the faults stop, every write is published, and the final tree
//!    holds every key both pages wrote.

use engine::{ClientId, Op as WriteOp, Params, State, WriteId};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use page::{Answer, Op, Page, PutPath};
use std::collections::BTreeMap;

const BLOCK_CODE: &[u8] = b"model block code";
const REGISTER_CODE: &[u8] = b"model register code";

/// A tiny xorshift, one per stream.
#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn new(seed: u64, stream: u64) -> Rng {
        Rng((seed ^ stream.wrapping_mul(0x9E37_79B9_7F4A_7C15)).max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.next() % 1000 < per_mille
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Fault rates, per mille. Zero after the write phase.
#[derive(Clone, Copy)]
struct Faults {
    put_lost: u64,
    put_answer_lost: u64,
    put_refused: u64,
    get_lost: u64,
    sign_lost: u64,
    update_lost: u64,
    update_not_applied: u64,
    head_lost: u64,
}

const FAULTS: Faults = Faults {
    put_lost: 60,
    put_answer_lost: 60,
    put_refused: 60,
    get_lost: 60,
    sign_lost: 80,
    update_lost: 80,
    update_not_applied: 80,
    head_lost: 80,
};
const CALM: Faults = Faults {
    put_lost: 0,
    put_answer_lost: 0,
    put_refused: 0,
    get_lost: 0,
    sign_lost: 0,
    update_lost: 0,
    update_not_applied: 0,
    head_lost: 0,
};

/// The block's kind, recovered from its id (the network holds `kind ‖ body`).
fn state_of(id: &Cid, body: &[u8]) -> Vec<u8> {
    let kind = [freenet_prolly::kind::TREE_NODE, freenet_prolly::kind::RAW, freenet_prolly::kind::PARITY]
        .into_iter()
        .find(|k| freenet_prolly::block_id(*k, body) == *id)
        .expect("every block hashes under one kind");
    let mut s = vec![kind];
    s.extend_from_slice(body);
    s
}

/// The scripted node: what it holds, and the signer's secrets.
struct Node {
    blocks: BTreeMap<Cid, Vec<u8>>,
    /// Block contract id → block id, for the signer's sync read.
    contracts: BTreeMap<[u8; 32], Cid>,
    register: Option<Vec<u8>>,
    register_id: [u8; 32],
    secrets: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Blocks for Node {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.blocks.get(cid).map(Vec::as_slice)
    }
}

struct Host<'a>(&'a mut Node);
impl signer::Host for Host<'_> {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.secrets.get(key).cloned()
    }
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
        self.0.secrets.insert(key.to_vec(), value.to_vec());
        true
    }
    fn contract_state(&self, id: &[u8; 32]) -> Option<Vec<u8>> {
        if *id == self.0.register_id {
            return self.0.register.clone();
        }
        let cid = self.0.contracts.get(id)?;
        self.0.blocks.get(cid).map(|b| state_of(cid, b))
    }
}

fn head_of(state: &[u8]) -> (u64, Cid) {
    let (seq, v) = engine_delegate::register::record_of(state).expect("a register record");
    (seq, v[..32].try_into().expect("32"))
}

impl Node {
    fn new() -> (Node, Vec<u8>) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let params = wire::register_params(&vk, wire::HEAD_NAME);
        let mut n = Node {
            blocks: BTreeMap::new(),
            contracts: BTreeMap::new(),
            register: None,
            register_id: signer::register_id(REGISTER_CODE, &params),
            secrets: BTreeMap::new(),
        };
        let req = signer::Request::Provision {
            signing_key: sk.to_bytes().to_vec(),
            register_code: REGISTER_CODE.to_vec(),
            register_params: params.clone(),
            block_code: BLOCK_CODE.to_vec(),
        };
        assert_eq!(signer::serve(&mut Host(&mut n), &signer::encode_request(&req)), signer::Answer::Provisioned);
        (n, params)
    }

    fn put(&mut self, id: Cid, body: &[u8]) {
        self.contracts.insert(engine_delegate::blocks::contract_for(BLOCK_CODE, &id), id);
        self.blocks.insert(id, body.to_vec());
    }

    /// The register's merge as measured (F56): the higher seq wins; at an
    /// equal seq the lower value; the loser is still answered success.
    fn update(&mut self, state: &[u8]) {
        let (seq, _) = engine_delegate::register::record_of(state).expect("a record");
        let keep_new = match &self.register {
            None => true,
            Some(cur) => {
                let (cseq, cval) = engine_delegate::register::record_of(cur).expect("a record");
                let (_, nval) = engine_delegate::register::record_of(state).expect("a record");
                seq > cseq || (seq == cseq && nval < cval)
            }
        };
        if keep_new {
            self.register = Some(state.to_vec());
        }
    }

    fn head(&self) -> Option<(u64, Cid)> {
        self.register.as_deref().map(head_of)
    }

    /// The REAL signer's answer, exactly as it encodes it and the page's
    /// `wire::signer::read_answer` decodes it.
    fn sign(&mut self, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid) -> signer_proto::Answer {
        let req = signer::Request::Sign {
            prev: signer::Head { seq: prev_seq, root: prev_root },
            next: signer::Next { seq, root, ledger: Vec::new() },
        };
        let answer = signer::serve(&mut Host(self), &signer::encode_request(&req));
        wire::signer::read_answer(&signer_proto::encode_answer(&answer)).expect("a signer answer reads back")
    }

    /// Every entry of the tree at `root`, or `None` if any block is missing.
    fn tree(&self, root: &Cid) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
        use freenet_prolly::range::{range, read_value, Range};
        use std::ops::Bound;
        let mut out = BTreeMap::new();
        let mut after = None;
        loop {
            let r = Range {
                lo: Bound::Unbounded,
                hi: Bound::Unbounded,
                reverse: false,
                after: after.clone(),
                max_entries: 4096,
                max_bytes: usize::MAX,
            };
            let page = range(self, root, &r).ok()?;
            if !page.need.is_empty() {
                return None;
            }
            for (k, v) in &page.entries {
                out.insert(k.clone(), read_value(self, *v).ok()?.to_vec());
            }
            if page.finished() {
                return Some(out);
            }
            after = page.next.clone();
        }
    }
}

/// One op in flight: it takes effect when DELIVERED.
struct Flight {
    at: u64,
    page: usize,
    op: Op,
}

/// A page and its app: an outbox of (key, value) writes not yet published.
struct App {
    page: Page,
    client: ClientId,
    next_id: u64,
    /// write id → (key, value), in flight in the engine.
    inflight: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
    /// Writes to (re-)submit.
    todo: Vec<(Vec<u8>, Vec<u8>)>,
    /// Every key this app wrote and its LAST value.
    wrote: BTreeMap<Vec<u8>, Vec<u8>>,
    published: usize,
}

impl App {
    fn submit(&mut self) {
        if let Some((k, v)) = self.todo.first().cloned() {
            self.todo.remove(0);
            self.next_id += 1;
            self.inflight.insert(self.next_id, (k.clone(), v.clone()));
            self.page.write(self.client, WriteId(self.next_id), vec![(k, WriteOp::Put(v))]);
        }
    }
}

#[derive(Default, Debug)]
struct Seen {
    published: usize,
    lost: usize,
    busy: usize,
    updates: usize,
}

fn run(seed: u64, writes_per_page: usize, path: PutPath) -> Result<Seen, String> {
    let (mut node, _) = Node::new();
    let mut s_delay = Rng::new(seed, 1);
    let mut s_put_lost = Rng::new(seed, 2);
    let mut s_put_ans = Rng::new(seed, 3);
    let mut s_put_ref = Rng::new(seed, 4);
    let mut s_get = Rng::new(seed, 5);
    let mut s_sign = Rng::new(seed, 6);
    let mut s_upd = Rng::new(seed, 7);
    let mut s_upd_na = Rng::new(seed, 8);
    let mut s_head = Rng::new(seed, 9);
    let mut s_app = Rng::new(seed, 10);

    let mut apps: Vec<App> = (0..2)
        .map(|i| App {
            page: Page::new(Params::default(), path),
            client: ClientId(i as u64 + 1),
            next_id: 0,
            inflight: BTreeMap::new(),
            todo: (0..writes_per_page)
                .map(|n| {
                    // 3 KB values: a tree with blocks BELOW its root, so a
                    // root can be signed while a child is missing — the
                    // signer's own check guards the root block only.
                    let mut v = format!("v{seed}-{i}-{n}:").into_bytes();
                    v.resize(3_000, b'a' + (n % 26) as u8);
                    (format!("p{i}/{n:03}").into_bytes(), v)
                })
                .collect(),
            wrote: BTreeMap::new(),
            published: 0,
        })
        .collect();
    for a in &mut apps {
        let todo = a.todo.clone();
        for (k, v) in todo {
            a.wrote.insert(k, v);
        }
    }

    let mut flights: Vec<Flight> = Vec::new();
    let mut seen = Seen::default();
    let mut now = 0u64;
    let calm_at = 400_000u64;
    let end = calm_at + 600_000;
    while now < end {
        let faults = if now < calm_at { FAULTS } else { CALM };
        // The apps submit.
        for a in &mut apps {
            if a.inflight.is_empty() && !a.todo.is_empty() && s_app.chance(300) {
                a.submit();
            }
        }
        // Ops leave the pages.
        for (i, a) in apps.iter_mut().enumerate() {
            for op in a.page.take_ops() {
                // INVARIANT 2.
                if let Op::Update { state } = &op {
                    if !a.page.signer_records().contains(state) {
                        return Err(format!("page {i} UPDATEd bytes the signer never returned"));
                    }
                }
                flights.push(Flight { at: now + 1 + s_delay.below(300), page: i, op });
            }
        }
        // Deliver what is due, in order.
        flights.sort_by_key(|f| f.at);
        let due: Vec<Flight> = {
            let n = flights.iter().take_while(|f| f.at <= now).count();
            flights.drain(..n).collect()
        };
        for f in due {
            let answer = match f.op {
                Op::Put { id, bytes } => {
                    if s_put_lost.chance(faults.put_lost) {
                        // On the wrapper path an answer is no evidence: a
                        // put that never landed is still answered ok, so an
                        // executor that trusted it would publish a hole.
                        (path == PutPath::Wrapper).then_some(Answer::PutOk(id))
                    } else if s_put_ref.chance(faults.put_refused) {
                        Some(Answer::PutRefused { id, transient: true })
                    } else {
                        node.put(id, &bytes);
                        // The wrapper path: the answer never reaches the page
                        // (sdk#214) — here it does, and must confirm nothing.
                        if s_put_ans.chance(faults.put_answer_lost) {
                            None
                        } else {
                            Some(Answer::PutOk(id))
                        }
                    }
                }
                Op::Get { id } => {
                    if s_get.chance(faults.get_lost) {
                        None
                    } else {
                        Some(match node.blocks.get(&id) {
                            Some(b) => Answer::Got { id, bytes: b.clone() },
                            None => Answer::GetMissed(id),
                        })
                    }
                }
                Op::Sign { prev_seq, prev_root, seq, root } => {
                    if s_sign.chance(faults.sign_lost) {
                        None
                    } else {
                        Some(Answer::Signer(node.sign(prev_seq, prev_root, seq, root)))
                    }
                }
                Op::Update { state } => {
                    seen.updates += 1;
                    if s_upd.chance(faults.update_lost) {
                        None
                    } else {
                        if !s_upd_na.chance(faults.update_not_applied) {
                            node.update(&state);
                        }
                        Some(Answer::Updated)
                    }
                }
                Op::AskHeld { id } => {
                    if s_head.chance(faults.head_lost) {
                        None
                    } else {
                        Some(Answer::Held { id, present: node.blocks.contains_key(&id) })
                    }
                }
                Op::ReadHead => {
                    if s_head.chance(faults.head_lost) {
                        None
                    } else {
                        Some(Answer::Head(node.head()))
                    }
                }
            };
            let Some(answer) = answer else { continue };
            apps[f.page].page.answer(answer, now);
            check(&mut apps, f.page, &node, &mut seen)?;
        }
        now += 5;
        for i in 0..apps.len() {
            apps[i].page.tick(now);
            check(&mut apps, i, &node, &mut seen)?;
        }
        if now > calm_at && apps.iter().all(|a| a.todo.is_empty() && a.inflight.is_empty()) && flights.is_empty() {
            break;
        }
    }
    // INVARIANT 4.
    for (i, a) in apps.iter().enumerate() {
        if !a.todo.is_empty() || !a.inflight.is_empty() {
            return Err(format!(
                "page {i}: {} writes never published ({} still in the engine); unusable: {:?}",
                a.todo.len() + a.inflight.len(),
                a.inflight.len(),
                a.page.unusable()
            ));
        }
    }
    // Nothing the pages could not do: a refusal hidden in `unusable` would
    // otherwise pass as long as the retries got round it.
    for (i, a) in apps.iter().enumerate() {
        if !a.page.unusable().is_empty() {
            return Err(format!("page {i} recorded: {:?}", a.page.unusable()));
        }
    }
    let (_, root) = node.head().ok_or("no head at the end")?;
    let tree = node.tree(&root).ok_or("the final root is not whole on the node")?;
    for a in &apps {
        for (k, v) in &a.wrote {
            if tree.get(k) != Some(v) {
                return Err(format!("the final tree lost {:?}", String::from_utf8_lossy(k)));
            }
        }
    }
    Ok(seen)
}

fn check(apps: &mut [App], i: usize, node: &Node, seen: &mut Seen) -> Result<(), String> {
    let a = &mut apps[i];
    for (_, wid, state) in a.page.take_notices() {
        match state {
            State::Published => {
                // INVARIANT 1.
                let (seq, root) = a.page.published();
                if node.head() != Some((seq, root)) {
                    return Err(format!(
                        "page {i}: write {} Published at ({seq}, {root:?}) while the register holds {:?}",
                        wid.0,
                        node.head()
                    ));
                }
                // INVARIANT 3.
                if node.tree(&root).is_none() {
                    return Err(format!("page {i}: Published at a root that is not whole on the node"));
                }
                if a.inflight.remove(&wid.0).is_some() {
                    a.published += 1;
                    seen.published += 1;
                }
            }
            State::Lost | State::Busy | State::Failed | State::TooLarge { .. } => {
                if state == State::Lost {
                    seen.lost += 1;
                } else if state == State::Busy {
                    seen.busy += 1;
                }
                if let Some(w) = a.inflight.remove(&wid.0) {
                    a.todo.push(w);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

const SEEDS: u64 = 40;
const WRITES: usize = 12;

#[test]
fn two_pages_on_one_key_publish_every_write_through_faults_and_the_invariants_hold() {
    let mut total = Seen::default();
    for seed in 1..=SEEDS {
        let s = run(seed, WRITES, PutPath::Page).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        total.published += s.published;
        total.lost += s.lost;
        total.busy += s.busy;
        total.updates += s.updates;
    }
    println!("{SEEDS} seeds × 2 pages × {WRITES} writes: {total:?}");
    // The model is not vacuous: the race and the faults were reached.
    assert_eq!(total.published, SEEDS as usize * 2 * WRITES, "not every write was published once");
    assert!(total.lost > 0, "no rebase was ever reached: the two pages never raced");
    assert!(total.updates > total.published / 2, "too few UPDATEs for the writes published");
}

/// THE CONTROL for invariant 3: the whole-tree check FAILS on a root with a
/// block missing. No executor mutant reached it — the signer's own root check
/// and the `after` ordering stop a root going out before its blocks, so
/// invariant 1 fires first — so this shows the check can fire at all.
#[test]
fn control_the_whole_tree_check_fails_on_a_missing_block() {
    let (mut node, _) = Node::new();
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.write(ClientId(1), WriteId(1), vec![(b"k".to_vec(), WriteOp::Put(vec![7u8; 5_000]))]);
    let mut puts = Vec::new();
    for _ in 0..20 {
        for op in p.take_ops() {
            match op {
                Op::Put { id, bytes } => {
                    node.put(id, &bytes);
                    puts.push(id);
                    p.answer(Answer::PutOk(id), 0);
                }
                Op::ReadHead => p.answer(Answer::Head(node.head()), 0),
                Op::Sign { prev_seq, prev_root, seq, root } => {
                    let a = node.sign(prev_seq, prev_root, seq, root);
                    p.answer(Answer::Signer(a), 0);
                }
                Op::Update { state } => {
                    node.update(&state);
                    p.answer(Answer::Updated, 0);
                }
                Op::Get { id } => p.answer(Answer::GetMissed(id), 0),
                Op::AskHeld { id } => p.answer(Answer::Held { id, present: node.blocks.contains_key(&id) }, 0),
            }
        }
    }
    let (_, root) = node.head().expect("the write published");
    assert!(node.tree(&root).is_some(), "the published tree is whole");
    let gone = *puts.iter().find(|b| **b != root).expect("a block under the root");
    node.blocks.remove(&gone);
    assert!(node.tree(&root).is_none(), "the whole-tree check passed over a missing block");
}

/// THE WRAPPER PATH: a PUT's answer confirms nothing, only the signer's
/// read-local `Held` does. Same faults, same invariants.
#[test]
fn on_the_wrapper_path_a_put_is_confirmed_by_held_and_the_invariants_hold() {
    let mut published = 0;
    for seed in 1..=SEEDS / 2 {
        let s = run(seed, WRITES, PutPath::Wrapper).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        published += s.published;
    }
    assert_eq!(published, (SEEDS / 2) as usize * 2 * WRITES);
}
