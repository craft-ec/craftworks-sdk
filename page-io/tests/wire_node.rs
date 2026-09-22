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
        let (seq, v) = engine_delegate::register::record_of(st)?;
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
                let mut values = Vec::new();
                for m in inbound {
                    if let InboundDelegateMsg::ApplicationMessage(am) = m {
                        let served = signer::serve_full(&mut Host(self), &am.payload);
                        values.push(OutboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(signer::reply(&served))));
                    }
                }
                Some(ok(HostResponse::DelegateResponse { key, values }))
            }
            // Registering the signer again (a reopened page): already held.
            ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { .. }) => {
                *self.served.entry("register delegate").or_default() += 1;
                Some(ok(HostResponse::Ok))
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
    Request::Write { write_id: id, ops: vec![protocol::Op::Put(k.as_bytes().to_vec(), v.as_bytes().to_vec())] }
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
    client(&mut b, &mut node, &mut now, &Request::Identity);
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
/// back to the CALLER by key through `take_others`: never taken as one of the
/// page's block or register answers, never dropped.
#[test]
fn an_apps_put_crosses_the_wire_and_its_answer_is_handed_back_by_key() {
    let mut node = WireNode::new(&[5u8; 32]);
    let mut io = page_io(&node);
    let mut now = 1_000;
    client(&mut io, &mut node, &mut now, &Request::Identity);
    let (key, c, state) = wire::puts::contract(b"an app's contract", b"its params", b"its state");
    let cid = id_of(&c.key());
    io.put_contract(c, state).expect("frames");
    let r = client(&mut io, &mut node, &mut now, &write(1, "a", "1"));
    assert!(states(&r, 1).contains(&WriteState::Published), "the page's own write did not publish beside it: {r:?}");
    assert_eq!(node.contracts.get(&cid).map(Vec::as_slice), Some(b"its state".as_slice()), "the app's PUT never reached the node");
    assert_eq!(io.take_others(), vec![wire::Incoming::Ack(wire::AckKind::Put(key))], "the app's ack was not handed back, or others' were");
    assert!(io.take_others().is_empty(), "handed back twice");
    assert!(io.unusable().is_empty(), "{:?}", io.unusable());
}

/// A PUT refusal NAMING the app's contract is handed back; one naming the
/// page's own register is the page's (reported), not handed back.
#[test]
fn a_put_refusal_goes_to_whoever_owns_the_contract_it_names() {
    use freenet_stdlib::client_api::{ContractError, ErrorKind, RequestError};
    let node = WireNode::new(&[6u8; 32]);
    let mut io = page_io(&node);
    let refusal = |key: ContractKey| {
        let e: Err = ErrorKind::RequestError(RequestError::ContractError(ContractError::Put { key, cause: "invalid put".into() })).into();
        bincode::serialize(&Err::<HostResponse, Err>(e)).expect("encodes")
    };
    let (key, c, _) = wire::puts::contract(b"an app's contract", b"p", b"s");
    io.inbound(&refusal(c.key()), Ms(1));
    assert_eq!(io.take_others(), vec![wire::Incoming::PutFailed { key, said: "invalid put".into() }]);
    assert!(io.unusable().is_empty(), "{:?}", io.unusable());

    let register = ContractKey::from_id_and_code(
        ContractInstanceId::new(node.register_id),
        CodeHash::new([0u8; 32]),
    );
    io.inbound(&refusal(register), Ms(2));
    assert!(io.take_others().is_empty(), "the page's own register refusal was handed to the app");
    assert_eq!(io.unusable(), ["the node refused: invalid put".to_string()]);
}
