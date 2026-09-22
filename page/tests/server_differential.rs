//! THE DIFFERENTIAL: `page::Server` against the engine delegate's Shell
//! (main's ruling B, condition 2).
//!
//! The same protocol requests go to both:
//! * the OLD side is testkit's `FullNode` — the Shell over a scripted node,
//!   signing the head with its own key;
//! * the NEW side is `page::Server` over a scripted CLIENT API — the real
//!   signer (`signer::serve`) signs, and every UPDATE goes through the
//!   Register contract's own `update_state`.
//!
//! Their replies are compared decoded, modulo the head's signer — the one
//! difference ruling B allows. Where the SIGNER makes a case behave
//! differently by design (a restart mid-commit: the signer's record lands the
//! commit the Shell lost), the divergence is asserted explicitly, not hidden.

use engine::Params;
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use page::server::{Server, SignerFacts};
use page::{Ms, Answer, Op, Page, PutPath};
use protocol::{Reply, Request, WriteState};
use std::collections::BTreeMap;
use testkit::full_node::FullNode;

const BLOCK_CODE: &[u8] = b"differential block code";
const REGISTER_CODE: &[u8] = b"differential register code";
const SESSION: u64 = 7;
const VERSION: u16 = 4;

fn state_of(id: &Cid, body: &[u8]) -> Vec<u8> {
    let kind = [freenet_prolly::kind::TREE_NODE, freenet_prolly::kind::RAW, freenet_prolly::kind::PARITY]
        .into_iter()
        .find(|k| freenet_prolly::block_id(*k, body) == *id)
        .expect("every block hashes under one kind");
    let mut s = vec![kind];
    s.extend_from_slice(body);
    s
}

/// The node behind the page's client API: blocks, the head Register, and the
/// signer's secrets. Shared by every page on it.
struct Node {
    blocks: BTreeMap<Cid, Vec<u8>>,
    /// Blocks only a FETCH reaches (a cold node): a GET of one of these
    /// answers from here and does not populate `blocks`.
    network: Option<BTreeMap<Cid, Vec<u8>>>,
    contracts: BTreeMap<[u8; 32], Cid>,
    register: Option<Vec<u8>>,
    register_id: [u8; 32],
    register_params: Vec<u8>,
    secrets: BTreeMap<Vec<u8>, Vec<u8>>,
    record_fails: bool,
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
        if self.0.record_fails && key == signer::RECORD {
            return false;
        }
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

impl Node {
    fn new() -> Node {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        let mut n = Node {
            blocks: BTreeMap::new(),
            network: None,
            contracts: BTreeMap::new(),
            register: None,
            register_id: signer::register_id(REGISTER_CODE, &params),
            register_params: params.clone(),
            secrets: BTreeMap::new(),
            record_fails: false,
        };
        let req = signer::Request::Provision {
            signing_key: sk.to_bytes().to_vec(),
            register_code: REGISTER_CODE.to_vec(),
            register_params: params,
            block_code: BLOCK_CODE.to_vec(),
        };
        assert_eq!(signer::serve(&mut Host(&mut n), &signer::encode_request(1, &req)), signer::Answer::Provisioned);
        n
    }

    fn put(&mut self, id: Cid, body: &[u8]) {
        self.contracts.insert(engine_delegate::blocks::contract_for(BLOCK_CODE, &id), id);
        self.blocks.insert(id, body.to_vec());
    }

    fn update(&mut self, state: &[u8]) {
        use freenet_stdlib::prelude::*;
        let params = Parameters::from(self.register_params.clone());
        self.register = Some(match &self.register {
            None => state.to_vec(),
            Some(cur) => <craftec_register_contract::Register as ContractInterface>::update_state(
                params,
                State::from(cur.clone()),
                vec![UpdateData::State(State::from(state.to_vec()))],
            )
            .expect("the register merged")
            .new_state
            .expect("a state")
            .as_ref()
            .to_vec(),
        });
    }

    fn head(&self) -> Option<(u64, Cid)> {
        let st = self.register.as_deref()?;
        engine_delegate::register::head_of(st)
    }

    /// Every entry of the tree at `root`, or `None` if a block is missing.
    fn tree(&self, root: &Cid) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
        use freenet_prolly::range::{range, read_value, Range};
        use std::ops::Bound;
        let mut out = BTreeMap::new();
        let mut after = None;
        loop {
            let r = Range { lo: Bound::Unbounded, hi: Bound::Unbounded, reverse: false, after: after.clone(), max_entries: 4096, max_bytes: usize::MAX };
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

    /// The head WHOLE, as page-io reads it off the node: seq and value.
    fn head_read(&self) -> Option<page::HeadRead> {
        page::HeadRead::from_record(self.register.as_deref()?)
    }

    fn sign(&mut self, id: u32, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid) -> (u32, signer_proto::Answer) {
        let req = signer::Request::Sign {
            prev: signer::Head { seq: prev_seq, root: prev_root },
            next: signer::Next { seq, root, ledger: page::sign_ledger(prev_seq, prev_root, root) },
        };
        let served = signer::serve_full(&mut Host(self), &signer::encode_request(id, &req));
        wire::signer::read_answer(&signer::reply(&served)).expect("a signer answer reads back")
    }
}

/// Faults the page's node can play, each a switch like FullNode's.
#[derive(Default, Clone, Copy)]
struct Faults {
    /// Each block's FIRST put lands and its answer is lost; a re-send is
    /// answered. (Every answer lost for ever is a divergence by ruling: the
    /// page confirms a put only by its answer, the Shell read blocks back.)
    lose_first_put_answers: bool,
    /// The first GET of each block goes unanswered.
    lose_first_gets: bool,
    /// The signer fails to save its record on the first sign.
    record_not_saved_once: bool,
    /// Hold every answer (FullNode's `hold_answers`).
    hold: bool,
}

/// `page::Server` over the scripted node, run to a standstill like `Conn`.
struct PageRig {
    server: Server,
    now: u64,
    faults: Faults,
    got_once: std::collections::BTreeSet<Cid>,
    put_once: std::collections::BTreeSet<Cid>,
    held: Vec<Op>,
}

impl PageRig {
    fn new() -> PageRig {
        let page = Page::unstarted(Params::default(), PutPath::Page);
        PageRig {
            server: Server::new(page, SignerFacts { head_writable: true, head_id: [0; 32] }),
            now: 1_000,
            faults: Faults::default(),
            got_once: Default::default(),
            put_once: Default::default(),
            held: Vec::new(),
        }
    }

    fn client_as(&mut self, node: &mut Node, r: &Request) -> Vec<Reply> {
        let frame = protocol::encode_session_request(VERSION, SESSION, r).expect("encodes");
        self.server.client(&frame);
        self.run(node)
    }

    /// Answer every op until none is left; when only silence is left, let the
    /// clock pass a deadline (bounded), as a real page's tick would.
    fn run(&mut self, node: &mut Node) -> Vec<Reply> {
        let mut replies = decode(self.server.take_replies());
        let mut idle = 0;
        for _ in 0..2_000 {
            let ops = self.server.take_ops();
            if ops.is_empty() {
                // Only silence left: let a deadline pass, as the page's own
                // tick would — bounded, so a page that never settles fails.
                if self.faults.hold || !self.server.page.waiting() || idle >= 40 {
                    break;
                }
                idle += 1;
                self.now = self.server.page.next_due().map_or(self.now + 1, |d| d.0.max(self.now + 1));
                self.server.tick(Ms(self.now));
                replies.extend(decode(self.server.take_replies()));
                continue;
            }
            idle = 0;
            for op in ops {
                if self.faults.hold {
                    self.held.push(op);
                    continue;
                }
                if let Some(a) = self.answer(node, op) {
                    self.server.node(a, Ms(self.now));
                }
            }
            replies.extend(decode(self.server.take_replies()));
        }
        replies
    }

    fn answer(&mut self, node: &mut Node, op: Op) -> Option<Answer> {
        Some(match op {
            Op::Put { id, bytes } => {
                node.put(id, &bytes);
                if self.faults.lose_first_put_answers && self.put_once.insert(id) {
                    return None;
                }
                Answer::PutOk(id)
            }
            Op::Get { id } => {
                if self.faults.lose_first_gets && self.got_once.insert(id) {
                    return None;
                }
                if let Some(b) = node.blocks.get(&id) {
                    Answer::Got { id, bytes: b.clone() }
                } else if let Some(b) = node.network.as_ref().and_then(|n| n.get(&id)) {
                    Answer::Got { id, bytes: b.clone() }
                } else {
                    Answer::GetMissed(id)
                }
            }
            Op::Sign { id, prev_seq, prev_root, seq, root } => {
                node.record_fails = std::mem::take(&mut self.faults.record_not_saved_once);
                let (id, answer) = node.sign(id, prev_seq, prev_root, seq, root);
                node.record_fails = false;
                Answer::Signer { id, answer }
            }
            Op::Update { state } => {
                node.update(&state);
                Answer::Updated
            }
            Op::ReadHead => Answer::Head(node.head_read()),
            Op::AskHeld { id } => Answer::Held { id, present: node.blocks.contains_key(&id) },
        })
    }
}

fn decode(frames: Vec<Vec<u8>>) -> Vec<Reply> {
    frames.iter().map(|f| protocol::decode_reply(f).expect("a reply decodes")).collect()
}

/// The Shell side, the same way.
fn old(conn: &mut testkit::full_node::Conn, r: &Request) -> Vec<Reply> {
    decode(conn.client_as(SESSION, VERSION, r))
}

fn write(id: u64, ops: &[(&str, Option<&str>)]) -> Request {
    Request::Write {
        write_id: id,
        ops: ops
            .iter()
            .map(|(k, v)| match v {
                Some(v) => protocol::Op::Put(k.as_bytes().to_vec(), v.as_bytes().to_vec()),
                None => protocol::Op::Delete(k.as_bytes().to_vec()),
            })
            .collect(),
    }
}

fn range(req_id: u64) -> Request {
    Request::Range {
        req_id,
        lo: protocol::Bound::Unbounded,
        hi: protocol::Bound::Unbounded,
        reverse: false,
        after: None,
        max_entries: 1_000,
    }
}

/// The write states a run reported for `id`, in order.
fn states(rs: &[Reply], id: u64) -> Vec<WriteState> {
    rs.iter()
        .filter_map(|r| match r {
            Reply::WriteState { write_id, state } | Reply::SessionWriteState { write_id, state, .. } if *write_id == id => {
                Some(*state)
            }
            _ => None,
        })
        .collect()
}

/// THE HAPPY PATH: identity, writes, a delete, a range, a subscribed range —
/// the replies are IDENTICAL, reply for reply.
#[test]
fn the_same_requests_get_the_same_replies() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let script = [
        Request::Identity,
        Request::SubscribeRange { sub_id: 3, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded },
        write(1, &[("a", Some("1")), ("b", Some("2"))]),
        write(2, &[("c", Some("3"))]),
        write(3, &[("a", None)]),
        range(10),
        Request::Get { req_id: 11, key: b"b".to_vec() },
        Request::Identity,
    ];
    for (n, r) in script.iter().enumerate() {
        let (o, p) = (old(&mut conn, r), rig.client_as(&mut node, r));
        assert_eq!(p, o, "request {n} ({r:?}): the page answered differently from the Shell");
    }
    assert_eq!(node.head().map(|h| h.1), fnode.head().map(|h| h.1), "the two published different trees");
}

/// v5 and above: `Unsupported` on both, naming the same served versions.
#[test]
fn a_v5_frame_is_unsupported_on_both() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let frame = protocol::encode_v5_request(SESSION, 1_000, 1, &Request::Identity).expect("a v5 envelope encodes");
    let o = decode(conn.step(vec![engine_delegate::shell::Inbound::Client(frame.clone())]));
    rig.server.client(&frame);
    let p = rig.run(&mut node);
    assert_eq!(p, o);
    assert!(matches!(p.first(), Some(Reply::Unsupported { got: 5, .. })), "{p:?}");
}

fn tree_of(store: &impl Blocks, root: &Cid) -> BTreeMap<Vec<u8>, Vec<u8>> {
    use freenet_prolly::range::{range, read_value, Range};
    use std::ops::Bound;
    let r = Range { lo: Bound::Unbounded, hi: Bound::Unbounded, reverse: false, after: None, max_entries: 100_000, max_bytes: usize::MAX };
    let page = range(store, root, &r).expect("the tree reads");
    page.entries.iter().map(|(k, v)| (k.clone(), read_value(store, *v).expect("value").to_vec())).collect()
}

fn page_entries(rs: &[Reply], id: u64) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    rs.iter().find_map(|r| match r {
        Reply::Page { req_id, entries, .. } if *req_id == id => Some(entries.clone()),
        _ => None,
    })
}

fn published(st: &[WriteState]) -> bool {
    st.contains(&WriteState::Published)
}

/// A PUT's answer lost, then the node heals. The Shell re-asks its puts after
/// `reask_after` seconds of ticks; the page re-sends each at its deadline.
/// Neither publishes on a lost answer alone (the Shell reads back only after
/// an ack); both publish once answers flow, the same tree.
#[test]
fn a_lost_put_answer_publishes_once_the_node_answers_on_both() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    rig.faults.lose_first_put_answers = true;
    let t0 = 1_800_000_000u64;
    for r in [Request::Identity, Request::Tick { now: t0 }] {
        old(&mut conn, &r);
        rig.client_as(&mut node, &r);
    }
    let w = write(1, &[("a", Some("1")), ("b", Some("2"))]);
    conn.lose_put_acks(true);
    let mut o = old(&mut conn, &w);
    assert!(!published(&states(&o, 1)), "the Shell published with every ack lost");
    conn.lose_put_acks(false);
    for t in 1..=40 {
        o.extend(old(&mut conn, &Request::Tick { now: t0 + t }));
    }
    let p = rig.client_as(&mut node, &w);
    assert!(published(&states(&o, 1)), "Shell: {:?}", states(&o, 1));
    assert!(published(&states(&p, 1)), "page: {:?}", states(&p, 1));
    assert_eq!(node.head().map(|h| h.1), fnode.head().map(|h| h.1));
}

/// A COLD read: the rows are only on the network, and the page's first GET of
/// each block goes unanswered and is fetched again. The same rows as the
/// Shell's cold node answers.
#[test]
fn a_cold_read_with_a_lost_get_returns_the_same_rows() {
    let fnode = FullNode::new();
    let mut w = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    old(&mut w, &Request::Identity);
    rig.client_as(&mut node, &Request::Identity);
    let rows: Vec<(String, String)> = (0..60).map(|i| (format!("k/{i:03}"), format!("v{i}"))).collect();
    let ops: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
    let wr = write(1, &ops);
    assert!(published(&states(&old(&mut w, &wr), 1)));
    assert!(published(&states(&rig.client_as(&mut node, &wr), 1)));
    // The readers: nothing local.
    let cold = FullNode::cold_over(&fnode);
    let mut rc = cold.connect();
    let mut rnode = Node::new();
    rnode.network = Some(std::mem::take(&mut node.blocks));
    rnode.register = node.register.clone();
    let mut rr = PageRig::new();
    rr.faults.lose_first_gets = true;
    old(&mut rc, &Request::Identity);
    rr.client_as(&mut rnode, &Request::Identity);
    let (o, p) = (old(&mut rc, &range(20)), rr.client_as(&mut rnode, &range(20)));
    let (oe, pe) = (page_entries(&o, 20), page_entries(&p, 20));
    assert!(oe.as_ref().is_some_and(|e| e.len() == 60), "Shell: {o:?}");
    assert_eq!(pe, oe, "the page's cold read returned different rows");
}

/// One commit at a time: a write while a commit is in flight is `Busy` on both.
#[test]
fn a_write_behind_a_commit_in_flight_is_busy_on_both() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    old(&mut conn, &Request::Identity);
    rig.client_as(&mut node, &Request::Identity);
    conn.hold_answers();
    rig.faults.hold = true;
    let (o1, p1) = (old(&mut conn, &write(1, &[("a", Some("1"))])), rig.client_as(&mut node, &write(1, &[("a", Some("1"))])));
    assert_eq!(states(&p1, 1), states(&o1, 1));
    let (o2, p2) = (old(&mut conn, &write(2, &[("b", Some("2"))])), rig.client_as(&mut node, &write(2, &[("b", Some("2"))])));
    assert_eq!(states(&o2, 2), vec![WriteState::Busy], "Shell");
    assert_eq!(states(&p2, 2), states(&o2, 2), "page");
}

/// A commit that cannot finish is told `Stalled` once `max_accept_age`
/// SECONDS have passed — on both, from the client's protocol ticks.
#[test]
fn a_held_commit_is_stalled_after_its_age_on_both() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let t0 = 1_800_000_000u64;
    for r in [Request::Identity, Request::Tick { now: t0 }] {
        old(&mut conn, &r);
        rig.client_as(&mut node, &r);
    }
    conn.hold_answers();
    rig.faults.hold = true;
    let w = write(1, &[("a", Some("1"))]);
    old(&mut conn, &w);
    rig.client_as(&mut node, &w);
    let early = Request::Tick { now: t0 + 30 };
    let (o, p) = (old(&mut conn, &early), rig.client_as(&mut node, &early));
    assert!(!states(&o, 1).contains(&WriteState::Stalled) && !states(&p, 1).contains(&WriteState::Stalled), "stalled early");
    let late = Request::Tick { now: t0 + 65 };
    let (o, p) = (old(&mut conn, &late), rig.client_as(&mut node, &late));
    assert_eq!(states(&o, 1), vec![WriteState::Stalled], "Shell");
    assert_eq!(states(&p, 1), states(&o, 1), "page");
}

/// Two tabs on ONE key, the second stale: its write is `Lost` (the page: the
/// signer's NotNext; the Shell: the head read back under another root), and
/// re-sent it publishes on the winner. The same outcomes, the same tree.
#[test]
fn two_tabs_on_one_key_the_stale_one_loses_then_publishes_on_both() {
    let fnode = FullNode::new();
    let (mut a, mut b) = (fnode.connect(), fnode.connect());
    let mut node = Node::new();
    let (mut pa, mut pb) = (PageRig::new(), PageRig::new());
    for c in [&mut a, &mut b] {
        old(c, &Request::Identity);
    }
    pa.client_as(&mut node, &Request::Identity);
    pb.client_as(&mut node, &Request::Identity);
    let w1 = write(1, &[("a", Some("1"))]);
    assert!(published(&states(&old(&mut a, &w1), 1)));
    assert!(published(&states(&pa.client_as(&mut node, &w1), 1)));
    let w2 = write(2, &[("b", Some("2"))]);
    let (o, p) = (old(&mut b, &w2), pb.client_as(&mut node, &w2));
    assert!(states(&o, 2).contains(&WriteState::Lost), "Shell: {:?}", states(&o, 2));
    assert!(states(&p, 2).contains(&WriteState::Lost), "page: {:?}", states(&p, 2));
    let w3 = write(3, &[("b", Some("2"))]);
    let (o, p) = (old(&mut b, &w3), pb.client_as(&mut node, &w3));
    assert!(published(&states(&o, 3)) && published(&states(&p, 3)), "Shell {:?} page {:?}", states(&o, 3), states(&p, 3));
    assert_eq!(node.head().map(|h| h.1), fnode.head().map(|h| h.1), "different trees after the race");
}

/// The signer fails to save its record once (page only — the Shell signed
/// with no record): asked again after its backoff, and the write publishes
/// with the same states as the Shell's.
#[test]
fn record_not_saved_once_publishes_like_the_shell() {
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    let mut node = Node::new();
    let mut rig = PageRig::new();
    rig.faults.record_not_saved_once = true;
    old(&mut conn, &Request::Identity);
    rig.client_as(&mut node, &Request::Identity);
    let w = write(1, &[("a", Some("1"))]);
    let (o, p) = (old(&mut conn, &w), rig.client_as(&mut node, &w));
    assert_eq!(states(&p, 1), states(&o, 1));
    assert!(published(&states(&p, 1)));
}

/// TWO ROOTS AT ONE SEQ for this key — another device of the same identity,
/// whose head won the Register's tie-break. The same identity never forks
/// (owner, sdk#225): the page ADOPTS the winner, the write built on the
/// displaced head is handed back `Lost`, sent again it publishes on the
/// winner, and nothing is unusable. The Shell had no check at all and
/// publishes over it blind — a divergence by design, asserted here.
#[test]
fn a_same_key_displacement_is_adopted_on_the_page_and_unseen_by_the_shell() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    rig.client_as(&mut node, &Request::Identity);
    assert!(published(&states(&rig.client_as(&mut node, &write(1, &[("a", Some("1"))])), 1)));
    let (seq, mine) = node.head().expect("published");
    let key = node.secrets.get(signer::KEY).cloned().expect("provisioned");
    // The other device's REAL tree, whose root WINS the equal-seq rule (the
    // lower BLAKE3 of the value): the fork would otherwise be invisible to
    // the signer's read, and a fake root would leave nothing to build on.
    let beats = |a: &Cid, b: &Cid| blake3::hash(a).as_bytes() < blake3::hash(b).as_bytes();
    let winner = (0u32..512).map(|salt| sibling_root(&mut node, salt)).find(|r| beats(r, &mine)).expect("some tree wins");
    let other = engine_delegate::register::head_state(&node.register_params, &key, seq, &winner).expect("signs");
    node.update(&other);
    assert_eq!(node.head(), Some((seq, winner)), "the winner did not take the register");
    let second = states(&rig.client_as(&mut node, &write(2, &[("b", Some("2"))])), 2);
    assert!(second.contains(&WriteState::Lost), "a write built on the displaced head was not handed back: {second:?}");
    assert_eq!(rig.server.page.published(), (seq, winner), "the winner was not adopted");
    let again = states(&rig.client_as(&mut node, &write(2, &[("b", Some("2"))])), 2);
    assert!(published(&again), "sent again, the write did not publish on the winner: {again:?}");
    assert!(rig.server.page.unusable().is_empty(), "a same-key displacement left the page unusable: {:?}", rig.server.page.unusable());
    // The Shell, the same story: nothing in it can see the fork.
    let fnode = FullNode::new();
    let mut conn = fnode.connect();
    old(&mut conn, &Request::Identity);
    old(&mut conn, &write(1, &[("a", Some("1"))]));
    let (s, _) = fnode.head().expect("published");
    fnode.set_head(s, [9u8; 32]);
    let o = old(&mut conn, &write(2, &[("b", Some("2"))]));
    assert!(!states(&o, 2).is_empty(), "the Shell said nothing: {o:?}");
}

/// A RESTART MID-COMMIT: the first page's UPDATE never lands and the page is
/// gone. The Shell's next engine LOSES that write; the page's next engine is
/// answered AlreadySigned and LANDS it — the signer's record is the third
/// memory (ENGINE-SHAPE §5). A divergence by design, asserted.
#[test]
fn a_restart_mid_commit_the_signer_lands_what_the_shell_lost() {
    let mut node = Node::new();
    let mut first = PageRig::new();
    first.client_as(&mut node, &Request::Identity);
    let frame = protocol::encode_session_request(VERSION, SESSION, &write(1, &[("gone", Some("1"))])).expect("encodes");
    first.server.client(&frame);
    // Answer everything except the UPDATE: the head never lands.
    for _ in 0..50 {
        let ops = first.server.take_ops();
        if ops.is_empty() {
            break;
        }
        for op in ops {
            if matches!(op, Op::Update { .. }) {
                continue;
            }
            if let Some(a) = first.answer(&mut node, op) {
                first.server.node(a, Ms(first.now));
            }
        }
    }
    assert!(node.head().is_none(), "the head landed; this is not a restart mid-commit");
    drop(first);
    let mut next = PageRig::new();
    next.client_as(&mut node, &Request::Identity);
    let w2 = write(2, &[("new", Some("2"))]);
    let p = next.client_as(&mut node, &w2);
    let mut rs = p.clone();
    if !published(&states(&p, 2)) {
        rs = next.client_as(&mut node, &write(3, &[("new", Some("2"))]));
    }
    let (_, root) = node.head().expect("published");
    let tree = tree_of(&node, &root);
    assert_eq!(tree.get(&b"gone"[..]).map(Vec::as_slice), Some(&b"1"[..]), "the signer's record did not land the first commit");
    assert_eq!(tree.get(&b"new"[..]).map(Vec::as_slice), Some(&b"2"[..]), "{rs:?}");

    // The Shell: the same restart, and the first write is gone.
    let fnode = FullNode::new();
    let mut c1 = fnode.connect();
    old(&mut c1, &Request::Identity);
    c1.hold_answers();
    old(&mut c1, &write(1, &[("gone", Some("1"))]));
    drop(c1);
    let mut c2 = fnode.connect();
    old(&mut c2, &Request::Identity);
    assert!(published(&states(&old(&mut c2, &write(2, &[("new", Some("2"))])), 2)));
    let (_, root) = fnode.head().expect("published");
    let tree = tree_of(&fnode.store(), &root);
    assert!(!tree.contains_key(&b"gone"[..]), "the Shell kept a write whose head never landed?");
}

/// sdk#175's twin: a READ that arrives before the page has read its head is
/// HELD, not answered from the empty tree the engine started on — it is
/// answered, with the rows, once the head is read.
#[test]
fn a_read_before_the_head_is_read_waits_for_it() {
    let mut node = Node::new();
    let mut writer = PageRig::new();
    writer.client_as(&mut node, &Request::Identity);
    assert!(published(&states(&writer.client_as(&mut node, &write(1, &[("a", Some("1")), ("b", Some("2"))])), 1)));
    // A second page: its Identity starts the engine and sends the head read,
    // and the Range goes in BEFORE any answer.
    let mut reader = PageRig::new();
    for r in [Request::Identity, range(42)] {
        let frame = protocol::encode_session_request(VERSION, SESSION, &r).expect("encodes");
        reader.server.client(&frame);
    }
    let early = decode(reader.server.take_replies());
    assert!(page_entries(&early, 42).is_none(), "a read was answered before the head was read: {early:?}");
    let p = reader.run(&mut node);
    assert_eq!(page_entries(&p, 42).map(|e| e.len()), Some(2), "the held read did not answer with the rows: {p:?}");
}

/// ANOTHER DEVICE'S TREE, for real: a fresh page from the same genesis writes
/// `salt`, and its blocks go onto the node; returns its root (never signed —
/// the caller signs a head for it as the other device's signer would).
fn sibling_root(node: &mut Node, salt: u32) -> Cid {
    use engine::{ClientId, Op as WriteOp, WriteId};
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.write(ClientId(9), WriteId(1), vec![(format!("sib/{salt}").into_bytes(), WriteOp::Put(vec![salt as u8; 64]))]);
    for _ in 0..50 {
        for op in p.take_ops() {
            match op {
                Op::ReadHead => p.answer(Answer::Head(None), Ms(1)),
                Op::Put { id, bytes } => {
                    node.put(id, &bytes);
                    p.answer(Answer::PutOk(id), Ms(1));
                }
                Op::AskHeld { id } => p.answer(Answer::Held { id, present: true }, Ms(1)),
                Op::Sign { root, .. } => return root,
                _ => {}
            }
        }
    }
    panic!("the sibling page never asked to sign");
}

/// A TAB: `Db` over `CachedStore`, its frames served by this rig's
/// `page::Server` over the scripted node, its replies fed back — the whole page
/// path, native.
struct Tab {
    db: craftworks_sdk::Db<craftworks_sdk::CachedStore, TabEnv>,
    /// Per write id, how many times it went on the wire (a `Commit` or a `Write`).
    sends: BTreeMap<u64, usize>,
    /// Per write id, the states it was told, in order.
    verdicts: BTreeMap<u64, Vec<protocol::WriteState>>,
}

struct TabEnv(u32);
impl craftworks_sdk::Env for TabEnv {
    fn now_ms(&mut self) -> u64 {
        1_750_000_000_000
    }
    fn rand32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        self.0
    }
}

impl Tab {
    fn open(rig: &mut PageRig, node: &mut Node) -> Tab {
        let (store, _clock) = testkit::cached_store();
        let mut t = Tab { db: craftworks_sdk::Db::new(store, TabEnv(1), [1, 2, 3, 4]), sends: BTreeMap::new(), verdicts: BTreeMap::new() };
        t.db.store_mut().client.send(&Request::Identity);
        t.pump(rig, node);
        t.db.store_mut().request_range(1, b"", &[0xFF; 64], 256);
        t.pump(rig, node);
        t
    }

    fn pump(&mut self, rig: &mut PageRig, node: &mut Node) {
        for _ in 0..400 {
            let frames = self.db.store_mut().take_outbound();
            let mut moved = !frames.is_empty();
            for f in &frames {
                if let protocol::Incoming::Ok(env) = protocol::decode_request(f) {
                    if let Request::Write { write_id, .. } | Request::Commit { write_id, .. } = env.body {
                        *self.sends.entry(write_id).or_default() += 1;
                    }
                }
                rig.server.client(f);
            }
            for op in rig.server.take_ops() {
                moved = true;
                if let Some(a) = rig.answer(node, op) {
                    rig.server.node(a, Ms(rig.now));
                }
            }
            for r in rig.server.take_replies() {
                moved = true;
                // What the HOST does with a page: the copy records a range as
                // loaded only when told the interval it asked for.
                let decoded = protocol::decode_reply(&r);
                if let Some((w, st)) = decoded.as_ref().ok().and_then(|x| self.db.store_mut().client.own_write_state(x)) {
                    self.verdicts.entry(w).or_default().push(st);
                }
                if let Ok(Reply::Page { req_id: 1, entries, cursor: None, at, .. }) = decoded {
                    self.db.store_mut().on_page(b"", &[0xFF; 64], entries, at.root);
                }
                self.db.store_mut().on_inbound(&r);
            }
            if !moved {
                if !rig.server.page.waiting() {
                    return;
                }
                rig.now = rig.server.page.next_due().map_or(rig.now + 1, |d| d.0.max(rig.now + 1));
                rig.server.tick(Ms(rig.now));
            }
        }
    }
}

fn tab_schema() -> craftworks_sdk::Schema {
    serde_json::from_value(serde_json::json!({ "type": "T", "fields": [{ "name": "title", "kind": "text", "required": true }] })).expect("a schema")
}

/// M2 on the PAGE path (sdk#148's SDK half): a tab's `update` over a record
/// another session has since changed is a stale `Commit`. The engine applies
/// NOTHING; the tab is told `Conflict` with `Conflicted` naming the key (a v4
/// client of this same build decodes both); the write is rolled back, the key
/// FORGOTTEN in the copy (no row shows a value the tree lacks), the conflict
/// surfaced to the app, and the write NEVER re-sent. The other session's
/// value stands. Control: the same update with nothing in between publishes,
/// and nothing is conflicted.
#[test]
fn a_stale_update_is_told_conflict_rolled_back_forgotten_and_never_re_sent() {
    for interfere in [false, true] {
        let mut node = Node::new();
        let mut rig = PageRig::new();
        let mut tab = Tab::open(&mut rig, &mut node);
        tab.db.define("t", &tab_schema()).expect("define");
        tab.pump(&mut rig, &mut node);
        let rec = tab.db.put("t", &serde_json::json!({ "title": "first" }).as_object().unwrap().clone()).expect("put");
        tab.pump(&mut rig, &mut node);
        let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("a record's id parses");
        let key = craftworks_sdk::db::record_key("t", loc);
        if interfere {
            // ANOTHER session writes the record's key, blind, behind this tab's copy.
            let other = protocol::encode_session_request(VERSION, SESSION + 1, &Request::Write { write_id: 1, ops: vec![protocol::Op::Put(key.clone(), b"elsewhere".to_vec())] }).expect("encodes");
            rig.server.client(&other);
            for _ in 0..200 {
                let ops = rig.server.take_ops();
                if ops.is_empty() && !rig.server.page.waiting() {
                    break;
                }
                for op in ops {
                    if let Some(a) = rig.answer(&mut node, op) {
                        rig.server.node(a, Ms(rig.now));
                    }
                }
                rig.now += 50;
                rig.server.tick(Ms(rig.now));
            }
            let _ = rig.server.take_replies();
        }
        let w = tab.db.store_mut().next_write_id();
        tab.db.update("t", loc, serde_json::json!({ "title": "second" }).as_object().unwrap()).expect("the update is made");
        tab.pump(&mut rig, &mut node);
        for _ in 0..5 {
            rig.now += 30_000;
            rig.server.tick(Ms(rig.now));
            tab.db.store_mut().tick();
            tab.pump(&mut rig, &mut node);
        }
        let conflicts = tab.db.store_mut().take_conflicts();
        assert_eq!(tab.sends.get(&w), Some(&1), "interfere {interfere}: the write went on the wire {:?} times", tab.sends.get(&w));
        if interfere {
            assert_eq!(tab.verdicts.get(&w).and_then(|v| v.last()), Some(&protocol::WriteState::Conflict), "not told Conflict: {:?}", tab.verdicts.get(&w));
            assert_eq!(conflicts.len(), 1, "a stale update was not told as ONE conflict: {conflicts:?}");
            assert_eq!((conflicts[0].write_id, &conflicts[0].key), (w, &key));
            assert_eq!(craftworks_sdk::store::Reads::row_state(tab.db.store_mut(), &key), craftworks_sdk::store::RowState::RolledBack);
            assert!(matches!(craftworks_sdk::store::Reads::get(tab.db.store_mut(), &key), Err(craftworks_sdk::store::StoreError::NotLoaded)), "the conflicted key was not forgotten: the copy still shows a value");
            let (_, root) = node.head().expect("a head");
            let _ = root;
        } else {
            assert!(conflicts.is_empty(), "nothing moved and yet a conflict: {conflicts:?}");
            assert_eq!(craftworks_sdk::store::Reads::row_state(tab.db.store_mut(), &key), craftworks_sdk::store::RowState::Clean, "the unconflicted update did not publish");
        }
    }
}

/// THE SCHEMA KEY (the architect's #1 on the slice): another session changes
/// the domain's SCHEMA behind this tab's copy. The tab's update reads the
/// record (unchanged) and the schema (stale): it conflicts ON THE SCHEMA KEY,
/// which no rolled-back write names — and that key is FORGOTTEN too, so the
/// next write reads the schema fresh instead of conflicting again for ever.
#[test]
fn a_schema_changed_elsewhere_conflicts_on_the_schema_key_and_forgets_it() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let mut tab = Tab::open(&mut rig, &mut node);
    tab.db.define("t", &tab_schema()).expect("define");
    tab.pump(&mut rig, &mut node);
    let rec = tab.db.put("t", &serde_json::json!({ "title": "first" }).as_object().unwrap().clone()).expect("put");
    tab.pump(&mut rig, &mut node);
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    // The schema key, as `Db` names it.
    let mut schema_key = vec![0u8];
    schema_key.extend_from_slice(b"schema\0t");
    let wider: craftworks_sdk::Schema = serde_json::from_value(serde_json::json!({ "type": "T", "fields": [
        { "name": "title", "kind": "text", "required": true },
        { "name": "note", "kind": "text" }
    ] })).expect("a schema");
    let other = protocol::encode_session_request(VERSION, SESSION + 1, &Request::Write {
        write_id: 1,
        ops: vec![protocol::Op::Put(schema_key.clone(), serde_json::to_vec(&wider).unwrap())],
    })
    .expect("encodes");
    rig.server.client(&other);
    for _ in 0..200 {
        let ops = rig.server.take_ops();
        if ops.is_empty() && !rig.server.page.waiting() {
            break;
        }
        for op in ops {
            if let Some(a) = rig.answer(&mut node, op) {
                rig.server.node(a, Ms(rig.now));
            }
        }
        rig.now += 50;
        rig.server.tick(Ms(rig.now));
    }
    let _ = rig.server.take_replies();
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, serde_json::json!({ "title": "second" }).as_object().unwrap()).expect("the update is made");
    tab.pump(&mut rig, &mut node);
    let conflicts = tab.db.store_mut().take_conflicts();
    assert_eq!(conflicts.iter().map(|c| (c.write_id, c.key.clone())).collect::<Vec<_>>(), vec![(w, schema_key.clone())], "not a conflict on the SCHEMA key");
    assert!(
        matches!(craftworks_sdk::store::Reads::get(tab.db.store_mut(), &schema_key), Err(craftworks_sdk::store::StoreError::NotLoaded)),
        "the stale schema was not forgotten: the next write would conflict on it again"
    );
    // And the fallen write's OWN key (the record: not the key that conflicted).
    let key = craftworks_sdk::db::record_key("t", loc);
    assert!(
        matches!(craftworks_sdk::store::Reads::get(tab.db.store_mut(), &key), Err(craftworks_sdk::store::StoreError::NotLoaded)),
        "the rolled-back record still shows its unsaved value"
    );
}

/// ANOTHER DEVICE'S TREE holding exactly `entries`, built by a fresh page from
/// the genesis; its blocks go onto the node. Returns its root.
fn device_tree(node: &mut Node, entries: &[(Vec<u8>, Vec<u8>)]) -> Cid {
    use engine::{ClientId, Op as WriteOp, WriteId};
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.write(ClientId(9), WriteId(1), entries.iter().map(|(k, v)| (k.clone(), WriteOp::Put(v.clone()))).collect());
    for _ in 0..50 {
        for op in p.take_ops() {
            match op {
                Op::ReadHead => p.answer(Answer::Head(None), Ms(1)),
                Op::Put { id, bytes } => {
                    node.put(id, &bytes);
                    p.answer(Answer::PutOk(id), Ms(1));
                }
                Op::AskHeld { id } => p.answer(Answer::Held { id, present: true }, Ms(1)),
                Op::Sign { root, .. } => return root,
                _ => {}
            }
        }
    }
    panic!("the other device's page never asked to sign");
}

/// sdk#225b through the page path: a tab publishes a record; ANOTHER DEVICE of
/// the same identity lands a head on the register (signed with the same key, as
/// two devices are until #226); the tab reads it on the node's hint and adopts
/// it (#225a). What it is TOLD depends on the winner's `prev` and contents:
/// * a same-seq winner that lacks the record → `Superseded` naming the record's
///   key, and the tab's copy forgets it (NotLoaded: the next read fetches the
///   winner's value);
/// * a winner whose prev IS the tab's tip (they built on it) → nothing;
/// * a winner two commits deep whose tree holds exactly the tab's value → nothing
///   (the architect's false-Superseded case: probed, equal, kept).
#[test]
fn a_displaced_tip_is_told_superseded_only_for_the_keys_the_winner_replaced() {
    use signer_proto::head::{value, Ledger};
    for case in ["same-seq, record lost", "built on the tip", "two deep, record kept"] {
        let mut node = Node::new();
        let mut rig = PageRig::new();
        let mut tab = Tab::open(&mut rig, &mut node);
        tab.db.define("t", &tab_schema()).expect("define");
        tab.pump(&mut rig, &mut node);
        let rec = tab.db.put("t", &serde_json::json!({ "title": "mine" }).as_object().unwrap().clone()).expect("put");
        tab.pump(&mut rig, &mut node);
        let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
        let key = craftworks_sdk::db::record_key("t", loc);
        let (tip_seq, tip_root) = node.head().expect("the tab published");
        let tip_tree = node.tree(&tip_root).expect("the tip is whole");
        let key_of = node.secrets.get(signer::KEY).cloned().expect("provisioned");
        let (seq, root, prev) = match case {
            "same-seq, record lost" => {
                // The other device's commit at the tip's seq, built on the
                // tip's prev, WITHOUT the record — and winning the tie-break.
                let hr = node.head_read().expect("a head");
                let base = hr.prev().expect("the tip has a prev");
                let mut salt = 0u8;
                loop {
                    let r = device_tree(&mut node, &[(b"other".to_vec(), vec![salt])]);
                    let v = value(&r, &Ledger { prev: Some(signer_proto::Head { seq: base.0, root: base.1 }), ..Ledger::default() });
                    if page::beats(&v, hr.value()) {
                        break (tip_seq, r, Some(base));
                    }
                    salt += 1;
                }
            }
            "built on the tip" => (tip_seq + 1, device_tree(&mut node, &[(b"other".to_vec(), b"x".to_vec())]), Some((tip_seq, tip_root))),
            _ => {
                // Two deep: a head at tip+2 whose prev is NOT the tip, but whose
                // tree holds the tab's record exactly (they built on it twice).
                let mut entries: Vec<(Vec<u8>, Vec<u8>)> = tip_tree.into_iter().collect();
                entries.push((b"other".to_vec(), b"y".to_vec()));
                (tip_seq + 2, device_tree(&mut node, &entries), Some((tip_seq + 1, [0x42; 32])))
            }
        };
        let v = value(&root, &Ledger { prev: prev.map(|(s, r)| signer_proto::Head { seq: s, root: r }), ..Ledger::default() });
        let st = engine_delegate::register::head_state(&node.register_params, &key_of, seq, &v).expect("signs");
        node.update(&st);
        assert_eq!(node.head().map(|h| h.0), Some(seq), "{case}: the other device's head did not take the register");
        rig.server.head_hint();
        tab.pump(&mut rig, &mut node);
        for _ in 0..10 {
            rig.now += 1_000;
            rig.server.tick(Ms(rig.now));
            tab.pump(&mut rig, &mut node);
        }
        assert_eq!(rig.server.page.published().0, seq, "{case}: the tab did not adopt the other device's head");
        let told = tab.db.store_mut().take_superseded();
        match case {
            "same-seq, record lost" => {
                assert_eq!(told.len(), 1, "{case}: {told:?}");
                assert_eq!(told[0].keys, vec![key.clone()], "{case}: the superseded rows are not the record");
                assert!(
                    matches!(craftworks_sdk::store::Reads::get(tab.db.store_mut(), &key), Err(craftworks_sdk::store::StoreError::NotLoaded)),
                    "{case}: the replaced row still shows the tab's value"
                );
            }
            _ => assert!(told.is_empty(), "{case}: told Superseded for a value the winner holds: {told:?}"),
        }
    }
}

/// A TIP OF TWO WRITES (main's condition on Z5): a commit carries ONE write —
/// the engine refuses a second with `Busy` — so a tip holds more than one only
/// through a NO-OP write Published at the same head (sdk#160), whose value at
/// every key is the tree's there. Here the second write puts the SAME bytes at
/// the record's key; it publishes at the same head, joining the tip. A
/// same-seq winner without the record then tells BOTH writes Superseded on
/// that key — each write its own reply, the same row.
#[test]
fn a_tip_of_two_writes_to_one_key_tells_both_superseded() {
    use signer_proto::head::{value, Ledger};
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let mut tab = Tab::open(&mut rig, &mut node);
    tab.db.define("t", &tab_schema()).expect("define");
    tab.pump(&mut rig, &mut node);
    let rec = tab.db.put("t", &serde_json::json!({ "title": "mine" }).as_object().unwrap().clone()).expect("put");
    let w1 = tab.db.store_mut().next_write_id() - 1;
    tab.pump(&mut rig, &mut node);
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    let key = craftworks_sdk::db::record_key("t", loc);
    let head = node.head().expect("published");
    let bytes = node.tree(&head.1).expect("whole").get(&key).cloned().expect("the record is in the tip");
    // The same bytes again: a no-op, Published at the same head.
    let w2 = tab.db.store_mut().next_write_id();
    craftworks_sdk::store::Store::apply_commit(
        tab.db.store_mut(),
        &[(key.clone(), protocol::Expect::Value(craftworks_sdk::read_token::read_token(&bytes)))],
        &[(key.clone(), craftworks_sdk::store::Edit::Put(bytes.clone()))],
    )
    .expect("the store took it");
    tab.pump(&mut rig, &mut node);
    assert!(tab.verdicts.get(&w2).is_some_and(|v| v.contains(&protocol::WriteState::Published)), "the second write did not publish: {:?}", tab.verdicts.get(&w2));
    assert_eq!(node.head(), Some(head), "the second write moved the head: it was not a no-op, the tip is one write");
    // Displaced at its own seq by a winner without the record.
    let hr = node.head_read().expect("a head");
    let base = hr.prev().expect("a prev");
    let key_of = node.secrets.get(signer::KEY).cloned().expect("provisioned");
    let mut salt = 0u8;
    let st = loop {
        let r = device_tree(&mut node, &[(b"other".to_vec(), vec![salt])]);
        let v = value(&r, &Ledger { prev: Some(signer_proto::Head { seq: base.0, root: base.1 }), ..Ledger::default() });
        if page::beats(&v, hr.value()) {
            break engine_delegate::register::head_state(&node.register_params, &key_of, head.0, &v).expect("signs");
        }
        salt += 1;
    };
    node.update(&st);
    rig.server.head_hint();
    tab.pump(&mut rig, &mut node);
    let mut told: Vec<(u64, Vec<Vec<u8>>)> = tab.db.store_mut().take_superseded().into_iter().map(|s| (s.write_id, s.keys)).collect();
    told.sort();
    assert_eq!(told, vec![(w1, vec![key.clone()]), (w2, vec![key.clone()])], "each write of the tip was not told its row");
}

/// sdk#225b part 2, THE MERGE (cell B): another device's commit at the tip's
/// seq, signed from the SAME base P, whose tree is exactly P plus THEIR
/// changes. Per key: changed only here (and its write's reads unchanged there)
/// → KEPT, re-applied on the winner as a Commit with reads; changed there too
/// → SUPERSEDED, told.
/// * c1 they added an unrelated key → nothing told; the merge publishes on top
///   of the winner, and the final tree holds BOTH their key and this page's
///   record;
/// * c2 they changed the record itself → the record is Superseded, and their
///   value stands.
#[test]
fn a_same_seq_race_is_merged_key_by_key() {
    use signer_proto::head::{value, Ledger};
    for case in ["one-sided mine is kept", "both changed the record", "both changed the record, behind a long delta"] {
        let mut node = Node::new();
        let mut rig = PageRig::new();
        let mut tab = Tab::open(&mut rig, &mut node);
        tab.db.define("t", &tab_schema()).expect("define");
        tab.pump(&mut rig, &mut node);
        let rec = tab.db.put("t", &serde_json::json!({ "title": "mine" }).as_object().unwrap().clone()).expect("put");
        tab.pump(&mut rig, &mut node);
        let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
        let key = craftworks_sdk::db::record_key("t", loc);
        // The long-delta case's tip is ONE write of TWO keys: the record again
        // (which the winner changes, behind the cut) and a key only this page
        // writes. Main's point: a key wrongly kept by a cut delta sinks the
        // whole merge write, so the truly one-sided key must end PUBLISHED —
        // an outcome, not only a message.
        let extra = vec![0x02, b'x'];
        if case.ends_with("behind a long delta") {
            let (_, r) = node.head().expect("published");
            let now_rec = node.tree(&r).expect("whole").get(&key).cloned().expect("the record");
            // A write that declared NO reads (a store-level batch): the
            // stale-premise rule cannot cover it, so only the COMPLETE delta
            // decides which of its keys the winner changed — and a cut read as
            // complete would keep the record and write this page's value over
            // the winner's, the merge write carrying no read to refuse it.
            let _ = now_rec;
            craftworks_sdk::store::Store::apply_batch(
                tab.db.store_mut(),
                &[
                    (key.clone(), craftworks_sdk::store::Edit::Put(b"mine, again".to_vec())),
                    (extra.clone(), craftworks_sdk::store::Edit::Put(b"only mine".to_vec())),
                ],
            )
            .expect("the store took it");
            tab.pump(&mut rig, &mut node);
        }
        let (tip_seq, tip_root) = node.head().expect("published");
        let mine = node.tree(&tip_root).expect("whole").get(&key).cloned().expect("the record");
        let hr = node.head_read().expect("a head");
        let base = hr.prev().expect("the tip has a prev");
        let base_tree = node.tree(&base.1).expect("the base is whole");
        let key_of = node.secrets.get(signer::KEY).cloned().expect("provisioned");
        let theirs_record = b"their bytes for the record".to_vec();
        let mut salt = 0u8;
        let st = loop {
            let mut entries: Vec<(Vec<u8>, Vec<u8>)> = base_tree.clone().into_iter().collect();
            entries.push((b"other".to_vec(), vec![salt]));
            if case.starts_with("both changed the record") {
                entries.push((key.clone(), theirs_record.clone()));
            }
            // 300 changes of theirs that sort BEFORE the record: longer than
            // one delta page, so the record's change comes after a CUT — a cut
            // delta is unknown, never "not changed there" (the architect's #3).
            if case.ends_with("behind a long delta") {
                for i in 0..300u16 {
                    let mut k = vec![0x00, b'f'];
                    k.extend_from_slice(&i.to_be_bytes());
                    entries.push((k, vec![1]));
                }
            }
            let r = device_tree(&mut node, &entries);
            let v = value(&r, &Ledger { prev: Some(signer_proto::Head { seq: base.0, root: base.1 }), ..Ledger::default() });
            if page::beats(&v, hr.value()) {
                break engine_delegate::register::head_state(&node.register_params, &key_of, tip_seq, &v).expect("signs");
            }
            salt += 1;
        };
        node.update(&st);
        rig.server.head_hint();
        tab.pump(&mut rig, &mut node);
        for _ in 0..20 {
            rig.now += 1_000;
            rig.server.tick(Ms(rig.now));
            tab.pump(&mut rig, &mut node);
        }
        let told = tab.db.store_mut().take_superseded();
        let (fseq, froot) = node.head().expect("a head");
        let fin = node.tree(&froot).expect("the final tree is whole");
        match case {
            "one-sided mine is kept" => {
                assert!(told.is_empty(), "{case}: told Superseded for a key only this page changed: {told:?}");
                assert_eq!(fseq, tip_seq + 1, "{case}: no merge commit on top of the winner");
                assert_eq!(fin.get(&key), Some(&mine), "{case}: this page's record did not survive the merge");
                assert!(fin.contains_key(b"other".as_slice()), "{case}: their change did not survive the merge");
            }
            _ => {
                assert_eq!(told.iter().map(|s| s.keys.clone()).collect::<Vec<_>>(), vec![vec![key.clone()]], "{case}: {told:?}");
                assert_eq!(fin.get(&key), Some(&theirs_record), "{case}: their value at the record did not stand");
                if case.ends_with("behind a long delta") {
                    assert_eq!(fin.get(&extra).map(|v| v.as_slice()), Some(&b"only mine"[..]), "{case}: the truly one-sided key was lost with the record: the merge write fell WHOLE");
                    assert_eq!(fseq, tip_seq + 1, "{case}: no merge commit on top of the winner");
                }
            }
        }
    }
}

/// Drive the rig, answering every op EXCEPT block GETs, which are held (a cold
/// node). Returns when nothing else moves.
fn run_holding_gets(tab: &mut Tab, rig: &mut PageRig, node: &mut Node, held: &mut Vec<Op>) {
    for _ in 0..400 {
        let frames = tab.db.store_mut().take_outbound();
        let mut moved = !frames.is_empty();
        for f in &frames {
            rig.server.client(f);
        }
        for op in rig.server.take_ops() {
            moved = true;
            if matches!(op, Op::Get { .. }) {
                held.push(op);
                continue;
            }
            if let Some(a) = rig.answer(node, op) {
                rig.server.node(a, Ms(rig.now));
            }
        }
        for r in rig.server.take_replies() {
            moved = true;
            tab.db.store_mut().on_inbound(&r);
        }
        if !moved {
            return;
        }
    }
}

/// THE MERGE WRITE IS A COMMIT WITH READS, NEVER BLIND (the architect's #2 on
/// the table): a merge decided against the winner x can LAND on a newer head,
/// when the engine was busy and the page adopted another device's x' in
/// between. Its reads — the kept writes' own, true at x — are compared where
/// it lands: x' changed the record, so the merge conflicts and the record is
/// SUPERSEDED, told; their x' value stands. A blind merge would have written
/// this page's value over it.
#[test]
fn a_merge_that_lands_on_a_newer_head_is_judged_by_its_reads_there() {
    use signer_proto::head::{value, Ledger};
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let mut tab = Tab::open(&mut rig, &mut node);
    tab.db.define("t", &tab_schema()).expect("define");
    tab.pump(&mut rig, &mut node);
    let rec = tab.db.put("t", &serde_json::json!({ "title": "mine" }).as_object().unwrap().clone()).expect("put");
    tab.pump(&mut rig, &mut node);
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    let key = craftworks_sdk::db::record_key("t", loc);
    let (tip_seq, _) = node.head().expect("published");
    let hr = node.head_read().expect("a head");
    let base = hr.prev().expect("a prev");
    let base_tree = node.tree(&base.1).expect("whole");
    let key_of = node.secrets.get(signer::KEY).cloned().expect("provisioned");
    // x: the same-seq winner, P plus an unrelated key (so the record is one-sided mine).
    let mut salt = 0u8;
    let (x_root, x_state) = loop {
        let mut e: Vec<(Vec<u8>, Vec<u8>)> = base_tree.clone().into_iter().collect();
        e.push((b"other".to_vec(), vec![salt]));
        let r = device_tree(&mut node, &e);
        let v = value(&r, &Ledger { prev: Some(signer_proto::Head { seq: base.0, root: base.1 }), ..Ledger::default() });
        if page::beats(&v, hr.value()) {
            break (r, engine_delegate::register::head_state(&node.register_params, &key_of, tip_seq, &v).expect("signs"));
        }
        salt += 1;
    };
    node.update(&x_state);
    // The page learns of x, adopts it and starts the merge; x's blocks are cold,
    // so the merge's delta waits on GETs (held).
    let mut held = Vec::new();
    rig.server.head_hint();
    run_holding_gets(&mut tab, &mut rig, &mut node, &mut held);
    assert_eq!(rig.server.page.published().1, x_root, "the page did not adopt x");
    assert!(!held.is_empty(), "the merge's delta did not need a fetch: the window this test is about does not exist");
    // A new write of this tab, parked on the same cold blocks: the engine is busy.
    tab.db.put("t", &serde_json::json!({ "title": "later" }).as_object().unwrap().clone()).expect("a later put");
    run_holding_gets(&mut tab, &mut rig, &mut node, &mut held);
    // x': ANOTHER head of the other device, on top of x, that changes the record.
    let x_tree = node.tree(&x_root).expect("whole");
    let theirs = b"their record, on x-prime".to_vec();
    let mut e: Vec<(Vec<u8>, Vec<u8>)> = x_tree.into_iter().collect();
    e.retain(|(k, _)| *k != key);
    e.push((key.clone(), theirs.clone()));
    let x2 = device_tree(&mut node, &e);
    let v2 = value(&x2, &Ledger { prev: Some(signer_proto::Head { seq: tip_seq, root: x_root }), ..Ledger::default() });
    node.update(&engine_delegate::register::head_state(&node.register_params, &key_of, tip_seq + 1, &v2).expect("signs"));
    // Release the GETs: the delta completes, the merge write meets a busy engine,
    // the later write's sign meets x' and the page adopts it; the merge re-sends ON x'.
    for op in held.drain(..) {
        if let Some(a) = rig.answer(&mut node, op) {
            rig.server.node(a, Ms(rig.now));
        }
    }
    tab.pump(&mut rig, &mut node);
    for _ in 0..20 {
        rig.now += 1_000;
        rig.server.tick(Ms(rig.now));
        tab.pump(&mut rig, &mut node);
    }
    let fin = node.tree(&node.head().expect("a head").1).expect("whole");
    assert_eq!(fin.get(&key), Some(&theirs), "the merge wrote this page's value over x-prime's: it went blind");
    let told: Vec<Vec<Vec<u8>>> = tab.db.store_mut().take_superseded().into_iter().map(|s| s.keys).collect();
    assert!(told.iter().any(|k| k.contains(&key)), "the record was not told Superseded: {told:?}");
}

// ---- sdk#143/#144: a conflicted update or define is RE-RUN on the new base ----

fn rerun_schema() -> craftworks_sdk::Schema {
    serde_json::from_value(serde_json::json!({ "type": "T", "fields": [
        { "name": "title", "kind": "text", "required": true },
        { "name": "body", "kind": "text" },
        { "name": "note", "kind": "text" }
    ] }))
    .expect("a schema")
}

fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().expect("an object").clone()
}

fn schema_key_of(domain: &str) -> Vec<u8> {
    let mut k = vec![0u8];
    k.extend_from_slice(b"schema\0");
    k.extend_from_slice(domain.as_bytes());
    k
}

/// ANOTHER SESSION writes `ops`, blind, behind the tab's copy: the tab does
/// not hear of it until a write of its own is refused.
fn elsewhere(rig: &mut PageRig, node: &mut Node, write_id: u64, ops: Vec<protocol::Op>) {
    let other = protocol::encode_session_request(VERSION, SESSION + 1, &Request::Write { write_id, ops }).expect("encodes");
    rig.server.client(&other);
    for _ in 0..200 {
        let ops = rig.server.take_ops();
        if ops.is_empty() && !rig.server.page.waiting() {
            break;
        }
        for op in ops {
            if let Some(a) = rig.answer(node, op) {
                rig.server.node(a, Ms(rig.now));
            }
        }
        rig.now += 50;
        rig.server.tick(Ms(rig.now));
    }
    let _ = rig.server.take_replies();
}

/// The record at `key` with `patch` over it, re-encoded as another session
/// would write it.
fn their_record(tab: &mut Tab, schema: &craftworks_sdk::Schema, key: &[u8], patch: serde_json::Value) -> Vec<u8> {
    let old = craftworks_sdk::store::Reads::get(tab.db.store_mut(), key).expect("loaded").expect("present");
    let mut d = craftworks_sdk::record::decode(schema, &old).expect("decodes");
    for (k, v) in obj(patch) {
        d.fields.insert(k, v);
    }
    craftworks_sdk::record::encode(schema, &d.fields, d.created, d.updated + 1, &d.author).expect("encodes")
}

/// What the HOST does on its tick, until nothing moves: re-run, load what the
/// re-run needs (the whole range, as this rig loads), pump. Returns what the
/// re-runs could not keep, and the raw conflicts no re-run took.
fn settle(tab: &mut Tab, rig: &mut PageRig, node: &mut Node) -> (Vec<craftworks_sdk::RerunEvent>, Vec<u64>) {
    let mut events = Vec::new();
    let mut raw = Vec::new();
    for _ in 0..12 {
        tab.pump(rig, node);
        let step = tab.db.rerun(rig.now, craftworks_sdk::Loads::default().budget_ms);
        events.extend(step.events);
        raw.extend(tab.db.store_mut().take_conflicts().into_iter().map(|c| c.write_id).filter(|w| !step.taken.contains(w)));
        if !step.load.is_empty() {
            tab.db.store_mut().request_range(1, b"", &[0xFF; 64], 256);
        }
        tab.pump(rig, node);
    }
    (events, raw)
}

/// The record as the node's head holds it.
fn published_record(node: &Node, schema: &craftworks_sdk::Schema, key: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    let (_, root) = node.head()?;
    let tree = node.tree(&root)?;
    let b = tree.get(key)?;
    Some(craftworks_sdk::record::decode(schema, b).expect("decodes").fields)
}

fn tab_with_record(rig: &mut PageRig, node: &mut Node) -> (Tab, Vec<u8>, craftworks_sdk::id::Loc) {
    let mut tab = Tab::open(rig, node);
    tab.db.define("t", &rerun_schema()).expect("define");
    tab.pump(rig, node);
    let rec = tab.db.put("t", &obj(serde_json::json!({ "title": "t0", "body": "b0", "note": "n0" }))).expect("put");
    tab.pump(rig, node);
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    (tab, craftworks_sdk::db::record_key("t", loc), loc)
}

/// sdk#143 on the PAGE path: another session changes the record's `title`;
/// this tab's stale update changes its `body`. The update conflicts, is
/// RE-RUN on their version, and BOTH land: title t1, body b1. Nothing is told
/// (nothing was lost), the conflict is not raw news, and the stale write went
/// on the wire once — the re-run is a new write. Control: with nothing in
/// between, the update publishes as itself and nothing is re-run.
#[test]
fn a_stale_update_to_another_field_is_re_run_and_both_changes_land() {
    for interfere in [false, true] {
        let mut node = Node::new();
        let mut rig = PageRig::new();
        let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
        if interfere {
            let theirs = their_record(&mut tab, &rerun_schema(), &key, serde_json::json!({ "title": "t1" }));
            elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(key.clone(), theirs)]);
        }
        let w = tab.db.store_mut().next_write_id();
        tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the update is made");
        let (events, raw) = settle(&mut tab, &mut rig, &mut node);
        assert!(events.is_empty(), "interfere {interfere}: nothing was lost and yet {events:?}");
        assert!(raw.is_empty(), "interfere {interfere}: a raw conflict reached the app: {raw:?}");
        assert_eq!(tab.sends.get(&w), Some(&1), "the stale write itself was re-sent");
        let rec = published_record(&node, &rerun_schema(), &key).expect("published");
        let want_title = if interfere { "t1" } else { "t0" };
        assert_eq!((rec.get("title"), rec.get("body")), (Some(&serde_json::json!(want_title)), Some(&serde_json::json!("b1"))), "interfere {interfere}");
        let resent = tab.sends.range(w + 1..).count();
        assert_eq!(resent, usize::from(interfere), "interfere {interfere}: {resent} re-run writes");
    }
}

/// THE SAME FIELD: their title t1, this tab's stale title t2. The other side
/// wins; the tab is told ONCE that `title` was dropped; nothing is written
/// again (the patch has nothing left).
#[test]
fn a_stale_update_to_the_same_field_goes_to_the_other_side_told_once() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let theirs = their_record(&mut tab, &rerun_schema(), &key, serde_json::json!({ "title": "t1" }));
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(key.clone(), theirs)]);
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, &obj(serde_json::json!({ "title": "t2", "body": "b1" }))).expect("the update is made");
    let (events, raw) = settle(&mut tab, &mut rig, &mut node);
    assert_eq!(events, vec![craftworks_sdk::RerunEvent::Dropped { write_id: w, fields: vec!["title".into()] }]);
    assert!(raw.is_empty(), "{raw:?}");
    let rec = published_record(&node, &rerun_schema(), &key).expect("published");
    assert_eq!((rec.get("title"), rec.get("body")), (Some(&serde_json::json!("t1")), Some(&serde_json::json!("b1"))));
}

/// MAIN'S MODEL CASE, THE CHAIN: two stale updates of this tab on one record
/// (title, then body) against their change to a third field (note). The
/// chain is re-run in order, the second on the first's result: all three
/// changes survive, nothing is told.
#[test]
fn a_chain_of_stale_updates_is_re_run_in_order_and_every_change_survives() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let theirs = their_record(&mut tab, &rerun_schema(), &key, serde_json::json!({ "note": "n1" }));
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(key.clone(), theirs)]);
    tab.db.update("t", loc, &obj(serde_json::json!({ "title": "t1" }))).expect("the first update");
    tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the second update");
    let (events, raw) = settle(&mut tab, &mut rig, &mut node);
    assert!(events.is_empty(), "{events:?}");
    assert!(raw.is_empty(), "{raw:?}");
    let rec = published_record(&node, &rerun_schema(), &key).expect("published");
    assert_eq!(
        (rec.get("title"), rec.get("body"), rec.get("note")),
        (Some(&serde_json::json!("t1")), Some(&serde_json::json!("b1")), Some(&serde_json::json!("n1"))),
        "a change of the chain was lost"
    );
}

/// THE BUDGET: the record is changed elsewhere before every re-run lands.
/// After RERUN_ROUNDS re-runs the chain ends as a named failure of its write —
/// never a spin — and their last value stands.
#[test]
fn a_record_changed_under_every_re_run_fails_named_after_the_budget() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let w = tab.db.store_mut().next_write_id();
    let mut events = Vec::new();
    let mut other_id = 1;
    let interfere = |tab: &mut Tab, rig: &mut PageRig, node: &mut Node, other_id: &mut u64| {
        let schema = rerun_schema();
        let (_, root) = node.head().expect("a head");
        let cur = node.tree(&root).expect("a tree").get(&key).cloned().expect("the record");
        let mut d = craftworks_sdk::record::decode(&schema, &cur).expect("decodes");
        d.fields.insert("note".into(), serde_json::json!(format!("n{other_id}")));
        let theirs = craftworks_sdk::record::encode(&schema, &d.fields, d.created, d.updated + 1, &d.author).expect("encodes");
        elsewhere(rig, node, *other_id, vec![protocol::Op::Put(key.clone(), theirs)]);
        *other_id += 1;
        let _ = tab;
    };
    interfere(&mut tab, &mut rig, &mut node, &mut other_id);
    tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the update is made");
    for _ in 0..10 {
        tab.pump(&mut rig, &mut node);
        let step = tab.db.rerun(rig.now, craftworks_sdk::Loads::default().budget_ms);
        events.extend(step.events);
        if !step.load.is_empty() {
            tab.db.store_mut().request_range(1, b"", &[0xFF; 64], 256);
            tab.pump(&mut rig, &mut node);
            // Behind the reload, before the re-run is made: changed again.
            interfere(&mut tab, &mut rig, &mut node, &mut other_id);
        }
    }
    let rounds = usize::from(craftworks_sdk::RERUN_ROUNDS);
    assert_eq!(tab.sends.range(w + 1..).count(), rounds, "not exactly {rounds} re-runs went on the wire");
    assert!(
        matches!(events.as_slice(), [craftworks_sdk::RerunEvent::Failed { reason, .. }] if reason.contains("re-runs")),
        "the chain did not end as ONE named failure: {events:?}"
    );
    let rec = published_record(&node, &rerun_schema(), &key).expect("published");
    assert_eq!(rec.get("body"), Some(&serde_json::json!("b0")), "a failed chain wrote anyway");
}

/// A record DELETED elsewhere: nothing is re-applied, and the tab is told.
#[test]
fn a_stale_update_of_a_record_deleted_elsewhere_is_told_deleted() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Delete(key.clone())]);
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the update is made");
    let (events, _) = settle(&mut tab, &mut rig, &mut node);
    assert_eq!(events, vec![craftworks_sdk::RerunEvent::Deleted { write_id: w }]);
    assert!(published_record(&node, &rerun_schema(), &key).is_none(), "the deleted record came back");
}

/// sdk#144, SCHEMAS: this tab and another session each append a field to the
/// same schema from one base. The tab's define conflicts on the schema key and
/// is RE-RUN as an APPEND on theirs: [title, body, note] + x, + y lands as
/// [title, body, note, x, y] — never the stale whole schema over theirs. A
/// record then carries both.
#[test]
fn a_stale_schema_append_is_re_run_as_an_append_on_theirs() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, _key, _loc) = tab_with_record(&mut rig, &mut node);
    let with = |extra: &[(&str, &str)]| -> craftworks_sdk::Schema {
        let mut s = rerun_schema();
        for (n, k) in extra {
            s.fields.push(serde_json::from_value(serde_json::json!({ "name": n, "kind": k })).expect("a field"));
        }
        s
    };
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(schema_key_of("t"), serde_json::to_vec(&with(&[("x", "text")])).unwrap())]);
    tab.db.define("t", &with(&[("y", "int")])).expect("the define is made");
    let (events, raw) = settle(&mut tab, &mut rig, &mut node);
    assert!(events.is_empty(), "{events:?}");
    assert!(raw.is_empty(), "{raw:?}");
    let (_, root) = node.head().expect("a head");
    let stored: craftworks_sdk::Schema = serde_json::from_slice(node.tree(&root).expect("a tree").get(&schema_key_of("t")).expect("the schema")).expect("a schema");
    let names: Vec<&str> = stored.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["title", "body", "note", "x", "y"]);
}

/// The same name appended on both sides: of the same kind it is already
/// there (no write); of a DIFFERENT kind the re-run is a named refusal and
/// their schema stands.
#[test]
fn a_schema_field_appended_on_both_sides_is_skipped_or_refused_by_kind() {
    for (their_kind, refused) in [("text", false), ("int", true)] {
        let mut node = Node::new();
        let mut rig = PageRig::new();
        let (mut tab, _key, _loc) = tab_with_record(&mut rig, &mut node);
        let with = |n: &str, k: &str| -> craftworks_sdk::Schema {
            let mut s = rerun_schema();
            s.fields.push(serde_json::from_value(serde_json::json!({ "name": "x", "kind": k })).expect("a field"));
            if n == "and z" {
                s.fields.push(serde_json::from_value(serde_json::json!({ "name": "z", "kind": "text" })).expect("a field"));
            }
            s
        };
        // Theirs: + x. Mine: + x (text) and z, so mine is not identical and
        // must be judged field by field.
        elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(schema_key_of("t"), serde_json::to_vec(&with("", their_kind)).unwrap())]);
        let w = tab.db.store_mut().next_write_id();
        tab.db.define("t", &with("and z", "text")).expect("the define is made");
        let (events, _) = settle(&mut tab, &mut rig, &mut node);
        let (_, root) = node.head().expect("a head");
        let stored: craftworks_sdk::Schema = serde_json::from_slice(node.tree(&root).expect("a tree").get(&schema_key_of("t")).expect("the schema")).expect("a schema");
        let names: Vec<&str> = stored.fields.iter().map(|f| f.name.as_str()).collect();
        if refused {
            assert!(matches!(events.as_slice(), [craftworks_sdk::RerunEvent::Failed { write_id, reason }] if *write_id == w && reason.contains("`x`")), "{events:?}");
            assert_eq!(names, ["title", "body", "note", "x"], "their schema did not stand");
        } else {
            assert!(events.is_empty(), "{events:?}");
            assert_eq!(names, ["title", "body", "note", "x", "z"]);
        }
    }
}

/// UNREACHABLE BY DESIGN (schemas only grow), GUARDED ANYWAY: the schema is
/// written back NARROWER elsewhere, without a field this tab's stale patch
/// sets. The re-run is a named refusal naming the field — never a silent drop
/// of it.
#[test]
fn a_patch_field_gone_from_the_schema_is_a_named_refusal() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let narrow: craftworks_sdk::Schema = serde_json::from_value(serde_json::json!({ "type": "T", "fields": [
        { "name": "title", "kind": "text", "required": true }
    ] }))
    .expect("a schema");
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(schema_key_of("t"), serde_json::to_vec(&narrow).unwrap())]);
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the update is made");
    let (events, _) = settle(&mut tab, &mut rig, &mut node);
    assert!(matches!(events.as_slice(), [craftworks_sdk::RerunEvent::Failed { write_id, reason }] if *write_id == w && reason.contains("`body`") && reason.contains("no longer")), "{events:?}");
    let _ = key;
}

/// THE WAIT: a re-run needs its record loaded again (the Conflict forgot it).
/// A host that never loads it does not hold the chain for ever: inside the
/// load path's budget it waits (nothing told, nothing written), past it the
/// write fails named.
#[test]
fn a_re_run_whose_record_never_loads_fails_at_the_load_budget() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let theirs = their_record(&mut tab, &rerun_schema(), &key, serde_json::json!({ "title": "t1" }));
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(key.clone(), theirs)]);
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, &obj(serde_json::json!({ "body": "b1" }))).expect("the update is made");
    tab.pump(&mut rig, &mut node);
    let budget = craftworks_sdk::Loads::default().budget_ms;
    let t0 = rig.now;
    let first = tab.db.rerun(t0, budget);
    assert!(!first.load.is_empty() && first.events.is_empty(), "the re-run did not ask for its record: {first:?}");
    let inside = tab.db.rerun(t0 + budget - 1, budget);
    assert!(!inside.load.is_empty() && inside.events.is_empty(), "gave up inside the budget: {inside:?}");
    let past = tab.db.rerun(t0 + budget, budget);
    assert!(matches!(past.events.as_slice(), [craftworks_sdk::RerunEvent::Failed { write_id, .. }] if *write_id == w), "{past:?}");
    assert!(tab.db.rerun(t0 + budget + 1, budget).events.is_empty(), "told twice");
    assert_eq!(tab.sends.range(w + 1..).count(), 0, "a re-run was written without its record");
}

/// ALREADY SATISFIED (sdk#145's second layer): the other side made the SAME
/// change (their title t1, this tab's stale title t1). Nothing was lost, so
/// nothing is told and nothing is written again. The same shape as this
/// tab's own re-send after a lost answer, when the write is stored already.
#[test]
fn a_stale_update_the_other_side_already_made_is_satisfied_silently() {
    let mut node = Node::new();
    let mut rig = PageRig::new();
    let (mut tab, key, loc) = tab_with_record(&mut rig, &mut node);
    let theirs = their_record(&mut tab, &rerun_schema(), &key, serde_json::json!({ "title": "t1", "note": "n1" }));
    elsewhere(&mut rig, &mut node, 1, vec![protocol::Op::Put(key.clone(), theirs)]);
    let w = tab.db.store_mut().next_write_id();
    tab.db.update("t", loc, &obj(serde_json::json!({ "title": "t1" }))).expect("the update is made");
    let (events, raw) = settle(&mut tab, &mut rig, &mut node);
    assert!(events.is_empty(), "a change that is there was told lost: {events:?}");
    assert!(raw.is_empty(), "{raw:?}");
    assert_eq!(tab.sends.range(w + 1..).count(), 0, "a satisfied change was written again");
    let rec = published_record(&node, &rerun_schema(), &key).expect("published");
    assert_eq!((rec.get("title"), rec.get("note")), (Some(&serde_json::json!("t1")), Some(&serde_json::json!("n1"))));
}
