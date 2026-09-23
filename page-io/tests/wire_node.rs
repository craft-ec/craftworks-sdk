//! `page-io` END TO END AT THE BYTE LEVEL: the SDK's protocol frames go in,
//! `page-io` frames client-API requests, and a scripted Freenet node DECODES
//! them (the real `ClientRequest` encoding), applies them — blocks, the head
//! Register through its contract's own `update_state`, and the REAL signer
//! behind a real delegate-op envelope — and answers with real encoded
//! `HostResponse`s. The replies the SDK gets back are decoded protocol replies.
//!
//! What a live node adds on top is the network; everything the page does to
//! reach it is exercised here.

use freenet_prolly::Cid;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::*;
use page::server::{Server, SignerFacts};
use page::{Ms, Page, PutPath};
use page_io::{Artefacts, PageIo};
use protocol::{Reply, Request, WriteState};
use std::collections::BTreeMap;

const BLOCK_CODE: &[u8] = b"wire-node block code";
const REGISTER_CODE: &[u8] = b"wire-node register code";
const SIGNER_CODE: &[u8] = b"wire-node signer code";

type Err = freenet_stdlib::client_api::ClientError;

/// The node: contract states by instance id, and the signer's secrets.
struct WireNode {
    contracts: BTreeMap<[u8; 32], Vec<u8>>,
    register_id: [u8; 32],
    register_params: Vec<u8>,
    secrets: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Frames the node answered, by kind (the test proves each path ran).
    served: BTreeMap<&'static str, usize>,
    /// PUTs of the Register (creations), counted.
    register_puts: usize,
    /// The next this many GETs of the Register FAIL, as an unreachable node's
    /// do (sdk#175).
    fail_register_gets: usize,
    /// The next this many signer requests are answered EMPTY — a
    /// DelegateResponse carrying no message — as the real node answered a
    /// request that reached it before the registration had taken (#260,
    /// measured: 7 of 12 opens).
    empty_signer_answers: usize,
    /// The next this many signer answers are LOST — nothing comes back at
    /// all: only the RTO re-ask recovers from that.
    drop_signer_answers: usize,
    /// No signer delegate on this node at all (a visitor's): a signer
    /// request is answered with the node's error.
    delegate_absent: bool,
    /// No signer delegate UNTIL one is registered here: its requests are
    /// answered EMPTY (as 0.2.136 answers a delegate it does not have), and
    /// a registration makes it present.
    empty_until_registered: bool,
}

struct Host<'a>(&'a mut WireNode);
impl signer::Host for Host<'_> {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.secrets.get(key).cloned()
    }
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
        self.0.secrets.insert(key.to_vec(), value.to_vec());
        true
    }
    fn contract_state(&self, id: &[u8; 32]) -> Option<Vec<u8>> {
        self.0.contracts.get(id).cloned()
    }
}

fn id_of(k: &ContractKey) -> [u8; 32] {
    k.id().as_bytes()[..32].try_into().expect("32")
}

fn ok(r: HostResponse) -> Vec<u8> {
    bincode::serialize(&Ok::<HostResponse, Err>(r)).expect("encodes")
}

impl WireNode {
    fn new(signing_key: &[u8; 32]) -> WireNode {
        let sk = ed25519_dalek::SigningKey::from_bytes(signing_key);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        let mut n = WireNode {
            contracts: BTreeMap::new(),
            register_id: signer::register_id(REGISTER_CODE, &params),
            register_params: params.clone(),
            secrets: BTreeMap::new(),
            served: BTreeMap::new(),
            register_puts: 0,
            fail_register_gets: 0,
            empty_signer_answers: 0,
            drop_signer_answers: 0,
            delegate_absent: false,
            empty_until_registered: false,
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

    /// A node whose signer has NEVER been provisioned: the person's first page.
    /// `signing_key` is what that page will mint, so the test knows the register.
    fn unprovisioned(signing_key: &[u8; 32]) -> WireNode {
        let sk = ed25519_dalek::SigningKey::from_bytes(signing_key);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        WireNode {
            contracts: BTreeMap::new(),
            register_id: signer::register_id(REGISTER_CODE, &params),
            register_params: params,
            secrets: BTreeMap::new(),
            served: BTreeMap::new(),
            register_puts: 0,
            fail_register_gets: 0,
            empty_signer_answers: 0,
            drop_signer_answers: 0,
            delegate_absent: false,
            empty_until_registered: false,
        }
    }

    fn merge_register(&mut self, state: &[u8]) {
        let params = Parameters::from(self.register_params.clone());
        let next = match self.contracts.get(&self.register_id) {
            None => state.to_vec(),
            Some(cur) => <craftec_register_contract::Register as ContractInterface>::update_state(
                params,
                State::from(cur.clone()),
                vec![UpdateData::State(State::from(state.to_vec()))],
            )
            .expect("merges")
            .new_state
            .expect("a state")
            .as_ref()
            .to_vec(),
        };
        self.contracts.insert(self.register_id, next);
    }

    fn head(&self) -> Option<(u64, Cid)> {
        let st = self.contracts.get(&self.register_id)?;
        let (seq, v) = contract_keys::register::record_of(st)?;
        Some((seq, v[..32].try_into().expect("32")))
    }

    /// One frame from a page, answered as a node would answer it.
    fn serve(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let req: ClientRequest = bincode::deserialize(frame).expect("page-io framed a real ClientRequest");
        match req {
            ClientRequest::ContractOp(ContractRequest::Put { contract, state, .. }) => {
                let key = contract.key();
                let id = id_of(&key);
                if id == self.register_id {
                    *self.served.entry("put register").or_default() += 1;
                    self.register_puts += 1;
                    self.merge_register(state.as_ref());
                } else {
                    *self.served.entry("put block").or_default() += 1;
                    self.contracts.insert(id, state.as_ref().to_vec());
                }
                Some(ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key })))
            }
            ClientRequest::ContractOp(ContractRequest::Update { key, data: UpdateData::State(s) }) => {
                *self.served.entry("update").or_default() += 1;
                assert_eq!(id_of(&key), self.register_id, "an UPDATE of something other than the head");
                self.merge_register(s.as_ref());
                Some(ok(HostResponse::ContractResponse(ContractResponse::UpdateResponse {
                    key,
                    summary: StateSummary::from(Vec::new()),
                })))
            }
            ClientRequest::ContractOp(ContractRequest::Get { key, subscribe, .. }) => {
                let id: [u8; 32] = key.as_bytes()[..32].try_into().expect("32");
                let failing = id == self.register_id && self.fail_register_gets > 0;
                if failing {
                    self.fail_register_gets -= 1;
                }
                if id == self.register_id {
                    assert!(subscribe, "the head was read WITHOUT a subscription (F55)");
                    *self.served.entry("get register").or_default() += 1;
                } else {
                    *self.served.entry("get block").or_default() += 1;
                }
                match self.contracts.get(&id).filter(|_| !failing) {
                    Some(state) => {
                        let ckey = ContractKey::from_id_and_code(key, CodeHash::new([0u8; 32]));
                        Some(ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                            key: ckey,
                            contract: None,
                            state: WrappedState::new(state.clone()),
                        })))
                    }
                    None => {
                        let e: Err = freenet_stdlib::client_api::ErrorKind::RequestError(
                            freenet_stdlib::client_api::RequestError::ContractError(
                                freenet_stdlib::client_api::ContractError::Get {
                                    key: ContractKey::from_id_and_code(key, CodeHash::new([0u8; 32])),
                                    cause: "not found".into(),
                                },
                            ),
                        )
                        .into();
                        Some(bincode::serialize(&Err::<HostResponse, Err>(e)).expect("encodes"))
                    }
                }
            }
            ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, inbound, .. }) => {
                *self.served.entry("signer").or_default() += 1;
                if self.delegate_absent {
                    return Some(bincode::serialize(&Err::<HostResponse, Err>(wire::_test::client_error("delegate not found"))).expect("encodes"));
                }
                if self.drop_signer_answers > 0 {
                    self.drop_signer_answers -= 1;
                    *self.served.entry("signer answer lost").or_default() += 1;
                    return None;
                }
                if self.empty_until_registered || self.empty_signer_answers > 0 {
                    self.empty_signer_answers = self.empty_signer_answers.saturating_sub(1);
                    return Some(ok(HostResponse::DelegateResponse { key, values: Vec::new() }));
                }
                let mut values = Vec::new();
                for m in inbound {
                    if let InboundDelegateMsg::ApplicationMessage(am) = m {
                        let served = signer::serve_full(&mut Host(self), &am.payload);
                        values.push(OutboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(signer::reply(&served))));
                    }
                }
                Some(ok(HostResponse::DelegateResponse { key, values }))
            }
            // Registering the signer (again, for a reopened page: already
            // held). Answered as the REAL node answers it — measured on
            // 0.2.136 (#260's frame log): an 80-byte DelegateResponse naming
            // the delegate and carrying NO message, which `wire::unframe`
            // reads as `Ack(Registered)`. This answered `HostResponse::Ok`
            // before, which no page ever sees from a node.
            ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, .. }) => {
                *self.served.entry("register delegate").or_default() += 1;
                self.empty_until_registered = false;
                Some(ok(HostResponse::DelegateResponse { key: delegate.key().clone(), values: Vec::new() }))
            }
            other => panic!("page-io sent a request the node does not expect: {other:?}"),
        }
    }
}

/// A page REOPENING on this node: its signer was provisioned before, so it
/// did not mint the key and the register may well exist.
fn page_io(node: &WireNode) -> PageIo {
    let (_, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts {
            block_code: BLOCK_CODE.to_vec(),
            register_code: REGISTER_CODE.to_vec(),
            register_params: node.register_params.clone(),
            signer,
        },
    );
    assert_eq!(io.register_id(), node.register_id, "page-io names the head differently from the node");
    io.signer_provisioned();
    io
}

/// Run the page against the node until nothing is left to send.
fn settle(io: &mut PageIo, node: &mut WireNode, now: &mut u64) -> Vec<Reply> {
    let mut replies = Vec::new();
    for _ in 0..2_000 {
        let frames = io.take_frames();
        replies.extend(io.take_replies().iter().map(|r| protocol::decode_reply(r).expect("a reply")));
        if frames.is_empty() {
            // Only timers left: go to the next one (or now, if it is due).
            match io.next_due() {
                Some(Ms(t)) => {
                    *now = (*now).max(t);
                    io.tick(Ms(*now));
                    continue;
                }
                None => break,
            }
        }
        *now += 1;
        for f in frames {
            if let Some(answer) = node.serve(&f) {
                io.inbound(&answer, Ms(*now));
            }
        }
    }
    replies.extend(io.take_replies().iter().map(|r| protocol::decode_reply(r).expect("a reply")));
    replies
}

fn client(io: &mut PageIo, node: &mut WireNode, now: &mut u64, r: &Request) -> Vec<Reply> {
    io.client(&protocol::encode_session_request(4, 9, r).expect("encodes"));
    settle(io, node, now)
}

fn write(id: u64, k: &str, v: &str) -> Request {
    Request::forced_write(id, vec![protocol::Op::Put(k.as_bytes().to_vec(), v.as_bytes().to_vec())])
}

fn states(rs: &[Reply], id: u64) -> Vec<WriteState> {
    rs.iter()
        .filter_map(|r| match r {
            Reply::WriteState { write_id, state } | Reply::SessionWriteState { write_id, state, .. } if *write_id == id => Some(*state),
            _ => None,
        })
        .collect()
}

#[test]
fn a_write_publishes_through_real_frames_and_the_first_head_creates_the_register() {
    let mut node = WireNode::new(&[3u8; 32]);
    let mut io = page_io(&node);
    let mut now = 1_000;
    let id = client(&mut io, &mut node, &mut now, &Request::Identity);
    assert!(id.iter().any(|r| matches!(r, Reply::Identity { head_writable: true, .. })), "{id:?}");
    let r = client(&mut io, &mut node, &mut now, &write(1, "a", "1"));
    assert!(states(&r, 1).contains(&WriteState::Published), "{r:?}; unusable {:?}", io.unusable());
    assert_eq!(node.register_puts, 1, "the first head did not CREATE the register by PUT");
    let r = client(&mut io, &mut node, &mut now, &write(2, "b", "2"));
    assert!(states(&r, 2).contains(&WriteState::Published), "{r:?}");
    assert_eq!(node.register_puts, 1, "a later head was PUT again instead of UPDATEd");
    assert!(node.served.get("update").is_some_and(|n| *n >= 1), "no UPDATE was ever sent: {:?}", node.served);
    for k in ["put block", "get register", "signer"] {
        assert!(node.served.contains_key(k), "no {k} crossed the wire: {:?}", node.served);
    }
    assert_eq!(node.head().map(|h| h.0), Some(2));
    assert!(io.unusable().is_empty(), "{:?}", io.unusable());
}

/// THE FIRST-PUT RACE: two pages on ONE key, neither has seen a register, both
/// write at once. The signer signs ONE record from the genesis; the second page
/// is answered `AlreadySigned` with it and PUTs the same bytes — an equal
/// decision, held bytes win (F56). The loser's write is told `Lost`, re-sent,
/// and publishes on top.
#[test]
fn two_pages_racing_the_first_put_create_one_register() {
    let mut node = WireNode::new(&[4u8; 32]);
    let (mut a, mut b) = (page_io(&node), page_io(&node));
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    // Page b is its own page LOAD: its own session (and so its own device id
    // in heads' `through`, COMMIT-LIFE ⁵ -- two pages under one session would
    // read each other's `through` as their own).
    b.client(&protocol::encode_session_request(4, 10, &Request::Identity).expect("encodes"));
    settle(&mut b, &mut node, &mut now);
    // Both write before either has heard anything back: interleave their frames.
    a.client(&protocol::encode_session_request(4, 9, &write(1, "a", "1")).expect("encodes"));
    b.client(&protocol::encode_session_request(4, 10, &write(1, "b", "1")).expect("encodes"));
    let (mut ra, mut rb) = (Vec::new(), Vec::new());
    for _ in 0..400 {
        let (fa, fb) = (a.take_frames(), b.take_frames());
        ra.extend(a.take_replies().iter().map(|r| protocol::decode_reply(r).expect("reply")));
        rb.extend(b.take_replies().iter().map(|r| protocol::decode_reply(r).expect("reply")));
        if fa.is_empty() && fb.is_empty() {
            let due = [a.next_due(), b.next_due()].into_iter().flatten().map(|m| m.0).filter(|t| *t > now).min();
            match due {
                Some(t) => {
                    now = t;
                    a.tick(Ms(now));
                    b.tick(Ms(now));
                    continue;
                }
                None => break,
            }
        }
        now += 1;
        for f in fa {
            if let Some(ans) = node.serve(&f) {
                a.inbound(&ans, Ms(now));
            }
        }
        for f in fb {
            if let Some(ans) = node.serve(&f) {
                b.inbound(&ans, Ms(now));
            }
        }
    }
    let (sa, sb) = (states(&ra, 1), states(&rb, 1));
    let published = [&sa, &sb].iter().filter(|s| s.contains(&WriteState::Published)).count();
    let lost = [&sa, &sb].iter().filter(|s| s.contains(&WriteState::Lost)).count();
    assert_eq!((published, lost), (1, 1), "a: {sa:?}; b: {sb:?}");
    assert_eq!(node.head().map(|h| h.0), Some(1), "the race made more than one head at seq 1");
    // The loser re-sends and publishes on top of the winner.
    let (loser, sess) = if sa.contains(&WriteState::Lost) { (&mut a, 9) } else { (&mut b, 10) };
    loser.client(&protocol::encode_session_request(4, sess, &write(2, "late", "2")).expect("encodes"));
    let r = settle(loser, &mut node, &mut now);
    assert!(states(&r, 2).contains(&WriteState::Published), "the race's loser never published: {r:?}");
    assert_eq!(node.head().map(|h| h.0), Some(2));
}

/// sdk#175: a head read that FAILS is silence, never "no head". A page that
/// reopens on a node whose register exists, and whose first head reads fail,
/// must not open an EMPTY tree: it asks again on its RTO and reads the rows.
#[test]
fn a_failed_head_read_never_opens_an_empty_tree() {
    let mut node = WireNode::new(&[6u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    for (n, k) in ["x", "y", "z"].iter().enumerate() {
        let r = client(&mut a, &mut node, &mut now, &write(n as u64 + 1, k, "v"));
        assert!(states(&r, n as u64 + 1).contains(&WriteState::Published));
    }
    // A second page reopens; its first three head reads fail.
    node.fail_register_gets = 3;
    let mut b = page_io(&node);
    client(&mut b, &mut node, &mut now, &Request::Identity);
    let range = Request::Range { req_id: 5, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };
    let r = client(&mut b, &mut node, &mut now, &range);
    let pages: Vec<usize> = r.iter().filter_map(|x| if let Reply::Page { req_id: 5, entries, .. } = x { Some(entries.len()) } else { None }).collect();
    assert_eq!(node.fail_register_gets, 0, "the failing reads were never asked again");
    assert_eq!(pages, vec![3], "the reopened page read {pages:?} rows: a failed head read opened an empty tree");
}

/// Main's ruling on sdk#175: a register read answered NotFound (a peered node
/// can say so FALSELY, F55) on an app that EXISTS must never open an empty
/// tree — even for a page that just provisioned (the same key again). The
/// signer holds a record, so the NotFound is only silence.
#[test]
fn a_false_not_found_on_an_existing_app_never_opens_an_empty_tree() {
    let mut node = WireNode::new(&[7u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    for (n, k) in ["p", "q"].iter().enumerate() {
        assert!(states(&client(&mut a, &mut node, &mut now, &write(n as u64 + 1, k, "v")), n as u64 + 1).contains(&WriteState::Published));
    }
    // A reopened page that PROVISIONS (the same key, accepted) and whose first
    // two head reads are answered "not found".
    node.fail_register_gets = 2;
    let mut b = page_io(&node);
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let (container, _) = wire::delegate_from_code(SIGNER_CODE);
    b.provision(container, sk.to_bytes().to_vec());
    client(&mut b, &mut node, &mut now, &Request::Identity);
    let range = Request::Range { req_id: 6, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };
    let r = client(&mut b, &mut node, &mut now, &range);
    let pages: Vec<usize> = r.iter().filter_map(|x| if let Reply::Page { req_id: 6, entries, .. } = x { Some(entries.len()) } else { None }).collect();
    assert_eq!(pages, vec![2], "a false NotFound on an existing app opened {pages:?}");
}

/// THE CONTROL: a genuinely NEW app (the signer holds no record) whose head
/// read is "not found" does open its empty tree — and publishes onto it.
#[test]
fn control_a_new_apps_not_found_is_no_head() {
    let mut node = WireNode::new(&[8u8; 32]);
    // The signer is provisioned but has signed nothing.
    let mut io = page_io(&node);
    let mut now = 1_000;
    client(&mut io, &mut node, &mut now, &Request::Identity);
    let range = Request::Range { req_id: 7, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };
    let r = client(&mut io, &mut node, &mut now, &range);
    let pages: Vec<usize> = r.iter().filter_map(|x| if let Reply::Page { req_id: 7, entries, .. } = x { Some(entries.len()) } else { None }).collect();
    assert_eq!(pages, vec![0], "a new app did not open its (empty) tree: {r:?}");
}

/// Frames only, NO clock: an op re-sent on its RTO, or the idle backstop,
/// cannot be what moved the page (sdk#225's hint test must not pass on the
/// backstop).
fn pump(io: &mut PageIo, node: &mut WireNode, now: &mut u64) -> Vec<Reply> {
    let mut replies = Vec::new();
    for _ in 0..500 {
        let frames = io.take_frames();
        replies.extend(io.take_replies().iter().map(|r| protocol::decode_reply(r).expect("a reply")));
        if frames.is_empty() {
            break;
        }
        *now += 1;
        for f in frames {
            if let Some(answer) = node.serve(&f) {
                io.inbound(&answer, Ms(*now));
            }
        }
    }
    replies.extend(io.take_replies().iter().map(|r| protocol::decode_reply(r).expect("a reply")));
    replies
}

/// sdk#225's RELOAD TRIGGER through real frames: another tab moves the
/// register; the node pushes `UpdateNotification` for it to the idle tab,
/// which READS the register and adopts what it holds — with no clock
/// passing, so neither a re-send nor the backstop is what moved it — and then
/// reads the other tab's row.
#[test]
fn a_head_changed_from_the_node_makes_an_idle_page_read_and_adopt_the_new_head() {
    let mut node = WireNode::new(&[8u8; 32]);
    let (mut a, mut b) = (page_io(&node), page_io(&node));
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    assert!(states(&client(&mut a, &mut node, &mut now, &write(1, "a", "1")), 1).contains(&WriteState::Published));
    client(&mut b, &mut node, &mut now, &Request::Identity);
    assert!(states(&client(&mut b, &mut node, &mut now, &write(1, "b", "2")), 1).contains(&WriteState::Published));
    assert_eq!(node.head().map(|h| h.0), Some(2));
    // `a` is idle at seq 1. The node's push:
    let key = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(REGISTER_CODE.to_vec())),
        Parameters::from(node.register_params.clone()),
    )))
    .key();
    let state = node.contracts.get(&node.register_id).cloned().expect("a register");
    let push = ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
        key,
        update: UpdateData::State(State::from(state)),
    }));
    a.inbound(&push, Ms(now));
    pump(&mut a, &mut node, &mut now);
    a.client(&protocol::encode_session_request(4, 9, &Request::Get { req_id: 7, key: b"b".to_vec() }).expect("encodes"));
    let r = pump(&mut a, &mut node, &mut now);
    let got = r.iter().any(|x| matches!(x, Reply::Value { req_id: 7, value: Some(v) } if v == b"2"));
    assert!(got, "the idle tab did not adopt the head the node pushed: {r:?}");
}

/// An APP's PUT (builder#104: a web container) goes out through page-io — the
/// only path to the node — beside the page's own writes, and its answer comes
/// back to the PAGE by key, which ends the PUT it sent: never taken as one of
/// the page's block or register answers, never dropped.
#[test]
fn an_apps_put_crosses_the_wire_and_its_answer_is_handed_back_by_key() {
    let mut node = WireNode::new(&[5u8; 32]);
    let mut io = page_io(&node);
    let mut now = 1_000;
    client(&mut io, &mut node, &mut now, &Request::Identity);
    let (key, c, state) = wire::puts::contract(b"an app's contract", b"its params", b"its state");
    let cid = id_of(&c.key());
    io.put_contract(c, state, Ms(now)).expect("frames");
    let r = client(&mut io, &mut node, &mut now, &write(1, "a", "1"));
    assert!(states(&r, 1).contains(&WriteState::Published), "the page's own write did not publish beside it: {r:?}");
    assert_eq!(node.contracts.get(&cid).map(Vec::as_slice), Some(b"its state".as_slice()), "the app's PUT never reached the node");
    // The PAGE sent it, so the page takes its answer: the PUT ENDED, as put.
    assert_eq!(io.app_put(&key), Some(&page::AppPut::Put), "the app's ack did not end its PUT");
    assert!(io.take_others().is_empty(), "the app's own ack was handed back as somebody else's");
    assert!(io.unusable().is_empty(), "{:?}", io.unusable());
}

/// A PUT refusal NAMING a contract this page never PUT is handed back; one
/// naming the app's PUT ends it; one naming the page's own register is the
/// page's (reported), not handed back.
#[test]
fn a_put_refusal_goes_to_whoever_owns_the_contract_it_names() {
    use freenet_stdlib::client_api::{ContractError, ErrorKind, RequestError};
    let node = WireNode::new(&[6u8; 32]);
    let mut io = page_io(&node);
    let refusal = |key: ContractKey| {
        let e: Err = ErrorKind::RequestError(RequestError::ContractError(ContractError::Put { key, cause: "invalid put".into() })).into();
        bincode::serialize(&Err::<HostResponse, Err>(e)).expect("encodes")
    };
    // A contract this page never PUT: handed back unread.
    let (key, c, _) = wire::puts::contract(b"an app's contract", b"p", b"s");
    io.inbound(&refusal(c.key()), Ms(1));
    assert_eq!(io.take_others(), vec![wire::Incoming::PutFailed { key, said: "invalid put".into() }]);
    assert!(io.unusable().is_empty(), "{:?}", io.unusable());
    // One it did PUT: the refusal ENDS it, in the node's words.
    let (mine, c, s) = wire::puts::contract(b"an app's contract", b"mine", b"s");
    io.put_contract(c.clone(), s, Ms(1)).expect("frames");
    io.inbound(&refusal(c.key()), Ms(2));
    assert_eq!(io.app_put(&mine), Some(&page::AppPut::Refused("invalid put".into())));
    assert!(io.take_others().is_empty(), "the app's own refusal was handed back as somebody else's");

    let register = ContractKey::from_id_and_code(
        ContractInstanceId::new(node.register_id),
        CodeHash::new([0u8; 32]),
    );
    io.inbound(&refusal(register), Ms(3));
    assert!(io.take_others().is_empty(), "the page's own register refusal was handed to the app");
    assert_eq!(io.unusable(), ["the node refused: invalid put".to_string()]);
}

fn reader(node: &WireNode) -> PageIo {
    PageIo::reader(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        BLOCK_CODE.to_vec(),
        node.register_id,
        1,
    )
}

fn rows(io: &mut PageIo, node: &mut WireNode, now: &mut u64, req_id: u64) -> Vec<usize> {
    let range = Request::Range { req_id, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };
    let r = client(io, node, now, &range);
    r.iter().filter_map(|x| if let Reply::Page { req_id: q, entries, .. } = x { (*q == req_id).then_some(entries.len()) } else { None }).collect()
}

/// A PUBLISHED HEAD READ BY A VISITOR (sdk#239): a reader of the named
/// register sees the publisher's rows, says the head is NOT writable, and —
/// the addition main asked for — leaves NO trace on the node it reads from:
/// no delegate registered, no signer message, no PUT, no UPDATE. Only reads.
#[test]
fn a_reader_of_a_named_head_sees_the_rows_and_leaves_no_trace_on_the_node() {
    let mut node = WireNode::new(&[9u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    for (n, k) in ["x", "y", "z"].iter().enumerate() {
        assert!(states(&client(&mut a, &mut node, &mut now, &write(n as u64 + 1, k, "v")), n as u64 + 1).contains(&WriteState::Published));
    }
    let before = node.served.clone();

    let mut v = reader(&node);
    assert!(v.read_only() && v.provisioned(), "a reader must be ready with nothing to provision");
    let id = client(&mut v, &mut node, &mut now, &Request::Identity);
    assert!(id.iter().any(|r| matches!(r, Reply::Identity { head_writable: false, head_id, .. } if *head_id == node.register_id)), "{id:?}");
    assert_eq!(rows(&mut v, &mut node, &mut now, 11), vec![3], "the visitor did not see the publisher's rows");

    // What the VISITOR made the node do: reads, and nothing else.
    let visitor: BTreeMap<&str, usize> = node.served.iter().map(|(k, n)| (*k, n - before.get(k).copied().unwrap_or(0))).filter(|(_, n)| *n > 0).collect();
    for k in visitor.keys() {
        assert!(["get register", "get block"].contains(k), "the visitor made the node serve a {k}: {visitor:?}");
    }
    assert!(visitor.contains_key("get register"), "THE CONTROL: the count saw the visitor at all: {visitor:?}");

    // The publisher writes again; the reader, reading again, sees it.
    assert!(states(&client(&mut a, &mut node, &mut now, &write(4, "w", "v")), 4).contains(&WriteState::Published));
    v.server.head_hint();
    let _ = settle(&mut v, &mut node, &mut now);
    assert_eq!(rows(&mut v, &mut node, &mut now, 12), vec![4], "the visitor never saw the publisher's next row");
    assert!(v.unusable().is_empty(), "{:?}", v.unusable());
}

/// The SAFETY NET: a write sent to a reader anyway is never framed — no block
/// PUT, no signer request, no head update reaches the node — and never
/// reported Published. (The UI never offers one: the runtime renders a view.)
#[test]
fn a_write_to_a_reader_reaches_nothing_and_never_publishes() {
    let mut node = WireNode::new(&[10u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    assert!(states(&client(&mut a, &mut node, &mut now, &write(1, "x", "v")), 1).contains(&WriteState::Published));
    let mut v = reader(&node);
    client(&mut v, &mut node, &mut now, &Request::Identity);
    let before = node.served.clone();
    let head = node.head();
    let r = client(&mut v, &mut node, &mut now, &write(2, "intruder", "v"));
    assert!(!states(&r, 2).contains(&WriteState::Published), "a reader's write was reported Published: {r:?}");
    for k in ["put block", "put register", "update", "signer", "register delegate"] {
        assert_eq!(node.served.get(k), before.get(k), "a reader's write made the node serve a {k}");
    }
    assert_eq!(node.head(), head, "a reader's write moved the publisher's head");
    // And provisioning a reader is refused, not sent.
    let (container, _) = wire::delegate_from_code(SIGNER_CODE);
    v.provision(container, vec![0u8; 32]);
    assert!(v.take_frames().is_empty(), "a reader framed a provisioning");
    assert!(v.unusable().iter().any(|u| u.contains("read-only")), "{:?}", v.unusable());
}

/// A failed read of a NAMED head is silence, re-asked — never an empty view,
/// and the reader asks no signer (it has none).
#[test]
fn a_readers_failed_head_read_is_silence_never_an_empty_view() {
    let mut node = WireNode::new(&[11u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    assert!(states(&client(&mut a, &mut node, &mut now, &write(1, "x", "v")), 1).contains(&WriteState::Published));
    node.fail_register_gets = 3;
    let signer_before = node.served.get("signer").copied();
    let mut v = reader(&node);
    client(&mut v, &mut node, &mut now, &Request::Identity);
    assert_eq!(rows(&mut v, &mut node, &mut now, 13), vec![1], "a failed named-head read opened an empty view");
    assert_eq!(node.fail_register_gets, 0, "the failing reads were never asked again");
    assert_eq!(node.served.get("signer").copied(), signer_before, "a reader asked a signer");
}

/// The answers to request `req_id`, as Debug text — what a client would see.
fn answers_to(r: &[Reply], req_id: u64) -> Vec<String> {
    r.iter()
        .filter(|x| match x {
            Reply::Page { req_id: q, .. } | Reply::Unavailable { req_id: q, .. } => *q == req_id,
            _ => false,
        })
        .map(|x| format!("{x:?}"))
        .collect()
}

/// ONE READ PATH (the owner's rule on sdk#239): the SAME tree read through
/// the person's OWN page and through a READER gives the same rows — and,
/// with its blocks corrupted on the node, the same refusal. A reader is the
/// same `PageIo`, `Server` and engine with writing switched off, so any
/// difference here is a second read path.
#[test]
fn the_own_page_and_a_reader_read_the_same_tree_identically_intact_and_corrupted() {
    let mut node = WireNode::new(&[12u8; 32]);
    let mut a = page_io(&node);
    let mut now = 1_000;
    client(&mut a, &mut node, &mut now, &Request::Identity);
    for (n, k) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
        assert!(states(&client(&mut a, &mut node, &mut now, &write(n as u64 + 1, k, &format!("value-{n}"))), n as u64 + 1).contains(&WriteState::Published));
    }
    let range = |req_id| Request::Range { req_id, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };

    // INTACT: a fresh own page (reopened, same key) and a reader.
    let mut own = page_io(&node);
    client(&mut own, &mut node, &mut now, &Request::Identity);
    let mine = answers_to(&client(&mut own, &mut node, &mut now, &range(21)), 21);
    let mut v = reader(&node);
    client(&mut v, &mut node, &mut now, &Request::Identity);
    let theirs = answers_to(&client(&mut v, &mut node, &mut now, &range(21)), 21);
    // Entries print as bytes: the key `alpha` and the value `value-3`.
    let alpha = format!("{:?}", b"alpha".to_vec());
    let value3 = format!("{:?}", b"value-3".to_vec());
    assert!(mine.len() == 1 && mine[0].contains(&alpha) && mine[0].contains(&value3), "the own page did not read the tree: {mine:?}");
    assert_eq!(theirs, mine, "a reader read the same tree differently from the own page");

    // CORRUPTED: every block's bytes on the node changed (the head intact).
    let reg = node.register_id;
    for (id, st) in node.contracts.iter_mut() {
        if *id != reg {
            let last = st.len() - 1;
            st[last] ^= 0x01;
        }
    }
    let mut own = page_io(&node);
    client(&mut own, &mut node, &mut now, &Request::Identity);
    let mine = answers_to(&client(&mut own, &mut node, &mut now, &range(22)), 22);
    let mut v = reader(&node);
    client(&mut v, &mut node, &mut now, &Request::Identity);
    let theirs = answers_to(&client(&mut v, &mut node, &mut now, &range(22)), 22);
    // REFUSED, not an empty page: a block that does not hash to its id is
    // Unavailable (blocked on it), never read as "no rows".
    assert!(mine.len() == 1 && mine[0].starts_with("Unavailable { req_id: 22"), "the own page did not refuse corrupted blocks: {mine:?}");
    assert_eq!(theirs, mine, "a reader and the own page refused a corrupted tree differently");
}

/// A page that OPENS as the Session does now (`begin`): it knows nothing of
/// the register yet — it asks the signer, and mints only when told "none".
/// Returns the page and how many keys it minted.
fn opening(node: &mut WireNode, now: &mut u64, mint: &[u8; 32], always_mint: bool) -> (PageIo, usize) {
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.begin(container);
    settle(&mut io, node, now);
    let mut minted = 0;
    if io.needs_key() || always_mint {
        let sk = ed25519_dalek::SigningKey::from_bytes(mint);
        minted += 1;
        if always_mint && !io.needs_key() {
            // THE MUTANT'S PATH: a page that mints whatever the signer said.
            io = PageIo::new(
                Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
                Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME), signer: wire::delegate_from_code(SIGNER_CODE).1 },
            );
            io.provision(wire::delegate_from_code(SIGNER_CODE).0, sk.to_bytes().to_vec());
        } else {
            io.provision_with(sk.to_bytes().to_vec(), wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME));
        }
        settle(&mut io, node, now);
    }
    (io, minted)
}

fn row_count(io: &mut PageIo, node: &mut WireNode, now: &mut u64, req_id: u64) -> Option<usize> {
    let range = Request::Range { req_id, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 100 };
    client(io, node, now, &range).iter().find_map(|x| match x { Reply::Page { req_id: q, entries, .. } if *q == req_id => Some(entries.len()), _ => None })
}

/// A RELOAD AND A SECOND TAB ARE THE SAME PERSON (the switch-over blocker): a
/// page asks the signer which Register it signs for and opens THAT one. Only
/// the first page ever — on a signer holding no key — mints.
#[test]
fn a_reload_and_a_second_tab_reopen_the_same_register_and_only_the_first_page_mints() {
    let key = [21u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    let mut now = 1_000;
    let (mut first, minted) = opening(&mut node, &mut now, &key, false);
    assert_eq!(minted, 1, "the first page, on a signer with no key, must mint");
    assert!(first.provisioned(), "the minted key was not provisioned: {:?}", first.unusable());
    assert_eq!(first.register_id(), node.register_id);
    client(&mut first, &mut node, &mut now, &Request::Identity);
    for (n, k) in ["a", "b", "c"].iter().enumerate() {
        assert!(states(&client(&mut first, &mut node, &mut now, &write(n as u64 + 1, k, "v")), n as u64 + 1).contains(&WriteState::Published));
    }
    for (i, label) in ["the reload", "a second tab"].into_iter().enumerate() {
        let (mut again, minted) = opening(&mut node, &mut now, &[99u8; 32], false);
        assert_eq!(minted, 0, "{label} minted a new identity");
        assert!(again.provisioned(), "{label} did not open the signer's register: {:?}", again.unusable());
        assert_eq!(again.register_id(), node.register_id, "{label} opened another register");
        client(&mut again, &mut node, &mut now, &Request::Identity);
        // The person's rows: the first page's three, and each earlier reopen's one.
        assert_eq!(row_count(&mut again, &mut node, &mut now, 31 + i as u64), Some(3 + i), "{label} did not see the person's rows");
        let k = format!("from-{i}");
        assert!(states(&client(&mut again, &mut node, &mut now, &write(10 + i as u64, &k, "v")), 10 + i as u64).contains(&WriteState::Published), "{label} cannot write its own tree");
    }
    assert_eq!(node.register_puts, 1, "a second register was created");
}

/// THE CONTROL (the mutant a page that always mints is): a reload that mints
/// its own key is refused by the signer and never sees the person's rows.
#[test]
fn control_a_page_that_always_mints_loses_the_persons_tree() {
    let key = [22u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    let mut now = 1_000;
    let (mut first, _) = opening(&mut node, &mut now, &key, false);
    client(&mut first, &mut node, &mut now, &Request::Identity);
    assert!(states(&client(&mut first, &mut node, &mut now, &write(1, "a", "v")), 1).contains(&WriteState::Published));
    let (again, minted) = opening(&mut node, &mut now, &[98u8; 32], true);
    assert_eq!(minted, 1);
    assert!(!again.provisioned(), "a second key was accepted: the check above could not have told the difference");
    assert!(again.register_id() != node.register_id, "the minting page is not on another register");
}

/// What each frame asks the node, by kind (a delegate registration, a signer
/// request, or anything else).
fn kinds(frames: &[Vec<u8>]) -> Vec<&'static str> {
    frames
        .iter()
        .map(|f| match bincode::deserialize::<ClientRequest>(f) {
            Ok(ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { .. })) => "register",
            Ok(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { .. })) => "signer",
            _ => "other",
        })
        .collect()
}

/// #260, MEASURED: the signer's registration and its first request sent in
/// ONE flush was answered EMPTY by the node 7 times in 12, and the page waited
/// for ever. So the registration goes ALONE, and the first request (the
/// Register query) only once the registration is answered.
#[test]
fn the_signers_first_request_waits_for_its_registration_to_be_answered() {
    let key = [23u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.begin(container);
    let first = io.take_frames();
    assert_eq!(kinds(&first), ["register"], "the first request went out with the registration, before it was answered");
    for f in &first {
        let answer = node.serve(f).expect("the registration is answered");
        io.inbound(&answer, Ms(1_000));
    }
    assert_eq!(kinds(&io.take_frames()), ["signer"], "the registration's answer did not release the Register query");
}

/// THE AMBIGUITY (main's ruling on #260): `wire::unframe` maps ANY empty
/// DelegateResponse to `Ack(Registered)`. Once the registration is answered,
/// an empty response to the outstanding query is "no answer" — asked again —
/// never a second registration. Here the node answers the query empty once,
/// then for real: the page asks again and opens.
#[test]
fn an_empty_reply_to_the_query_is_no_answer_and_the_re_ask_opens() {
    let key = [24u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    node.empty_signer_answers = 1;
    let mut now = 1_000;
    let (io, minted) = opening(&mut node, &mut now, &key, false);
    assert_eq!(node.empty_signer_answers, 0, "the node never answered empty: the case did not happen");
    assert_eq!(minted, 1, "the page never learnt the signer holds no key");
    assert!(io.provisioned(), "an empty reply ended the open: {:?}", io.unusable());
    assert!(node.served["signer"] >= 3, "the query was not asked again after the empty reply ({} signer requests)", node.served["signer"]);
}

/// And it is BOUNDED: a node that only ever answers empty ends the open BY
/// NAME, never a silent wait.
#[test]
fn a_signer_that_only_answers_empty_is_named_not_waited_on() {
    let key = [25u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    node.empty_signer_answers = usize::MAX;
    let mut now = 1_000;
    let (io, _) = opening(&mut node, &mut now, &key, false);
    assert!(!io.provisioned());
    assert!(io.unusable().iter().any(|u| u.contains("answered its first request EMPTY")), "not named: {:?}", io.unusable());
}

/// EVERY NODE CALL ON THE RTO (the ruling since #227): a lost answer to the
/// page's FIRST signer request — "which Register?" — is asked again, and the
/// page opens. Measured before this: 4 of 9 fresh-node page-mode opens hung.
#[test]
fn a_lost_first_signer_answer_is_asked_again_and_the_page_opens() {
    let key = [23u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    node.drop_signer_answers = 1;
    let mut now = 1_000;
    let (page, minted) = opening(&mut node, &mut now, &key, false);
    assert_eq!(node.served.get("signer answer lost"), Some(&1), "THE CONTROL: the first answer was not lost, so nothing was tested");
    assert!(page.provisioned(), "a lost first answer was never asked again: {:?} {:?}", node.served, page.unusable());
    assert_eq!(minted, 1, "the re-asked question was answered 'none' and the key minted once");
    assert!(page.unusable().is_empty(), "{:?}", page.unusable());
}

/// ...and a lost PROVISIONING answer too.
#[test]
fn a_lost_provisioning_answer_is_asked_again() {
    let key = [24u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    let mut now = 1_000;
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.begin(container);
    settle(&mut io, &mut node, &mut now);
    assert!(io.needs_key());
    node.drop_signer_answers = 1;
    let sk = ed25519_dalek::SigningKey::from_bytes(&key);
    io.provision_with(sk.to_bytes().to_vec(), wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME));
    settle(&mut io, &mut node, &mut now);
    assert!(io.provisioned(), "a lost provisioning answer was never asked again: {:?}", node.served);
}

/// BOUNDED: a signer that never answers is asked a bounded number of times,
/// then named "not answering" — never re-asked for ever, never a silent hang.
#[test]
fn a_signer_that_never_answers_is_named_not_answering_and_not_asked_for_ever() {
    let key = [25u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    node.drop_signer_answers = 10_000;
    let mut now = 1_000;
    let (page, _) = opening(&mut node, &mut now, &key, false);
    assert!(!page.provisioned());
    assert!(page.unusable().iter().any(|u| u.starts_with("the signer is not answering")), "{:?}", page.unusable());
    let asked = node.served["signer"];
    assert!((2..=10).contains(&asked), "asked {asked} times within the budget");
    assert!(now >= 1_000 + page::VERIFY_BUDGET_MS, "gave up before its budget, at {now}");
}

/// OPENING ENDS BY NAME (what `open()` reports): REFUSED — the signer's own
/// words — when a page provisions another key over the one it holds.
#[test]
fn opening_that_the_signer_refuses_is_refused_in_its_words() {
    let mut node = WireNode::new(&[26u8; 32]); // the signer already holds this person's key
    let mut now = 1_000;
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let other = ed25519_dalek::SigningKey::from_bytes(&[27u8; 32]);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: wire::register_params(&other.verifying_key().to_bytes(), wire::HEAD_NAME), signer },
    );
    io.provision(container, other.to_bytes().to_vec());
    settle(&mut io, &mut node, &mut now);
    assert!(!io.provisioned());
    assert!(io.refused().is_some_and(|r| r.contains("KeyAlreadyProvisioned")), "{:?}", io.refused());
    assert!(!io.exhausted() && !io.stalled(), "a refusal is not also 'not answering' or 'still waiting'");
}

/// ...EXHAUSTED when its re-asks are spent, and STALLED while it is still
/// waiting past the first RTO — and neither once it is answered.
#[test]
fn opening_is_stalled_while_unanswered_and_exhausted_when_the_reasks_are_spent() {
    // Stalled, stepped by hand: the first answer is lost.
    let key = [28u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    node.drop_signer_answers = 1;
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.begin(container);
    for f in io.take_frames() {
        if let Some(a) = node.serve(&f) { io.inbound(&a, Ms(1_000)); }
    }
    io.tick(Ms(1_000)); // anchors the first exchange
    assert!(!io.stalled(), "stalled before its first RTO");
    io.tick(Ms(2_000)); // past it, and re-asked
    assert!(io.stalled(), "not stalled past its first RTO, unanswered");
    let mut now = 2_000;
    settle(&mut io, &mut node, &mut now);
    assert!(io.needs_key() && !io.stalled() && !io.exhausted(), "answered, and still stalled or exhausted");

    // Exhausted: a signer that never answers.
    let mut silent = WireNode::unprovisioned(&[29u8; 32]);
    silent.drop_signer_answers = 10_000;
    let mut now = 1_000;
    let (page, _) = opening(&mut silent, &mut now, &[29u8; 32], false);
    assert!(page.exhausted() && page.refused().is_none() && !page.stalled(), "exhausted {} refused {:?} stalled {}", page.exhausted(), page.refused(), page.stalled());
}

/// sdk#259: page-io reports the head subscription AS IT IS — asked, answered,
/// and how many head moves it has delivered. On the page path this is what
/// `LiveMode` is built from: the Session's own `watching` belongs to the
/// delegate path the switch-over deleted, so a page that held the
/// subscription the whole time reported "Polled" and a probe read its number
/// as "the tick's wearing a different name".
#[test]
fn page_io_says_whether_the_head_subscription_was_asked_answered_and_delivering() {
    let mut node = WireNode::new(&[3u8; 32]);
    let mut io = page_io(&node);
    let mut now = 0u64;

    // Before anything is sent: nothing asked, nothing answered. NOT "live".
    let h = io.head_subscription();
    assert!(!h.asked && !h.answered && h.changes == 0, "page-io claimed a subscription before it read the head: {h:?}");

    let _ = client(&mut io, &mut node, &mut now, &Request::Identity);
    let h = io.head_subscription();
    println!("  after Identity (no head yet): {h:?}");
    assert!(h.asked, "the head was never read with subscribe, so nothing can notify this page");
    assert!(!h.answered, "a head that does not exist yet was called answered");
    assert!(h.failed > 0, "the node's failure to serve a head that does not exist was not counted, so `Polled` could not say why");

    // The first write CREATES the head register, and the page reads it again.
    let _ = client(&mut io, &mut node, &mut now, &write(1, "k/1", "v"));
    let h = io.head_subscription();
    println!("  after the first write (the head exists): {h:?}");
    assert!(h.asked && h.answered, "the page holds no answered head read once the head exists: nothing can notify it");
    assert_eq!(h.ended, None, "an opening that succeeded was reported as ended");
}

/// sdk#259: an opening that ENDED — refused in the signer's words, here —
/// means no head read is coming, so no subscription either, and the report
/// says so in those words rather than "not answered yet" for ever.
#[test]
fn an_opening_that_ended_is_reported_as_the_reason_there_is_no_subscription() {
    let mut node = WireNode::new(&[26u8; 32]);
    let mut now = 1_000;
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let other = ed25519_dalek::SigningKey::from_bytes(&[27u8; 32]);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: wire::register_params(&other.verifying_key().to_bytes(), wire::HEAD_NAME), signer },
    );
    io.provision(container, other.to_bytes().to_vec());
    settle(&mut io, &mut node, &mut now);
    let h = io.head_subscription();
    assert!(!h.answered, "a page whose opening was refused reported a subscription: {h:?}");
    assert!(
        h.ended.as_deref().is_some_and(|s| s.contains("KeyAlreadyProvisioned")),
        "the reason there is no subscription is not in the report, so it would say 'not answered yet' for ever: {h:?}"
    );
}

/// sdk#259: THE MAPPING, every branch. What a page tells its app — `mode`,
/// `why`, `headChanges` — from page-io's facts. It used to live only in the
/// web Session, which no native test can build: a mutant there that made
/// "subscribed" unreachable (`h if false && h.answered`) survived every suite,
/// so the one thing this report exists to say was never pinned.
#[test]
fn the_live_mode_mapping_says_subscribed_only_when_answered_and_otherwise_why() {
    use page_io::{HeadSubscription, LiveMode};
    let h = |asked, answered, changes, failed, ended: Option<&str>| HeadSubscription { asked, answered, changes, failed, ended: ended.map(str::to_string) };

    // ANSWERED: subscribed, with what it delivered — and it WINS over a
    // failure counted before the head existed (measured on the fixture: 1000
    // failed reads, then answered once the head was created).
    let m = h(true, true, 3, 1000, None).live_mode();
    assert_eq!((m.mode, m.why.as_str(), m.head_changes), ("HeadSubscribed", "", 3), "an answered head read is not reported as a subscription: {m:?}");

    // ENDED: Polled, in the words it ended with — even if a read was asked.
    let m = h(true, false, 0, 2, Some("the signer refused: KeyAlreadyProvisioned")).live_mode();
    assert_eq!(m.mode, "Polled");
    assert!(m.why.contains("opening ended") && m.why.contains("KeyAlreadyProvisioned"), "an ended opening is not the reason given: {m:?}");

    // FAILED: Polled, how many times, and that page-io cannot tell which.
    let m = h(true, false, 0, 4, None).live_mode();
    assert_eq!(m.mode, "Polled");
    assert!(m.why.contains("failure 4 time(s)") && m.why.contains("F55"), "failed head reads are not the reason given: {m:?}");

    // ASKED, unanswered.
    let m = h(true, false, 0, 0, None).live_mode();
    assert_eq!(m.mode, "Polled");
    assert!(m.why.contains("not been answered yet"), "{m:?}");

    // NOT ASKED.
    let m = h(false, false, 0, 0, None).live_mode();
    assert_eq!(m.mode, "Polled");
    assert!(m.why.contains("has not been read"), "{m:?}");

    // No page at all.
    assert_eq!(LiveMode::no_page().mode, "Polled");

    // And end to end, through real frames: a page whose head exists reports
    // HeadSubscribed — the report the two-tab's live arm asserts.
    let mut node = WireNode::new(&[3u8; 32]);
    let mut io = page_io(&node);
    let mut now = 0u64;
    let _ = client(&mut io, &mut node, &mut now, &Request::Identity);
    assert_eq!(io.head_subscription().live_mode().mode, "Polled", "a page reported a subscription to a head that does not exist yet");
    let _ = client(&mut io, &mut node, &mut now, &write(1, "k/1", "v"));
    let m = io.head_subscription().live_mode();
    assert_eq!(m.mode, "HeadSubscribed", "a page whose head read was answered does not say so: {m:?}");
}

/// A page that only ASKS whose node this is (`PageIo::ask`): the same
/// Register query, with no registration before it.
fn asker() -> PageIo {
    let (_, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.ask();
    io
}

/// WHOSE NODE (the owner's ruling): the page asks the node's EXISTING signer
/// which Register it signs for, and nothing is registered, minted or
/// provisioned — on the publisher's node, on a node whose signer holds no key,
/// and on a visitor's node with no signer at all.
#[test]
fn asking_whose_node_registers_mints_and_provisions_nothing() {
    // The publisher's node: its signer names the publisher's Register.
    let mut node = WireNode::new(&[7u8; 32]);
    let secrets = node.secrets.clone();
    let mut io = asker();
    let mut now = 1_000;
    settle(&mut io, &mut node, &mut now);
    assert_eq!(io.asked(), Some(&page_io::Asked::Register(node.register_id)), "{:?}", io.unusable());
    assert!(!io.provisioned(), "asking provisioned the page");
    assert_eq!(node.secrets, secrets, "asking changed the signer's secrets");
    assert_eq!(node.served.get("register delegate"), None, "asking registered the signer: {:?}", node.served);

    // A node whose signer holds no key: "no key", and nothing minted.
    let mut node = WireNode::unprovisioned(&[8u8; 32]);
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert_eq!(io.asked(), Some(&page_io::Asked::NoKey));
    assert!(!io.needs_key(), "asking asked the caller to MINT a key");
    assert!(node.secrets.is_empty(), "asking put a key on a node that had none");
    assert_eq!(node.served.get("register delegate"), None, "asking registered the signer: {:?}", node.served);

    // A visitor's node with no signer: refused, in the node's words.
    let mut node = WireNode::unprovisioned(&[9u8; 32]);
    node.delegate_absent = true;
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(matches!(io.asked(), Some(page_io::Asked::Refused(_))), "{:?}", io.asked());
    // A visitor leaves no trace: the signer was never REGISTERED here.
    assert_eq!(node.served.get("register delegate"), None, "asking registered the signer: {:?}", node.served);

    // As the REAL node answers a delegate it does not have (measured on
    // 0.2.136): EMPTY, every time. Refused as well, by name.
    let mut node = WireNode::unprovisioned(&[10u8; 32]);
    node.empty_signer_answers = usize::MAX;
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(matches!(io.asked(), Some(page_io::Asked::NoSigner(_))), "an EMPTY-answering node is not named as having no signer: {:?}", io.asked());
    assert_eq!(node.served.get("register delegate"), None, "asking registered the signer: {:?}", node.served);
}

/// A VISITOR LEAVES NO TRACE, frame by frame: on a node without the signer
/// (it answers EMPTY, as a real 0.2.136 node does), every frame the asking
/// page sends is the Register QUERY, and none is a registration. And every
/// EMPTY is an answer to a query: the page counts exactly as many as the node
/// served. A page that took the first EMPTY for its signer's registration
/// (the page never registered one) would ask one query more than it counts.
#[test]
fn asking_on_a_visitors_node_sends_only_the_query_and_counts_every_empty_answer() {
    let mut node = WireNode::unprovisioned(&[11u8; 32]);
    node.empty_signer_answers = usize::MAX;
    let mut io = asker();
    let mut now = 1_000u64;
    let mut sent = Vec::new();
    for _ in 0..2_000 {
        let frames = io.take_frames();
        if frames.is_empty() {
            match io.next_due() {
                Some(Ms(t)) => {
                    now = now.max(t);
                    io.tick(Ms(now));
                    continue;
                }
                None => break,
            }
        }
        sent.extend(kinds(&frames));
        now += 1;
        for f in frames {
            if let Some(answer) = node.serve(&f) {
                io.inbound(&answer, Ms(now));
            }
        }
    }
    assert!(!sent.is_empty(), "the asking page sent nothing: the check below could not fail");
    assert!(sent.iter().all(|k| *k == "signer"), "asking sent a frame that is not the query: {sent:?}");
    assert_eq!(node.served.get("register delegate"), None, "asking registered the signer: {:?}", node.served);
    assert!(!io.provisioned() && node.secrets.is_empty(), "asking provisioned the visitor's node");
    let served = node.served.get("signer").copied().unwrap_or(0);
    let counted = match io.asked() {
        // Named as a node with NO SIGNER (its own answer, not a refusal), in
        // words that count the EMPTY answers.
        Some(page_io::Asked::NoSigner(w)) => w
            .split("EMPTY ")
            .nth(1)
            .and_then(|r| r.split(' ').next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("the answer does not say how many EMPTY answers: {w}")),
        other => panic!("a node without the signer was not named as one: {other:?}"),
    };
    assert_eq!(counted, served, "the node answered {served} queries EMPTY and the page counted {counted}: an EMPTY was taken for something else");
    assert_eq!(sent.len(), served, "frames sent {sent:?} against queries served {served}");
}

/// THE CONTROL: the page that OPENS (`begin`) does register the signer — so
/// the "no registration" check above is one that can fail.
#[test]
fn control_opening_registers_the_signer() {
    let mut node = WireNode::new(&[7u8; 32]);
    let (container, signer) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts { block_code: BLOCK_CODE.to_vec(), register_code: REGISTER_CODE.to_vec(), register_params: Vec::new(), signer },
    );
    io.begin(container);
    let mut now = 1_000;
    settle(&mut io, &mut node, &mut now);
    assert_eq!(node.served.get("register delegate"), Some(&1), "{:?}", node.served);
}

/// THE VIEWER'S OWN TREE (DATA-SOURCE `viewer`, sdk#241): an ASKED page is
/// claimed as the person's own, on each kind of node, through `begin`'s own
/// opening from where the answer left it:
/// * the node signs for their Register: it is opened as it is, nothing
///   registered or minted;
/// * its signer holds no key: one is minted (`needs_key`), nothing registered;
/// * there is no signer here: it is registered and asked, then a key minted.
///
/// On every one the first write CREATES the head, and a reopened page reads
/// the row back (a reload keeps the viewer's rows).
#[test]
fn a_claimed_page_opens_the_persons_own_tree_on_each_kind_of_node() {
    for case in ["signs for their register", "signer holds no key", "no signer here"] {
        let key = [31u8; 32];
        let mut node = if case == "signs for their register" { WireNode::new(&key) } else { WireNode::unprovisioned(&key) };
        node.empty_until_registered = case == "no signer here";
        let mut now = 1_000;
        let mut io = asker();
        settle(&mut io, &mut node, &mut now);
        match (case, io.asked()) {
            ("signs for their register", Some(page_io::Asked::Register(r))) => assert_eq!(*r, node.register_id, "{case}"),
            ("signer holds no key", Some(page_io::Asked::NoKey)) | ("no signer here", Some(page_io::Asked::NoSigner(_))) => {}
            (_, other) => panic!("{case}: asked {other:?}"),
        }
        let want = io.asked().cloned();
        let (container, _) = wire::delegate_from_code(SIGNER_CODE);
        assert!(io.claim(container), "{case}: the claim was refused");
        settle(&mut io, &mut node, &mut now);
        if io.needs_key() {
            let sk = ed25519_dalek::SigningKey::from_bytes(&key);
            io.provision_with(sk.to_bytes().to_vec(), wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME));
            settle(&mut io, &mut node, &mut now);
        }
        assert!(io.provisioned(), "{case}: the claimed page never opened: {:?}", io.unusable());
        assert_eq!(io.register_id(), node.register_id, "{case}: the claimed page names another register");
        let registered = node.served.get("register delegate").copied().unwrap_or(0);
        assert_eq!(registered, usize::from(case == "no signer here"), "{case}: the signer was registered {registered} times");
        assert_eq!(io.asked().cloned(), want, "{case}: the claim rewrote the answer (the identity's one owner)");
        client(&mut io, &mut node, &mut now, &Request::Identity);
        let rs = client(&mut io, &mut node, &mut now, &write(1, "mine", "1"));
        assert!(states(&rs, 1).iter().any(|s| matches!(s, WriteState::Published)), "{case}: the viewer's first write did not publish: {:?}", states(&rs, 1));
        assert!(node.contracts.contains_key(&node.register_id), "{case}: the first write did not create the viewer's head");
        let mut again = page_io(&node);
        client(&mut again, &mut node, &mut now, &Request::Identity);
        assert_eq!(row_count(&mut again, &mut node, &mut now, 70), Some(1), "{case}: a reopened page does not read the viewer's row");
    }
}

/// NOTHING IS CLAIMED ON AN ANSWER NOT HAD: before the signer answers, and
/// when it never does, a claim says `false` and nothing is registered,
/// minted or opened -- the runtime shows the inputs disabled, with why.
#[test]
fn a_claim_before_an_answer_or_on_a_silent_signer_claims_nothing() {
    let key = [32u8; 32];
    let mut node = WireNode::new(&key);
    let (container, _) = wire::delegate_from_code(SIGNER_CODE);
    let mut io = asker();
    assert_eq!(io.asked(), None, "answered before the node was asked");
    assert!(!io.claim(container.clone()), "a page claimed its tree before the signer said whose node it is");
    let mut now = 1_000;
    node.drop_signer_answers = usize::MAX;
    settle(&mut io, &mut node, &mut now);
    assert_eq!(io.asked(), Some(&page_io::Asked::NotAnswering), "{:?}", io.unusable());
    assert!(!io.claim(container), "a silent signer's page was claimed");
    assert!(!io.provisioned() && !io.needs_key(), "a refused claim opened or minted");
    assert_eq!(node.served.get("register delegate"), None, "a refused claim registered the signer");
}

/// THE ONE DECISION (`PageIo::may_write`; DATA-SOURCE, the architect's point
/// 5): every cell of the table, answered from what the page holds -- a view,
/// the signer's answer to an ask, or an opened page -- for the page's OWN
/// tree (`None`, a `viewer` component's) and for a named head.
#[test]
fn may_write_is_one_decision_read_from_the_signers_answer() {
    use page_io::MayWrite::{No, Unknown, Yes};
    let yes = |m: page_io::MayWrite| m == Yes;
    let no = |m: page_io::MayWrite| matches!(m, No(_));
    let unknown = |m: page_io::MayWrite| matches!(m, Unknown(_));
    let stranger = [0xAB; 32];
    let mut now = 1_000;

    // A VIEW (`reader`): never, whose ever head.
    let node = WireNode::new(&[40u8; 32]);
    let view = reader(&node);
    assert!(no(view.may_write(None)) && no(view.may_write(Some(node.register_id))), "a view may write");

    // ASKED, not answered yet: not known, for any head.
    let io = asker();
    assert!(unknown(io.may_write(None)) && unknown(io.may_write(Some(stranger))), "an unanswered ask was decided");

    // It signs for their Register: that head, and the own tree, yes; another, no.
    let mut node = WireNode::new(&[41u8; 32]);
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(yes(io.may_write(None)) && yes(io.may_write(Some(node.register_id))), "{:?}", io.may_write(Some(node.register_id)));
    assert!(no(io.may_write(Some(stranger))), "a node that signs for one head may write another");

    // No key, and no signer: the own tree yes (made on first use); another head no.
    for empty in [false, true] {
        let mut node = WireNode::unprovisioned(&[42u8; 32]);
        node.empty_until_registered = empty;
        let mut io = asker();
        settle(&mut io, &mut node, &mut now);
        assert!(yes(io.may_write(None)), "empty={empty}: {:?}", io.asked());
        assert!(no(io.may_write(Some(stranger))), "empty={empty}: a node with no key may write a head");
    }

    // Not answering: not known, for any head.
    let mut node = WireNode::new(&[43u8; 32]);
    node.drop_signer_answers = usize::MAX;
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(unknown(io.may_write(None)) && unknown(io.may_write(Some(node.register_id))), "{:?}", io.asked());

    // Refused by the node: the own tree not known; a named head no.
    let mut node = WireNode::unprovisioned(&[44u8; 32]);
    node.delegate_absent = true;
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(unknown(io.may_write(None)) && no(io.may_write(Some(stranger))), "{:?}", io.asked());

    // OPENED (`begin`'s, or a claimed ask): its register yes, another no.
    let mut node = WireNode::new(&[45u8; 32]);
    let (io, _) = opening(&mut node, &mut now, &[45u8; 32], false);
    assert!(io.provisioned(), "the opening did not open: {:?}", io.unusable());
    assert!(yes(io.may_write(None)) && yes(io.may_write(Some(node.register_id))) && no(io.may_write(Some(stranger))));

    // Claimed where there was no key: the MINTED register is then its own.
    let key = [46u8; 32];
    let mut node = WireNode::unprovisioned(&key);
    let mut io = asker();
    settle(&mut io, &mut node, &mut now);
    assert!(io.claim(wire::delegate_from_code(SIGNER_CODE).0));
    settle(&mut io, &mut node, &mut now);
    let sk = ed25519_dalek::SigningKey::from_bytes(&key);
    io.provision_with(sk.to_bytes().to_vec(), wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME));
    settle(&mut io, &mut node, &mut now);
    assert!(yes(io.may_write(Some(node.register_id))), "a claimed page may not write the register it minted: {:?}", io.may_write(Some(node.register_id)));
    assert!(no(io.may_write(Some(stranger))));
}
