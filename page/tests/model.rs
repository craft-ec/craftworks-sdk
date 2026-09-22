//! THE MODEL TEST for the engine library in the page (sdk#213).
//!
//! Two pages on ONE key (one signer) write through a SCRIPTED CLIENT API. On
//! the far side run the real rules, not models of them:
//! * the SIGNER: `signer::serve` over an in-memory host whose synchronous
//!   read sees exactly what the scripted node holds;
//! * the head REGISTER itself: every UPDATE goes through the contract's own
//!   `update_state` (craftec-register-contract, natively) — the lower
//!   BLAKE3(value) at an equal seq, F56's sticky fork evidence, and a losing
//!   UPDATE still answered as a success.
//!
//! Faults, each on its OWN random stream (a new fault never shifts an old
//! one's draws): a PUT lost before it lands, a PUT's answer lost after it
//! landed, a transient PUT refusal (F51's queue), a GET answer lost, a sign
//! request lost, the signer failing to save its record (`RecordNotSaved`),
//! an UPDATE lost, an UPDATE answered and NOT applied, a head read lost, and
//! every answer's delay.
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
use page::{Answer, Ms, Op, Page, PutPath};
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
    record_not_saved: u64,
    /// The node's `HeadChanged` push to a page is dropped (a full
    /// notification channel, an evicted subscription).
    hint_lost: u64,
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
    record_not_saved: 60,
    hint_lost: 300,
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
    record_not_saved: 0,
    hint_lost: 0,
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
    register_params: Vec<u8>,
    /// Each DEVICE's signer secrets (the key and its record): one device is
    /// two tabs on one signer; two devices are two signers with one key on one
    /// register — the user's own devices (sdk#225).
    secrets: Vec<BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Which device's signer the current request is served by.
    dev: usize,
    /// The signer's record write fails while this is set (a fault).
    record_fails: bool,
    /// Every head the register has ever held: what "read back" can mean.
    held_heads: std::collections::BTreeSet<(u64, Cid)>,
}

impl Blocks for Node {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.blocks.get(cid).map(Vec::as_slice)
    }
}

struct Host<'a>(&'a mut Node);
impl signer::Host for Host<'_> {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.secrets[self.0.dev].get(key).cloned()
    }
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
        if self.0.record_fails && key == signer::RECORD {
            return false;
        }
        let d = self.0.dev;
        self.0.secrets[d].insert(key.to_vec(), value.to_vec());
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

/// F56's equal-seq rule, as the Register decides it (the lower BLAKE3 of the
/// value, which is the bare root today).
fn page_beats(a: &Cid, b: &Cid) -> bool {
    blake3::hash(a).as_bytes() < blake3::hash(b).as_bytes()
}

fn seq_of(node: &Node) -> u64 {
    node.head().map_or(0, |h| h.0)
}

fn head_of(state: &[u8]) -> (u64, Cid) {
    let (seq, v) = engine_delegate::register::record_of(state).expect("a register record");
    (seq, v[..32].try_into().expect("32"))
}

impl Node {
    fn new() -> (Node, Vec<u8>) {
        Node::with_devices(1)
    }

    fn with_devices(devices: usize) -> (Node, Vec<u8>) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let params = wire::register_params(&vk, wire::HEAD_NAME);
        let mut n = Node {
            blocks: BTreeMap::new(),
            contracts: BTreeMap::new(),
            register: None,
            register_id: signer::register_id(REGISTER_CODE, &params),
            register_params: params.clone(),
            secrets: vec![BTreeMap::new(); devices],
            dev: 0,
            record_fails: false,
            held_heads: Default::default(),
        };
        let req = signer::Request::Provision {
            signing_key: sk.to_bytes().to_vec(),
            register_code: REGISTER_CODE.to_vec(),
            register_params: params.clone(),
            block_code: BLOCK_CODE.to_vec(),
        };
        for d in 0..devices {
            n.dev = d;
            assert_eq!(signer::serve(&mut Host(&mut n), &signer::encode_request(1, &req)), signer::Answer::Provisioned);
        }
        n.dev = 0;
        (n, params)
    }

    fn put(&mut self, id: Cid, body: &[u8]) {
        self.contracts.insert(engine_delegate::blocks::contract_for(BLOCK_CODE, &id), id);
        self.blocks.insert(id, body.to_vec());
    }

    /// An UPDATE, through the Register contract's OWN `update_state`: the
    /// state the node keeps is exactly the contract's answer (F56: a loser is
    /// still told success; that is the caller's `Updated`).
    fn update(&mut self, state: &[u8]) {
        use freenet_stdlib::prelude::*;
        let params = Parameters::from(self.register_params.clone());
        let next = match &self.register {
            None => {
                let v = <craftec_register_contract::Register as ContractInterface>::validate_state(
                    params,
                    State::from(state.to_vec()),
                    RelatedContracts::default(),
                );
                assert!(matches!(v, Ok(ValidateResult::Valid)), "the signer's record is not a valid register state");
                state.to_vec()
            }
            Some(cur) => {
                let m = <craftec_register_contract::Register as ContractInterface>::update_state(
                    params,
                    State::from(cur.clone()),
                    vec![UpdateData::State(State::from(state.to_vec()))],
                )
                .expect("the register merged");
                m.new_state.expect("the register answered a state").as_ref().to_vec()
            }
        };
        self.register = Some(next);
        if let Some(h) = self.head() {
            self.held_heads.insert(h);
        }
    }

    fn head(&self) -> Option<(u64, Cid)> {
        self.register.as_deref().map(head_of)
    }

    /// The REAL signer's answer, exactly as it encodes it and the page's
    /// `wire::signer::read_answer` decodes it.
    fn sign(&mut self, id: u32, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid) -> (u32, signer_proto::Answer) {
        let req = signer::Request::Sign {
            prev: signer::Head { seq: prev_seq, root: prev_root },
            next: signer::Next { seq, root, ledger: Vec::new() },
        };
        // Through the BYTES both ways: the request under the page's id, the answer under the id the signer echoes.
        let served = signer::serve_full(&mut Host(self), &signer::encode_request(id, &req));
        wire::signer::read_answer(&signer::reply(&served)).expect("a signer answer reads back")
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
    /// When each write was submitted (page ms): `Stalled` is judged by it.
    submitted: BTreeMap<u64, u64>,
    /// Writes to (re-)submit.
    todo: Vec<(Vec<u8>, Vec<u8>)>,
    /// Every key this app wrote and its LAST value.
    wrote: BTreeMap<Vec<u8>, Vec<u8>>,
    published: usize,
}

impl App {
    fn submit(&mut self, now: u64) {
        if let Some((k, v)) = self.todo.first().cloned() {
            self.todo.remove(0);
            self.next_id += 1;
            self.inflight.insert(self.next_id, (k.clone(), v.clone()));
            self.submitted.insert(self.next_id, now);
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
    record_not_saved: usize,
    landings: u32,
    most_landing_updates: u32,
    /// (page, write, the head it was told Published at).
    published_at: Vec<(usize, u64, (u64, Cid), Option<(Vec<u8>, Vec<u8>)>)>,
    /// Published writes whose head a same-seq WINNER later displaced, and
    /// whose value the final tree does not hold: the per-key merge (#225b)
    /// is what keeps them. Counted and named here, never silent.
    displaced: usize,
    /// Seqs the register held under two roots: the devices really raced.
    races: usize,
}

/// What a run plays.
#[derive(Clone, Copy)]
struct Cfg {
    faults: Faults,
    /// 1: the two pages are two TABS on one signer. 2: two DEVICES of one
    /// identity — two signers, one key, one register (sdk#225): they race at
    /// every seq, and the register's tie-break decides.
    devices: usize,
    /// Every `HeadChanged` push is dropped, calm or not: an idle page learns
    /// only by the backstop read.
    no_hints: bool,
}

const NORMAL: Cfg = Cfg { faults: FAULTS, devices: 1, no_hints: false };

/// Does `later` descend from `h` through the signer's records (next → prev)?
fn descends(edges: &BTreeMap<(u64, Cid), (u64, Cid)>, later: (u64, Cid), h: (u64, Cid)) -> bool {
    let mut cur = later;
    loop {
        if cur == h {
            return true;
        }
        if cur.0 <= h.0 {
            return false;
        }
        match edges.get(&cur) {
            Some(prev) => cur = *prev,
            None => return false,
        }
    }
}

fn run(seed: u64, writes_per_page: usize, path: PutPath) -> Result<Seen, String> {
    run_with(seed, writes_per_page, path, NORMAL)
}

fn run_with(seed: u64, writes_per_page: usize, path: PutPath, cfg: Cfg) -> Result<Seen, String> {
    let mut edges: BTreeMap<(u64, Cid), (u64, Cid)> = BTreeMap::new();
    let (mut node, _) = Node::with_devices(cfg.devices);
    let dev_of = |page: usize| if cfg.devices == 1 { 0 } else { page };
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
    let mut s_rec = Rng::new(seed, 11);
    let mut s_hint = Rng::new(seed, 12);

    let mut apps: Vec<App> = (0..2)
        .map(|i| App {
            page: Page::new(Params::default(), path),
            client: ClientId(i as u64 + 1),
            next_id: 0,
            inflight: BTreeMap::new(),
            submitted: BTreeMap::new(),
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
        let faults = if now < calm_at { cfg.faults } else { CALM };
        // The apps submit.
        for a in &mut apps {
            if a.inflight.is_empty() && !a.todo.is_empty() && s_app.chance(300) {
                a.submit(now);
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
                Op::Sign { id, prev_seq, prev_root, seq, root } => {
                    if s_sign.chance(faults.sign_lost) {
                        None
                    } else {
                        node.record_fails = s_rec.chance(faults.record_not_saved);
                        node.dev = dev_of(f.page);
                        let (id, a) = node.sign(id, prev_seq, prev_root, seq, root);
                        node.record_fails = false;
                        if let Some(rec) = node.secrets[node.dev].get(signer::RECORD) {
                            let rec: signer::Record = bincode::deserialize(rec).expect("the signer's record");
                            edges.insert((rec.next.seq, rec.next.root), (rec.prev.seq, rec.prev.root));
                        }
                        if matches!(a, signer_proto::Answer::Refused(signer_proto::Why::RecordNotSaved)) {
                            seen.record_not_saved += 1;
                        }
                        Some(Answer::Signer { id, answer: a })
                    }
                }
                Op::Update { state } => {
                    seen.updates += 1;
                    if s_upd.chance(faults.update_lost) {
                        None
                    } else {
                        if !s_upd_na.chance(faults.update_not_applied) {
                            let before = node.head();
                            node.update(&state);
                            // THE SUBSCRIPTION: a head that moved is pushed to
                            // every OTHER page as `HeadChanged` — a hint, and
                            // a lossy one.
                            if node.head() != before {
                                for j in 0..apps.len() {
                                    if j != f.page && !cfg.no_hints && !s_hint.chance(faults.hint_lost) {
                                        apps[j].page.head_hint();
                                    }
                                }
                            }
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
            apps[f.page].page.answer(answer, Ms(now));
            check(&mut apps, f.page, &node, &mut seen, now)?;
        }
        now += 5;
        for i in 0..apps.len() {
            apps[i].page.tick(Ms(now));
            check(&mut apps, i, &node, &mut seen, now)?;
        }
        let converged = apps.iter().all(|a| Some(a.page.published()) == node.head());
        if now > calm_at && apps.iter().all(|a| a.todo.is_empty() && a.inflight.is_empty()) && flights.is_empty() && converged {
            break;
        }
    }
    for a in &apps {
        let (l, m) = a.page.landings();
        seen.landings += l;
        seen.most_landing_updates = seen.most_landing_updates.max(m);
    }
    // RACES: seqs the register held under two roots (non-vacuity for the
    // two-device runs).
    let mut per_seq: BTreeMap<u64, usize> = BTreeMap::new();
    for (sq, _) in &node.held_heads {
        *per_seq.entry(*sq).or_default() += 1;
    }
    seen.races = per_seq.values().filter(|n| **n > 1).count();
    // CONVERGED: every page ends on the register's head — an IDLE one too,
    // by the `HeadChanged` hint or, every hint lost, the backstop read.
    for (i, a) in apps.iter().enumerate() {
        if Some(a.page.published()) != node.head() {
            return Err(format!("page {i} ends on {:?}, the register on {:?}: it never learned", a.page.published().0, node.head().map(|h| h.0)));
        }
    }
    // INVARIANT 1, in full: a Published write's head H was read from the
    // register (checked as it happened), AND the final register head
    // descends from H through the signers' records (BOTH devices'), OR the
    // final chain passes through ANOTHER root at H's own seq — a sibling
    // device's head the register kept, by the equal-seq tie-break or by a
    // higher seq built on it (F56: the higher seq wins). The same identity
    // never forks (sdk#225): that is the register's rule, never a fork. Until
    // the per-key merge (#225b) those writes' values are gone, and they are
    // COUNTED (`displaced`), not silent.
    if let Some(fin) = node.head() {
        let tree = node.tree(&fin.1).unwrap_or_default();
        let mut chain: BTreeMap<u64, Cid> = BTreeMap::new();
        let mut c = fin;
        chain.insert(c.0, c.1);
        while let Some(p) = edges.get(&c) {
            c = *p;
            chain.insert(c.0, c.1);
        }
        for (i, wid, h, kv) in &seen.published_at {
            if descends(&edges, fin, *h) {
                continue;
            }
            let sibling = chain.get(&h.0).is_some_and(|x| *x != h.1 && node.held_heads.contains(&(h.0, *x)));
            if !sibling {
                return Err(format!(
                    "page {i}: write {wid} was Published at {:?}, which the final head {:?} does not descend from through any sibling at that seq",
                    h.0, fin.0
                ));
            }
            if kv.as_ref().is_some_and(|(k, v)| tree.get(k) != Some(v)) {
                seen.displaced += 1;
            }
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
                // Only a write Published at a head a winner displaced may be
                // missing (counted above, #225b's to keep).
                let displaced = seen.published_at.iter().any(|(_, _, h, kv)| {
                    kv.as_ref().is_some_and(|(pk, pv)| pk == k && pv == v) && !descends(&edges, (seq_of(&node), root), *h)
                });
                if !displaced {
                    return Err(format!("the final tree lost {:?}", String::from_utf8_lossy(k)));
                }
            }
        }
    }
    Ok(seen)
}

fn check(apps: &mut [App], i: usize, node: &Node, seen: &mut Seen, now: u64) -> Result<(), String> {
    // THE REGISTER IS NEVER 2+ BEHIND THE SIGNER'S RECORD (1b on the sign
    // side, the architect's attack): past one, the record for the seq between
    // is overwritten and no page could land it.
    for secrets in &node.secrets {
        if let Some(rec) = secrets.get(signer::RECORD) {
            let rec: signer::Record = bincode::deserialize(rec).expect("the signer's record");
            let reg = node.head().map_or(0, |h| h.0);
            if rec.next.seq > reg + 1 {
                return Err(format!("the signer's record (seq {}) is {} ahead of the register (seq {reg})", rec.next.seq, rec.next.seq - reg));
            }
        }
    }
    let a = &mut apps[i];
    for (_, wid, state) in a.page.take_notices() {
        match state {
            State::Published => {
                // INVARIANT 1: Published at a head the register was READ to
                // hold — now or earlier (a write that changes nothing is
                // Published at the engine's published head, and another page
                // may have moved the register on since; sdk#160) — and the
                // write's own value is in that head's tree.
                let (seq, root) = a.page.published();
                if !node.held_heads.contains(&(seq, root)) {
                    return Err(format!(
                        "page {i}: write {} Published at ({seq}, {root:?}), a head the register never held (it holds {:?})",
                        wid.0,
                        node.head()
                    ));
                }
                if let Some((k, v)) = a.inflight.get(&wid.0) {
                    let tree = node.tree(&root).unwrap_or_default();
                    if tree.get(k) != Some(v) {
                        return Err(format!("page {i}: write {} Published at ({seq}, ..) whose tree does not hold its value", wid.0));
                    }
                }
                // INVARIANT 3.
                if node.tree(&root).is_none() {
                    return Err(format!("page {i}: Published at a root that is not whole on the node"));
                }
                seen.published_at.push((i, wid.0, (seq, root), a.inflight.get(&wid.0).cloned()));
                if a.inflight.remove(&wid.0).is_some() {
                    a.published += 1;
                    seen.published += 1;
                }
            }
            State::Lost | State::Busy | State::Failed | State::TooLarge { .. } => {
                if state == State::Lost {
                    // ONLY A CONFIRMED WINNER IS ADOPTED (the architect's
                    // attack on sdk#225, case 1): the head this page now
                    // stands on never loses the tie-break to a record this
                    // page's signer made at the same seq.
                    let (ps, pr) = a.page.published();
                    for rec in a.page.signer_records() {
                        let (rs, rr) = head_of(rec);
                        if rs == ps && rr != pr && page_beats(&rr, &pr) {
                            return Err(format!("page {i}: adopted ({ps}, ..), which LOSES the tie-break to its own record at that seq"));
                        }
                    }
                    seen.lost += 1;
                } else if state == State::Busy {
                    seen.busy += 1;
                }
                if let Some(w) = a.inflight.remove(&wid.0) {
                    a.todo.push(w);
                }
            }
            // NO WRITE IS STALLED BEFORE ITS BUDGET: the engine says Stalled
            // after `max_accept_age` (64) SECONDS unconfirmed. A page clock
            // passed through in ms (#215's defect) told writes Stalled after
            // 64 ms — the model now sees that, not only the unit test.
            // The engine's clock is WHOLE seconds (`engine_seconds` floors),
            // so 64 of its seconds can be just over 63 real ones: seed 10's
            // fork run is told Stalled at 63 815 ms, inside the budget.
            State::Stalled => {
                let since = a.submitted.get(&wid.0).copied().unwrap_or(0);
                let age = now.saturating_sub(since);
                if age <= 63_000 {
                    return Err(format!("page {i}: write {} told Stalled {age} ms after it was submitted (budget 64 engine seconds, over 63 s)", wid.0));
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
        total.record_not_saved += s.record_not_saved;
        total.landings += s.landings;
        total.most_landing_updates = total.most_landing_updates.max(s.most_landing_updates);
    }
    println!("{SEEDS} seeds × 2 pages × {WRITES} writes: {total:?}");
    // The model is not vacuous: the race and the faults were reached.
    assert_eq!(total.published, SEEDS as usize * 2 * WRITES, "not every write was published once");
    assert!(total.lost > 0, "no rebase was ever reached: the two pages never raced");
    assert!(total.updates > total.published / 2, "too few UPDATEs for the writes published");
    assert!(total.record_not_saved > 0, "the signer never failed to save its record: RecordNotSaved unexercised");
    // The LAND cell is exercised.
    assert!(total.landings > 0, "no page ever landed a signer's record: the cell is unexercised");
}

/// LANDING UNDER LOSS: UPDATEs lost three times as often. At least one
/// landing's UPDATE is lost twice and re-sent, and everything still publishes
/// with every invariant (main's condition 2).
#[test]
fn a_landing_whose_update_is_lost_twice_still_lands() {
    let harsh = Cfg { faults: Faults { update_lost: 300, ..FAULTS }, ..NORMAL };
    let mut most = 0;
    let mut landings = 0;
    for seed in 1..=20 {
        let s = run_with(seed, WRITES, PutPath::Page, harsh).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(s.published, 2 * WRITES, "seed {seed}: not every write published");
        most = most.max(s.most_landing_updates);
        landings += s.landings;
    }
    println!("harsh: {landings} landings, most UPDATEs one landing needed: {most}");
    assert!(landings > 0, "nothing landed");
    assert!(most >= 3, "no landing's UPDATE was lost twice (most: {most})");
}

/// TWO DEVICES OF ONE IDENTITY (sdk#225): two signers, one key, one
/// register, racing at every seq through the same faults, `HeadChanged`
/// pushes lost 30% of the time. The same identity never forks: no page is
/// ever unusable, every adopt is the tie-break's CONFIRMED winner, every
/// write publishes, and both pages end on the register's head. Non-vacuous:
/// the register held some seq under two roots. A Published write a winner
/// displaced is counted — the per-key merge (#225b) is what keeps it.
#[test]
fn two_devices_on_one_key_race_and_no_page_is_ever_unusable() {
    let cfg = Cfg { devices: 2, ..NORMAL };
    let (mut races, mut displaced, mut lost) = (0, 0, 0);
    for seed in 1..=SEEDS / 2 {
        let s = run_with(seed, WRITES, PutPath::Page, cfg).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(s.published, 2 * WRITES, "seed {seed}: not every write was published once");
        races += s.races;
        displaced += s.displaced;
        lost += s.lost;
    }
    println!("two devices: {races} raced seqs, {lost} Lost and re-sent, {displaced} Published writes displaced by a winner (#225b keeps them)");
    assert!(races > 0, "the two devices never raced at a seq: the test is vacuous");
}

/// AN IDLE PAGE LEARNS WITH EVERY HINT LOST: no `HeadChanged` ever reaches a
/// page, so only the backstop read (HEAD_BACKSTOP_MS) tells a device that
/// stopped writing that the other one moved the register. Both still end on
/// the register's head (the run's convergence check).
#[test]
fn with_every_hint_lost_an_idle_device_still_learns_by_the_backstop() {
    let cfg = Cfg { devices: 2, no_hints: true, ..NORMAL };
    for seed in 1..=6 {
        run_with(seed, WRITES, PutPath::Page, cfg).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    }
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
                    p.answer(Answer::PutOk(id), Ms(0));
                }
                Op::ReadHead => p.answer(Answer::Head(node.head()), Ms(0)),
                Op::Sign { id, prev_seq, prev_root, seq, root } => {
                    let (id, a) = node.sign(id, prev_seq, prev_root, seq, root);
                    p.answer(Answer::Signer { id, answer: a }, Ms(0));
                }
                Op::Update { state } => {
                    node.update(&state);
                    p.answer(Answer::Updated, Ms(0));
                }
                Op::Get { id } => p.answer(Answer::GetMissed(id), Ms(0)),
                Op::AskHeld { id } => p.answer(Answer::Held { id, present: node.blocks.contains_key(&id) }, Ms(0)),
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

/// THE LAND CELL, where it is needed: page A signs its commit, its UPDATE
/// never lands, and A is GONE. Page B is stale (it never saw A's first head),
/// so the signer answers it NotNext naming A's unlanded record. The register is
/// one behind, so B LANDS A's record itself (asking from the register's head
/// with next = register + 1: `AlreadySigned`), adopts it once the register
/// shows it, and B's own write — told Lost by that rebase — publishes on top.
#[test]
fn a_stale_page_lands_a_gone_pages_record_then_publishes() {
    let (mut node, _) = Node::new();
    let mut a = Page::new(Params::default(), PutPath::Page);
    let mut b = Page::new(Params::default(), PutPath::Page);
    let mut now = 1_000u64;
    // Serve every op of `p`, dropping UPDATEs when `drop_updates`.
    fn serve(p: &mut Page, node: &mut Node, now: &mut u64, drop_updates: bool) {
        for _ in 0..400 {
            let ops = p.take_ops();
            if ops.is_empty() {
                if !p.waiting() {
                    return;
                }
                *now = p.next_due().map_or(*now + 1, |d| d.0.max(*now + 1));
                p.tick(Ms(*now));
                continue;
            }
            for op in ops {
                let ans = match op {
                    Op::Put { id, bytes } => {
                        node.put(id, &bytes);
                        Some(Answer::PutOk(id))
                    }
                    Op::Get { id } => Some(match node.blocks.get(&id) {
                        Some(b) => Answer::Got { id, bytes: b.clone() },
                        None => Answer::GetMissed(id),
                    }),
                    Op::Sign { id, prev_seq, prev_root, seq, root } => {
                        let (id, answer) = node.sign(id, prev_seq, prev_root, seq, root);
                        Some(Answer::Signer { id, answer })
                    }
                    Op::Update { state } => {
                        if drop_updates {
                            None
                        } else {
                            node.update(&state);
                            Some(Answer::Updated)
                        }
                    }
                    Op::ReadHead => Some(Answer::Head(node.head())),
                    Op::AskHeld { id } => Some(Answer::Held { id, present: node.blocks.contains_key(&id) }),
                };
                if let Some(ans) = ans {
                    p.answer(ans, Ms(*now));
                }
            }
        }
    }
    // Both start on the empty tree.
    serve(&mut a, &mut node, &mut now, false);
    serve(&mut b, &mut node, &mut now, false);
    // A publishes seq 1.
    a.write(ClientId(1), WriteId(1), vec![(b"a1".to_vec(), WriteOp::Put(b"x".to_vec()))]);
    serve(&mut a, &mut node, &mut now, false);
    assert_eq!(node.head().map(|h| h.0), Some(1));
    // A's second commit is SIGNED (record seq 2), its UPDATE never lands, and A is gone.
    a.write(ClientId(1), WriteId(2), vec![(b"a2".to_vec(), WriteOp::Put(b"y".to_vec()))]);
    for _ in 0..30 {
        let ops = a.take_ops();
        for op in ops {
            let ans = match op {
                Op::Put { id, bytes } => {
                    node.put(id, &bytes);
                    Some(Answer::PutOk(id))
                }
                Op::Sign { id, prev_seq, prev_root, seq, root } => {
                        let (id, answer) = node.sign(id, prev_seq, prev_root, seq, root);
                        Some(Answer::Signer { id, answer })
                    }
                _ => None,
            };
            if let Some(ans) = ans {
                a.answer(ans, Ms(now));
            }
        }
    }
    let rec: signer::Record = bincode::deserialize(node.secrets[0].get(signer::RECORD).expect("a record")).expect("decodes");
    assert_eq!((rec.next.seq, node.head().map(|h| h.0)), (2, Some(1)), "the record is not one ahead of the register");
    drop(a);
    // B, stale at seq 0, writes: NotNext names record 2; B lands it.
    b.write(ClientId(2), WriteId(1), vec![(b"b1".to_vec(), WriteOp::Put(b"z".to_vec()))]);
    serve(&mut b, &mut node, &mut now, false);
    let lost = b.take_notices().iter().any(|(_, w, s)| w.0 == 1 && *s == State::Lost);
    assert!(lost, "B's write was not told Lost by the rebase onto the landed record");
    assert!(b.landings().0 > 0, "B never landed the record");
    assert_eq!(node.head().map(|h| h.0), Some(2), "A's record was never landed");
    b.write(ClientId(2), WriteId(2), vec![(b"b1".to_vec(), WriteOp::Put(b"z".to_vec()))]);
    serve(&mut b, &mut node, &mut now, false);
    let (_, root) = node.head().expect("a head");
    let tree = node.tree(&root).expect("whole");
    for (k, v) in [(&b"a1"[..], &b"x"[..]), (b"a2", b"y"), (b"b1", b"z")] {
        assert_eq!(tree.get(k).map(Vec::as_slice), Some(v), "{:?} missing from the final tree", String::from_utf8_lossy(k));
    }
    assert!(b.unusable().is_empty(), "{:?}", b.unusable());
}
