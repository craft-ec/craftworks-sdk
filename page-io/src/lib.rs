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
//! | `GetFailed` of the Register | `Head(None)` |
//! | `Got` of a block | `Got { id, body }` (the executor verifies it against its id) |
//! | `GetFailed` of a block | `GetMissed` |
//! | `Ack(Put)` of a block | `PutOk` |
//! | `Ack(Put)` / `Ack(Updated)` of the Register | `Updated` (it says nothing more, F56); the register exists |
//! | a signer answer | `Signer { id, answer }`; a `Held { present }` goes back to the blocks its id asked about |
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
    /// The Register is named by a key THIS page minted (`provision`), so it
    /// cannot exist on the network yet: a failed read of it is certainly
    /// "no head" — the only case where it may be (sdk#175).
    register_new: bool,
}

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
            register_new: false,
        }
    }

    /// PROVISION the signer: register its delegate (from its wasm) and hand it
    /// the head's key, the Register it signs for and the Block code. The key
    /// is the page's TEST key, minted by the caller and forgotten after this
    /// (real device keys are sdk#14). `provisioned()` turns true when the
    /// signer answers `Provisioned`.
    pub fn provision(&mut self, signer: DelegateContainer, signing_key: Vec<u8>) {
        self.register_new = true;
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
        self.server.set_facts(SignerFacts { head_writable: true, head_id: self.register_id });
    }

    /// A client protocol frame (what `sdk::Client` would have sent a delegate).
    pub fn client(&mut self, bytes: &[u8]) {
        self.server.client(bytes);
        self.pump();
    }

    /// A frame from the node.
    pub fn inbound(&mut self, bytes: &[u8], now: Ms) {
        match wire::unframe(&mut self.frames, bytes) {
            Incoming::Got { id, state } => {
                if id == self.register_id {
                    self.register_seen = true;
                    let head = engine_delegate::register::record_of(&state)
                        .and_then(|(seq, v)| v.get(..32).map(|r| (seq, r.try_into().expect("32"))));
                    self.server.node(Answer::Head(head), now);
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    let body = wire::block::block_of_state(&state).map(|(_, b)| b.to_vec()).unwrap_or_default();
                    self.server.node(Answer::Got { id: cid, bytes: body }, now);
                } else {
                    self.unusable.push("a GET answer for a contract this page never asked".into());
                }
            }
            Incoming::GetFailed { id } => {
                if id == self.register_id {
                    // A failed read of the head is SILENCE, re-asked on its RTO
                    // and reported "not answering" at its budget — never "no
                    // head": that would open an EMPTY tree over a register
                    // that is merely unreachable (sdk#175). The one exception
                    // is a register this page just named with a key it minted,
                    // which cannot exist yet.
                    if self.register_new && !self.register_seen {
                        self.server.node(Answer::Head(None), now);
                    }
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    self.server.node(Answer::GetMissed(cid), now);
                }
            }
            Incoming::Ack(wire::AckKind::Put(key)) | Incoming::Ack(wire::AckKind::Updated(key)) => {
                if key == self.register_key {
                    self.register_seen = true;
                    self.server.node(Answer::Updated, now);
                } else if let Some(cid) = self.by_key.get(&key).copied() {
                    self.server.node(Answer::PutOk(cid), now);
                }
            }
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
            Incoming::Refused(r) => self.unusable.push(format!("the node refused: {}", r.said)),
            Incoming::Unusable(u) => self.unusable.push(format!("{u:?}")),
            _ => {}
        }
        self.pump();
    }

    /// The page's clock.
    pub fn tick(&mut self, now: Ms) {
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

    fn next_stream(&mut self) -> u32 {
        self.stream = self.stream.wrapping_add(1).max(1);
        self.stream
    }

    /// Every op the server emitted becomes a frame; every reply it made is
    /// queued for the client.
    fn pump(&mut self) {
        self.replies.extend(self.server.take_replies());
        for op in self.server.take_ops() {
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
                    signer_proto::Next { seq, root, ledger: Vec::new() },
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
    }
}
