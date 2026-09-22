//! # page-io: the page executor's I/O over Freenet's client API (ruling B, part 2)
//!
//! [`page::server::Server`] answers the page ↔ engine protocol in-process and
//! emits [`page::Op`]s; this crate is the ONLY place they become client-API
//! frames (`wire`, `wire::signer`) and the node's answers become
//! [`page::Answer`]s again (main's condition 3: no other path to the node).
//! Sans-IO like everything under it: frames in, frames out, and the time.
//!
//! | op | frame |
//! |---|---|
//! | `Put { id, bytes }` | PUT of the Block contract for `id` (`wire::block`), state `kind ‖ body` |
//! | `Get { id }` | GET of that contract |
//! | `ReadHead` | GET **with subscribe** of the head Register: on a peered node a delegate-created register is not served to a bare GET (F55), and the subscription is how a head move reaches the page |
//! | `Update { state }` | the first head this page has never seen a register for → PUT of the Register contract (it CREATES it); after that, UPDATE |
//! | `Sign { id, .. }` | `wire::signer::frame_sign` under the request's own id (SG02) |
//! | `AskHeld { id }` | `wire::signer::frame_held` of the block's contract |
//!
//! | answer | becomes |
//! |---|---|
//! | `Got` of the Register | `Head(record)`; the register now exists |
//! | `GetFailed` / NotFound of the Register | `Head(None)` ONLY if the signer holds no record for it (asked once, `ask_record`); otherwise silence — re-asked on the RTO, "not answering" at its budget (sdk#175; a peered NotFound can be false, F55) |
//! | `Got` of a block | `Got { id, body }` (the executor verifies it against its id) |
//! | `GetFailed` of a block | `GetMissed` |
//! | `Ack(Put)` of a block | `PutOk` |
//! | `Ack(Put)` / `Ack(Updated)` of the Register | `Updated` (it says nothing more, F56); the register exists |
//! | a signer answer | `Signer { id, answer }`; a `Held { present }` goes back to the blocks its id asked about |
//! | `Ack(Put)` / `PutFailed` of any OTHER contract | handed back unread ([`PageIo::take_others`]): the app's own PUTs ([`PageIo::put_contract`], builder#104) are the caller's to match |
//!
//! THE FIRST-PUT RACE: two pages on one key both see no register and both
//! PUT it. The signer signs ONE record from the genesis (at most one
//! signature per prev), so the second page is answered `AlreadySigned` with
//! the FIRST record and PUTs the same bytes: an equal decision, the held
//! bytes win (F56 only ever displaced DIFFERENT roots). Neither PUT fails;
//! the loser's own commit is judged by the read-back and told `Lost`.

use freenet_prolly::Cid;
use freenet_stdlib::prelude::*;
use page::server::{Server, SignerFacts};
use page::{Answer, Ms, Op};
use std::collections::BTreeMap;
use wire::{DelegateKey, Incoming};

/// What the page needs to know about the platform: the Block contract's code
/// (a page PUT carries it), the head Register's code and params, and the
/// signer delegate's key.
pub struct Artefacts {
    pub block_code: Vec<u8>,
    pub register_code: Vec<u8>,
    pub register_params: Vec<u8>,
    pub signer: DelegateKey,
}

pub struct PageIo {
    pub server: Server,
    art: Artefacts,
    register: ContractContainer,
    register_id: [u8; 32],
    register_key: String,
    /// The Register exists on the node: this page read it, or its PUT was
    /// answered. Until then a head goes out as a PUT, which creates it.
    register_seen: bool,
    /// Blocks in flight, by contract id and by key string (a PUT's answer
    /// names the key).
    by_contract: BTreeMap<[u8; 32], Cid>,
    by_key: BTreeMap<String, Cid>,
    /// `Held` asks in flight, by signer request id: the blocks, in order.
    held: BTreeMap<u32, Vec<Cid>>,
    next_held_id: u32,
    frames: wire::Reassembler,
    stream: u32,
    out: Vec<Vec<u8>>,
    replies: Vec<Vec<u8>>,
    unusable: Vec<String>,
    /// The signer answered `Provisioned`: it holds the key and the naming.
    provisioned: bool,
    /// Does the SIGNER hold a record for this register (main's ruling on
    /// sdk#175)? `None`: not asked yet. Only a signer with NO record — and no
    /// head it can read — makes a failed head read "no head"; otherwise the
    /// head exists and a NotFound (which a peered node can answer FALSELY,
    /// F55) is only silence.
    signer_has_record: Option<bool>,
    /// A failed head read waiting on that answer.
    head_failed_pending: bool,
    /// Node answers about contracts that are not this page's register or
    /// blocks — the app's own PUTs — handed back unread (`take_others`).
    others: Vec<Incoming>,
    /// A READER of a NAMED head ([`PageIo::reader`], sdk#239): no signer, so
    /// nothing is signed, PUT or updated, and nothing is installed on the node.
    read_only: bool,
    /// The last time the node or the clock spoke, for answers made locally.
    now: Ms,
    /// A reader's stream-id range (`reader`), in the top byte; 0 for the
    /// person's own page, which keeps the whole space below it.
    stream_base: u32,
}

/// The counter part of a reader's stream id; the top byte is its range.
const STREAM_COUNTER: u32 = 0x00FF_FFFF;

/// The id the record query goes out under.
const RECORD_QUERY_ID: u32 = (1 << 31) - 2;

/// A root no node holds: the record query names it so that nothing can be
/// signed (see `ask_record`).
const UNHELD_ROOT: [u8; 32] = [0xA5; 32];

/// The id a provisioning request goes out under: far from the executor's own
/// sign ids (from 1) and from the `Held` ids (from 2³¹).
const PROVISION_ID: u32 = (1 << 31) - 1;

impl PageIo {
    pub fn new(server: Server, art: Artefacts) -> PageIo {
        let register = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
            std::sync::Arc::new(ContractCode::from(art.register_code.clone())),
            Parameters::from(art.register_params.clone()),
        )));
        let mut register_id = [0u8; 32];
        register_id.copy_from_slice(&register.key().id().as_bytes()[..32]);
        let register_key = register.key().to_string();
        PageIo {
            server,
            art,
            register,
            register_id,
            register_key,
            register_seen: false,
            by_contract: BTreeMap::new(),
            by_key: BTreeMap::new(),
            held: BTreeMap::new(),
            // High, so it never meets the executor's own sign ids (from 1).
            next_held_id: 1 << 31,
            frames: wire::Reassembler::default(),
            stream: 0,
            out: Vec::new(),
            replies: Vec::new(),
            unusable: Vec::new(),
            provisioned: false,
            signer_has_record: None,
            head_failed_pending: false,
            others: Vec::new(),
            read_only: false,
            now: Ms(0),
            stream_base: 0,
        }
    }

    /// A READER of somebody's published head (sdk#239): the page reads the
    /// Register `register_id` names and the blocks under it, and can do
    /// nothing else. Published data is readable by default; writing is access
    /// control, which a reader does not have.
    ///
    /// * NO SIGNER, and nothing provisioned or registered on the node: a
    ///   visitor leaves no trace on the node it reads from beyond the blocks
    ///   it caches and its subscription to the head.
    /// * The head is read by GET with subscribe (a peered node serves a bare
    ///   GET of a delegate-created register falsely, F55; and the subscription
    ///   carries the publisher's next head here).
    /// * A failed head read is SILENCE, re-asked on the RTO — never "no head":
    ///   the head was named because it was published.
    /// * `Put`, `Update` and `Sign` are never framed. `AskHeld` (is a block
    ///   local to the signer?) is answered here: not held, so it is fetched.
    ///
    /// `range` (1..=255) is this reader's stream-id range on the socket it
    /// shares: one per tree, so their chunked frames can never be joined.
    pub fn reader(server: Server, block_code: Vec<u8>, register_id: [u8; 32], range: u8) -> PageIo {
        let (_, no_signer) = wire::delegate_from_code(&[]);
        let mut io = PageIo::new(
            server,
            Artefacts { block_code, register_code: Vec::new(), register_params: Vec::new(), signer: no_signer },
        );
        io.register_id = register_id;
        io.register_key = wire::contract_id(register_id).to_string();
        io.read_only = true;
        io.stream_base = u32::from(range.max(1)) << 24;
        io.provisioned = true;
        // "The head exists": a failed read of it is silence, and the signer
        // is never asked (there is none).
        io.signer_has_record = Some(true);
        io.server.set_facts(SignerFacts { head_writable: false, head_id: register_id });
        io
    }

    /// A reader of a named head (`reader`): nothing can be written.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// PUT a contract the APP names (builder#104: a web container). Framed
    /// here, on this page's stream counter, because this is the only path to
    /// the node (main's condition 3) and two chunked requests on one stream id
    /// would be reassembled into each other. The answer comes back through
    /// [`PageIo::take_others`], named by the contract's key.
    pub fn put_contract(&mut self, contract: ContractContainer, state: WrappedState) -> Result<(), String> {
        let stream = self.next_stream();
        let f = wire::frame_put(contract, state, stream)?;
        self.out.extend(f);
        Ok(())
    }

    /// Node answers this page did not own (see `others`), oldest first.
    pub fn take_others(&mut self) -> Vec<Incoming> {
        std::mem::take(&mut self.others)
    }

    /// PROVISION the signer: register its delegate (from its wasm) and hand it
    /// the head's key, the Register it signs for and the Block code. The key
    /// is the page's TEST key, minted by the caller and forgotten after this
    /// (real device keys are sdk#14). `provisioned()` turns true when the
    /// signer answers `Provisioned`.
    pub fn provision(&mut self, signer: DelegateContainer, signing_key: Vec<u8>) {
        // A READER installs nothing on the node it reads from (sdk#239).
        if self.read_only {
            self.unusable.push("read-only: provisioning refused — a reader installs nothing".into());
            return;
        }
        let stream = self.next_stream();
        match wire::frame_register_delegate(signer, stream) {
            Ok(f) => self.out.extend(f),
            Err(e) => self.unusable.push(format!("could not frame the signer's registration: {e}")),
        }
        let stream = self.next_stream();
        match wire::signer::frame_provision(
            &self.art.signer,
            PROVISION_ID,
            signing_key,
            self.art.register_code.clone(),
            self.art.register_params.clone(),
            self.art.block_code.clone(),
            stream,
        ) {
            Ok(f) => self.out.extend(f),
            Err(e) => self.unusable.push(format!("could not frame the signer's provisioning: {e}")),
        }
    }

    /// The signer said it holds the key and the naming.
    pub fn provisioned(&self) -> bool {
        self.provisioned
    }

    /// The head Register's instance id (what `Identity` reports as `head_id`).
    pub fn register_id(&self) -> [u8; 32] {
        self.register_id
    }

    /// Tell the server what the signer holds, once it is provisioned.
    pub fn signer_provisioned(&mut self) {
        if self.read_only {
            return;
        }
        self.server.set_facts(SignerFacts { head_writable: true, head_id: self.register_id });
    }

    /// A client protocol frame (what `sdk::Client` would have sent a delegate).
    pub fn client(&mut self, bytes: &[u8]) {
        self.server.client(bytes);
        self.pump();
    }

    /// A frame from the node.
    ///
    /// Returns whether the frame was THIS page's — what it asked for, by
    /// contract id or key. One socket can carry several pages (a person's own
    /// tree and the trees they read, sdk#239): each is offered every frame and
    /// takes only its own, and a frame nobody takes is counted once, by the
    /// caller. So a frame that is not this page's is left alone here, not
    /// counted as unusable.
    pub fn inbound(&mut self, bytes: &[u8], now: Ms) -> bool {
        self.now = now;
        let incoming = wire::unframe(&mut self.frames, bytes);
        if !self.owns(&incoming) {
            return false;
        }
        match incoming {
            Incoming::Got { id, state } => {
                if id == self.register_id {
                    self.register_seen = true;
                    // The head WHOLE (root ‖ ledger), tolerantly: the root is
                    // the value's first 32 bytes whatever ledger follows.
                    self.server.node(Answer::Head(page::HeadRead::from_record(&state)), now);
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    let body = wire::block::block_of_state(&state).map(|(_, b)| b.to_vec()).unwrap_or_default();
                    self.server.node(Answer::Got { id: cid, bytes: body }, now);
                } else {
                    self.unusable.push("a GET answer for a contract this page never asked".into());
                }
            }
            Incoming::GetFailed { id } => {
                if id == self.register_id {
                    // A failed read of the head — a refusal, or 0.2.136's
                    // explicit NotFound, which a PEERED node can answer
                    // falsely (F55) — is "no head" ONLY if the signer holds no
                    // record for this register. Otherwise the head exists and
                    // this is SILENCE: re-asked on the RTO, "not answering" at
                    // its budget. Opening an empty tree over an existing app
                    // would have its first commit PUT a second register that
                    // F56 then merges against the real one (sdk#175).
                    match (self.register_seen, self.signer_has_record) {
                        (false, Some(false)) => self.server.node(Answer::Head(None), now),
                        (false, None) => {
                            self.head_failed_pending = true;
                            self.ask_record();
                        }
                        _ => {}
                    }
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    self.server.node(Answer::GetMissed(cid), now);
                }
            }
            Incoming::Ack(wire::AckKind::Put(key)) | Incoming::Ack(wire::AckKind::Updated(key))
                if key == self.register_key || self.by_key.contains_key(&key) =>
            {
                if key == self.register_key {
                    self.register_seen = true;
                    self.server.node(Answer::Updated, now);
                } else if let Some(cid) = self.by_key.get(&key).copied() {
                    self.server.node(Answer::PutOk(cid), now);
                }
            }
            // Someone else's PUT answer: the app's, handed back unread.
            answer @ Incoming::Ack(wire::AckKind::Put(_)) => self.others.push(answer),
            // A refused PUT of our own register or block is what a refusal
            // naming nothing was before `PutFailed` existed: reported.
            Incoming::PutFailed { key, said } if key == self.register_key || self.by_key.contains_key(&key) => {
                self.unusable.push(format!("the node refused: {said}"))
            }
            answer @ Incoming::PutFailed { .. } => self.others.push(answer),
            Incoming::EngineBytes(msgs) => {
                for m in msgs {
                    match wire::signer::read_answer(&m) {
                        Some((_, signer_proto::Answer::Provisioned)) => {
                            self.provisioned = true;
                            self.signer_provisioned();
                        }
                        Some((PROVISION_ID, signer_proto::Answer::Refused(why))) => {
                            self.unusable.push(format!("the signer refused provisioning: {why:?}"));
                        }
                        // The record query's answer (`ask_record`).
                        Some((RECORD_QUERY_ID, answer)) => {
                            use signer_proto::{Answer as A, Why};
                            let has = match answer {
                                // No record, no head it can read: only genesis
                                // would be signable, and the unheld root stops it.
                                A::Refused(Why::RootNotHeld) => Some(false),
                                // A record from genesis, or a later truth.
                                A::AlreadySigned(_) | A::NotNext { .. } | A::Refused(Why::Forked { .. }) => Some(true),
                                _ => None,
                            };
                            if let Some(h) = has {
                                self.signer_has_record = Some(h);
                                if !h && std::mem::take(&mut self.head_failed_pending) && !self.register_seen {
                                    self.server.node(Answer::Head(None), now);
                                }
                            }
                        }
                        Some((id, signer_proto::Answer::Held { present })) => {
                            let asked = self.held.remove(&id).unwrap_or_default();
                            for (cid, p) in asked.into_iter().zip(present) {
                                self.server.node(Answer::Held { id: cid, present: p }, now);
                            }
                        }
                        Some((id, answer)) => self.server.node(Answer::Signer { id, answer }, now),
                        None => self.unusable.push("a delegate message that is not the signer's".into()),
                    }
                }
            }
            // The head moved on the node (the subscription the head read
            // took): the RELOAD TRIGGER. A hint only — the page READS the
            // register and adopts only what that read shows (sdk#225).
            Incoming::HeadChanged { key } if key == self.register_key => self.server.head_hint(),
            Incoming::Refused(r) => self.unusable.push(format!("the node refused: {}", r.said)),
            Incoming::Unusable(u) => self.unusable.push(format!("{u:?}")),
            _ => {}
        }
        self.pump();
        true
    }

    /// Is this frame one this page asked for? A READER owns only its head and
    /// its blocks; a writer also owns its signer's answers, answers that name
    /// nothing, and PUT answers for the app's own contracts (handed back).
    fn owns(&self, incoming: &Incoming) -> bool {
        let mine = |id: &[u8; 32]| *id == self.register_id || self.by_contract.contains_key(id);
        let my_key = |k: &String| *k == self.register_key || self.by_key.contains_key(k);
        match incoming {
            Incoming::Got { id, .. } | Incoming::GetFailed { id } => mine(id),
            Incoming::Ack(wire::AckKind::Put(k)) | Incoming::PutFailed { key: k, .. } => my_key(k) || !self.read_only,
            Incoming::Ack(wire::AckKind::Updated(k)) | Incoming::Ack(wire::AckKind::Subscribed(k)) => my_key(k),
            Incoming::HeadChanged { key } => *key == self.register_key,
            Incoming::Partial => true,
            Incoming::EngineBytes(_) | Incoming::Ack(_) | Incoming::Refused(_) | Incoming::Unusable(_) => !self.read_only,
        }
    }

    /// The page's clock.
    pub fn tick(&mut self, now: Ms) {
        self.now = now;
        self.server.tick(now);
        self.pump();
    }

    /// When the page's next timer falls (a host arms a one-shot for it).
    pub fn next_due(&self) -> Option<Ms> {
        self.server.page.next_due()
    }

    /// Frames for the node, in order.
    pub fn take_frames(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    /// Protocol replies for the client, in order.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.replies)
    }

    pub fn unusable(&self) -> &[String] {
        &self.unusable
    }

    /// Ask the signer whether it holds a record for this register, WITHOUT
    /// being able to sign anything: a sign request from the genesis naming a
    /// root no node holds. `signer::decide` answers it in this order —
    /// `AlreadySigned` if its record's prev is the genesis, `NotNext` if its
    /// record or a head it can read is the truth, and only then, with neither,
    /// `Refused(RootNotHeld)` because the root is not held. Nothing is ever
    /// signed: the root check comes before any signature. ASSUMPTION (a verb
    /// of its own would be clearer; the signer's owner may add one): the
    /// order of `decide` stays as it is, which signer/tests pin.
    fn ask_record(&mut self) {
        let genesis_root = self.server.page.published().1;
        let stream = self.next_stream();
        match wire::signer::frame_sign(
            &self.art.signer,
            RECORD_QUERY_ID,
            signer_proto::Head { seq: 0, root: genesis_root },
            signer_proto::Next { seq: 1, root: UNHELD_ROOT, ledger: Vec::new() },
            stream,
        ) {
            Ok(f) => self.out.extend(f),
            Err(e) => self.unusable.push(format!("could not frame the record query: {e}")),
        }
    }

    fn next_stream(&mut self) -> u32 {
        if self.stream_base == 0 {
            self.stream = self.stream.wrapping_add(1).max(1);
            return self.stream;
        }
        // A READER's ids stay in its own range: `base` in the top byte, a
        // counter below it, so no two pages on one socket share a stream id.
        self.stream = (self.stream.wrapping_add(1) & STREAM_COUNTER).max(1);
        self.stream_base | self.stream
    }

    /// Every op the server emitted becomes a frame; every reply it made is
    /// queued for the client.
    fn pump(&mut self) {
        self.replies.extend(self.server.take_replies());
        let mut not_held = Vec::new();
        for op in self.server.take_ops() {
            if self.read_only {
                match op {
                    Op::AskHeld { id } => {
                        not_held.push(id);
                        continue;
                    }
                    Op::Put { .. } | Op::Update { .. } | Op::Sign { .. } => {
                        self.unusable.push(format!("read-only: a {} was not sent", op_name(&op)));
                        continue;
                    }
                    Op::Get { .. } | Op::ReadHead => {}
                }
            }
            let stream = self.next_stream();
            let framed = match op {
                Op::Put { id, bytes } => match wire::block::block_state(&id, &bytes) {
                    Some(state) => {
                        let c = wire::block::block_contract(&self.art.block_code, &id);
                        self.by_key.insert(c.key().to_string(), id);
                        self.by_contract.insert(wire::block::contract_for(&self.art.block_code, &id), id);
                        wire::frame_put(c, WrappedState::new(state), stream)
                    }
                    None => Err("a block whose bytes hash under no kind".into()),
                },
                Op::Get { id } => {
                    let contract = wire::block::contract_for(&self.art.block_code, &id);
                    self.by_contract.insert(contract, id);
                    wire::frame_get(wire::contract_id(contract), false, stream)
                }
                Op::ReadHead => wire::frame_get(wire::contract_id(self.register_id), true, stream),
                Op::Update { state } => {
                    if self.register_seen {
                        wire::frame_update(self.register.key(), state, stream)
                    } else {
                        // The first head: the Register does not exist yet, and
                        // a PUT is what creates it (no delegate Install on this
                        // path). An existing one merges a PUT like an UPDATE.
                        wire::frame_put(self.register.clone(), WrappedState::new(state), stream)
                    }
                }
                Op::Sign { id, prev_seq, prev_root, seq, root } => wire::signer::frame_sign(
                    &self.art.signer,
                    id,
                    signer_proto::Head { seq: prev_seq, root: prev_root },
                    signer_proto::Next { seq, root, ledger: page::sign_ledger(prev_seq, prev_root, root) },
                    stream,
                ),
                Op::AskHeld { id } => {
                    let hid = self.next_held_id;
                    self.next_held_id = self.next_held_id.wrapping_add(1).max(1 << 31);
                    self.held.insert(hid, vec![id]);
                    wire::signer::frame_held(&self.art.signer, hid, vec![wire::block::contract_for(&self.art.block_code, &id)], stream)
                }
            };
            match framed {
                Ok(f) => self.out.extend(f),
                Err(e) => self.unusable.push(format!("could not frame an op: {e}")),
            }
        }
        if !not_held.is_empty() {
            let now = self.now;
            for id in not_held {
                self.server.node(Answer::Held { id, present: false }, now);
            }
            self.pump();
        }
    }
}

fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Put { .. } => "block PUT",
        Op::Update { .. } => "head update",
        Op::Sign { .. } => "sign request",
        Op::Get { .. } => "block GET",
        Op::ReadHead => "head read",
        Op::AskHeld { .. } => "held query",
    }
}
