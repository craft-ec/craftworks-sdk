//! The delegate the node loads.
//!
//! Nothing but conversion: the platform's types into [`crate::shell`]'s and
//! back. Every rule worth testing lives in the shell, where it is tested
//! without wasm and without a node — what remains here is the part a test
//! could only check by running the thing, and it is kept small for that
//! reason.

use crate::blocks::NodeBlocks;
use crate::schedule::Op;
use crate::shell::{Inbound, Shell};
use engine::Params;
use freenet_stdlib::prelude::*;

pub struct EngineDelegate;

/// The Block contract's STATE for a block the engine emitted.
///
/// The contract's state is `kind ‖ body` and its params are `blake3(state)`,
/// which is exactly `freenet_prolly::block_id(kind, body)` — so the engine's
/// cid IS the params and nothing is re-hashed. What the engine does NOT hand
/// over is the kind: an `Effect` carries the id and the body, because inside
/// the tree the id already determines everything.
///
/// So the kind is recovered by trying the four. That is at most four BLAKE3
/// passes and it is EXACT, because the id is a hash over the kind byte and
/// only the right one can match. Assuming RAW for everything is what the
/// first version did: every PUT was refused by the contract for params that
/// did not hash, and the engine saw only a commit that never published.
fn block_state(id: &[u8; 32], body: &[u8]) -> Option<Vec<u8>> {
    use freenet_prolly::kind;
    // 6 is the Block contract's PACK kind, which freenet-prolly does not
    // export: packs are a transport the tree itself never holds.
    for k in [kind::TREE_NODE, kind::RAW, kind::PARITY, 6u8] {
        if freenet_prolly::block_id(k, body) == *id {
            let mut v = Vec::with_capacity(1 + body.len());
            v.push(k);
            v.extend_from_slice(body);
            return Some(v);
        }
    }
    None
}

/// The contract that holds the block whose id is `params`.
///
/// A block id is NOT a contract instance id: the instance id is derived by
/// the node from the code AND the params together, so the same bytes under a
/// different Block contract are a different contract. The delegate has to
/// build the container to name one.
fn block_contract(code: &[u8], params: &[u8; 32]) -> ContractContainer {
    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(code.to_vec())),
        Parameters::from(params.to_vec()),
    )))
}

/// What the store already holds, as an `Install` finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Held {
    pub block_code: bool,
    pub register_code: bool,
    pub register_params: bool,
    pub signing_key: bool,
}

/// Which secrets an `Install` writes, given what is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writes {
    pub block_code: bool,
    pub register_code: bool,
    pub register_params: bool,
    pub signing_key: bool,
}

/// **`SIGNING_KEY` is never overwritten once present.**
///
/// It is the one secret that cannot be re-derived. The contract codes and the
/// Register's parameters can all be sent again — they are public bytes, and
/// an install that replaces them with the same thing costs nothing. The key
/// cannot: replacing it moves the head's contract id and orphans everything
/// signed with the old one, and nobody kept a copy.
///
/// The first-writer-wins guard in the shell tests `head_writable`, which is
/// code AND params AND key. So a store holding a KEY but missing a code
/// secret reads as "not writable" and an `Install` is allowed through — and
/// it would replace exactly the secret that matters. That state is
/// unreachable today ONLY because the entry point writes the key LAST, which
/// is a property of the write order three lines apart and not a decision
/// anybody recorded. Recorded here: an install over a partial store fills
/// what is missing and KEEPS the key.
pub fn install_writes(held: Held) -> Writes {
    Writes {
        // Public bytes. Re-writing them is free and makes a partial store
        // whole.
        block_code: true,
        register_code: true,
        register_params: true,
        // The one that cannot be re-derived.
        signing_key: !held.signing_key,
    }
}

/// Everything a head write needs, present in the secret store.
///
/// Two callers ask this: `Op::Head`, to decide whether a head can be signed
/// at all, and `Reply::Identity`, to tell a page whether it still has to
/// provision. **They are the same question and must not be allowed to
/// drift.** Were the reported answer `BLOCK_CODE` while the drop tested the
/// signing triple, a partly-installed delegate would report itself ready and
/// then silently drop every head — which is precisely the failure the report
/// exists to prevent. One function, both callers.
///
/// Presence only. The key itself never leaves the store and nothing derived
/// from it is ever reported.
fn head_writable(ctx: &DelegateCtx) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    match (
        ctx.get_secret(REGISTER_CODE),
        ctx.get_secret(REGISTER_PARAMS),
        ctx.get_secret(SIGNING_KEY),
    ) {
        (Some(rc), Some(rp), Some(sk)) => Some((rc, rp, sk)),
        _ => None,
    }
}

fn id32(k: &ContractInstanceId) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&k.as_bytes()[..32]);
    out
}

#[delegate]
impl DelegateInterface for EngineDelegate {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        _origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        // The context is read BEFORE the blocks borrow the ctx, and written
        // after they are dropped.
        let carried = ctx.read();

        // The Register's own contract id, when the delegate has been told
        // which Register holds its head. A GET coming back for THAT id is a
        // head read-back, not a block: both are 32 bytes and the difference
        // is not in the bytes, so it has to be decided here.
        let head_id: Option<[u8; 32]> = match (
            ctx.get_secret(REGISTER_CODE),
            ctx.get_secret(REGISTER_PARAMS),
        ) {
            (Some(rc), Some(rp)) => {
                let c = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                    std::sync::Arc::new(ContractCode::from(rc)),
                    Parameters::from(rp),
                )));
                let mut id = [0u8; 32];
                id.copy_from_slice(&c.key().id().as_bytes()[..32]);
                Some(id)
            }
            _ => None,
        };

        let mut node_said = String::new();
        let mut trace = String::new();
        let saw = match &inbound {
            InboundDelegateMsg::ApplicationMessage(_) => protocol::Saw::Client,
            InboundDelegateMsg::GetContractResponse(_) => protocol::Saw::GetResponse,
            InboundDelegateMsg::PutContractResponse(_) => protocol::Saw::PutResponse,
            InboundDelegateMsg::UpdateContractResponse(_) => protocol::Saw::UpdateResponse,
            _ => protocol::Saw::Other,
        };
        // What block a response is about, if this is one and the pairing was
        // remembered. Read before the shell is built, because the shell needs
        // the answer to build the message it is given.
        let remembered: Option<[u8; 32]> = match &inbound {
            InboundDelegateMsg::GetContractResponse(r) => {
                Shell::<NodeBlocks>::peek_block_for(&carried, &id32(&r.contract_id))
            }
            InboundDelegateMsg::PutContractResponse(r) => {
                Shell::<NodeBlocks>::peek_block_for(&carried, &id32(&r.contract_id))
            }
            _ => None,
        };
        let msg = match &inbound {
            InboundDelegateMsg::ApplicationMessage(m) => Inbound::Client(m.payload.to_vec()),
            InboundDelegateMsg::GetContractResponse(r) => {
                let mut id32 = [0u8; 32];
                id32.copy_from_slice(&r.contract_id.as_bytes()[..32]);
                // The state a Block contract holds is `kind ‖ body`; the
                // engine wants the BODY, under the id it knows. Both
                // translations happen here, or the block arrives as bytes the
                // engine cannot match to anything it asked for.
                let bytes = r.state.as_ref().map(|s| s.as_ref().to_vec());
                if head_id == Some(id32) {
                    // A head read-back. What it must yield is the (seq, root)
                    // the record decided — read out of the record, never
                    // assumed to be the one that was written, because a
                    // DIFFERENT seq there is another writer's head and that is
                    // a conflict rather than a confirmation.
                    match bytes.as_deref().and_then(crate::register::head_of) {
                        Some((seq, root)) => Inbound::GotHead { seq, root },
                        None => Inbound::NoHead,
                    }
                } else {
                    // A response names the CONTRACT; the engine named the
                    // BLOCK. Only the call that issued the request knows they
                    // are the same, so a response for anything else is passed
                    // on under its own id and the shell counts it as unknown.
                    match bytes {
                        // The state IS `kind ‖ body`, so the block id it
                        // belongs to can be computed rather than looked up.
                        // Cheaper and surer than a map: a state that hashes to
                        // something nobody asked for is caught by the engine's
                        // own id check rather than trusted because a map said
                        // so.
                        Some(st) if !st.is_empty() => {
                            let body = st[1..].to_vec();
                            Inbound::GotState {
                                id: freenet_prolly::block_id(st[0], &body),
                                bytes: Some(body),
                            }
                        }
                        // A MISS carries no state to compute from, so this is
                        // the one case the remembered pairing is for.
                        _ => Inbound::GotState {
                            id: remembered.unwrap_or(id32),
                            bytes: None,
                        },
                    }
                }
            }
            // The head's ack when the bump was an UPDATE. Same meaning as
            // the PUT one: accepted, not confirmed — the head is still read
            // back before the commit publishes.
            InboundDelegateMsg::UpdateContractResponse(r) => {
                if let Err(e) = &r.result {
                    node_said = e.clone();
                }
                Inbound::HeadAcked {
                    ok: r.result.is_ok(),
                }
            }
            // The HEAD's own ack. It arrives as a PutContractResponse like
            // any other, and only this point can tell that the contract it
            // names is the Register — the shell would otherwise try to read
            // the head back as a block it has never heard of, which is what
            // it did: a GET loop that ran out its bound every run.
            InboundDelegateMsg::PutContractResponse(r) if head_id == Some(id32(&r.contract_id)) => {
                if let Err(e) = &r.result {
                    node_said = e.clone();
                }
                Inbound::HeadAcked {
                    ok: r.result.is_ok(),
                }
            }
            InboundDelegateMsg::PutContractResponse(r) => {
                let mut id32 = [0u8; 32];
                id32.copy_from_slice(&r.contract_id.as_bytes()[..32]);
                // The node's own words, carried out to where someone can read
                // them. A delegate has no log; discarding this leaves "the
                // put did not stick" with no way to tell a refused contract
                // from a lost message.
                if let Err(e) = &r.result {
                    node_said = e.clone();
                }
                Inbound::PutAcked {
                    // The CONTRACT id, translated back to the block the
                    // engine knows. Telling the core a contract id is telling
                    // it about a block it never emitted, and it ignores it —
                    // which is a commit that never publishes and no error
                    // anywhere.
                    id: {
                        // Whether the contract id could be matched back to a
                        // block the engine knows. An UNPAIRED put response is
                        // one the core will ignore, so it is worth saying.
                        if remembered.is_none() {
                            trace = "shell: put response UNPAIRED".into();
                        }
                        remembered.unwrap_or(id32)
                    },
                    ok: r.result.is_ok(),
                }
            }
            _ => Inbound::Other,
        };

        let code = ctx.get_secret(BLOCK_CODE);
        // Read from the store at the START of this call, which is what a page
        // asking `Identity` needs: the state a PREVIOUS call's `Install` left
        // behind. An `Install` arriving in this call writes its secrets after
        // the shell has already answered, and reports itself through
        // `installed` rather than through this.
        let writable = head_writable(ctx).is_some();
        // Derived from what the node says THIS call, never carried: a
        // remembered "the Register exists" dies with the node, and the first
        // commit after a restart then re-CREATES one that is already there —
        // 152,542 B for a fact the very next head read states (F38).
        //
        // The shell learns it from a HeadRead during this call, so it is read
        // back out of the shell AFTER handling rather than before.
        let head_exists;
        let (out, saved, installing) = {
            let blocks = NodeBlocks::with_code(ctx, code.clone());
            let mut shell = Shell::resume_with(
                &carried,
                Params::default(),
                blocks,
                code.is_some(),
                writable,
            );
            let out = shell.handle(vec![msg]);
            head_exists = shell.head_exists();
            (out, shell.to_context(), shell.installed)
        };
        // The install is the one thing the shell cannot do itself: only the
        // entry point has a secret store. Done AFTER the blocks are dropped,
        // since writing a secret needs the ctx mutably.
        if installing.is_some() {
            if let InboundDelegateMsg::ApplicationMessage(m) = &inbound {
                if let protocol::Incoming::Ok(protocol::Envelope {
                    body:
                        protocol::Request::Install {
                            block_code,
                            register_code,
                            register_params,
                            signing_key,
                        },
                    ..
                }) = protocol::decode_request(m.payload.as_ref())
                {
                    // WRITE ORDER IS LOAD-BEARING, and no longer only
                    // implicitly: the key goes LAST, so a store that holds a
                    // key always holds the codes too. `install_writes` states
                    // the rule that used to rest on these three lines being
                    // in this order.
                    let held = Held {
                        block_code: ctx.get_secret(BLOCK_CODE).is_some(),
                        register_code: ctx.get_secret(REGISTER_CODE).is_some(),
                        register_params: ctx.get_secret(REGISTER_PARAMS).is_some(),
                        signing_key: ctx.get_secret(SIGNING_KEY).is_some(),
                    };
                    let writes = install_writes(held);
                    if writes.block_code {
                        ctx.set_secret(BLOCK_CODE, &block_code);
                    }
                    if writes.register_code {
                        ctx.set_secret(REGISTER_CODE, &register_code);
                    }
                    if writes.register_params {
                        ctx.set_secret(REGISTER_PARAMS, &register_params);
                    }
                    // The signing key. Stored as given and never derived
                    // from; the delegate's only use for it is to sign a
                    // Register record, and today it is a key a driver made
                    // for one run and will remove afterwards. NEVER replaced
                    // once present — see `install_writes`.
                    if writes.signing_key {
                        ctx.set_secret(SIGNING_KEY, &signing_key.0);
                    }
                }
            }
        }

        // A context that cannot be written is not an error to report: the
        // next call starts fresh, re-reads the head, and reports whatever was
        // in flight `Lost`. Failing the call instead would turn a recoverable
        // state into a refused message.
        if let Some(c) = saved.as_deref() {
            ctx.write(c);
        }
        let carried2 = ctx.read();

        let mut msgs: Vec<OutboundDelegateMsg> = Vec::new();
        // Set when the engine asked for its head and there is no Register to
        // ask. Answered on the spot rather than left outstanding.
        let mut no_head = false;
        // Blocks whose id does not hash their own bytes under any kind.
        let mut unknown_kind = 0usize;
        // Packs this shell refused. Counted, so a core that starts emitting
        // them again is visible rather than silently ignored.
        let mut refused_pack = 0usize;
        // What the head bump cost this call, by route. Reported rather than
        // asserted: the saving is a measurement and belongs in the output.
        let mut head_put_bytes = 0usize;
        let mut head_update_bytes = 0usize;
        // contract id -> the block id the engine knows it by.
        let mut asked: Vec<([u8; 32], [u8; 32])> = Vec::new();
        for op in out.ops {
            match op {
                Op::Get { id, .. } => {
                    let Some(code) = code.clone() else { continue };
                    let c = block_contract(&code, &id);
                    asked.push((id32(c.key().id()), id));
                    msgs.push(OutboundDelegateMsg::GetContractRequest(
                        GetContractRequest::new(*c.key().id()),
                    ));
                }
                Op::Put { id, bytes } => {
                    let Some(code) = code.clone() else {
                        // The shell already dropped these; belt and braces.
                        continue;
                    };
                    // A block whose kind cannot be recovered is one whose id
                    // does not hash its own bytes, which the contract would
                    // refuse. Dropped here, where it is counted.
                    let Some(state) = block_state(&id, &bytes) else {
                        unknown_kind += 1;
                        continue;
                    };
                    // PHASE 3 SHIPS NO PACK, and this REFUSES one rather
                    // than quietly coping with it.
                    //
                    // The core does not emit PutPack on the write path
                    // (`pack_on_write: false`), so a pack arriving here means
                    // the core and the shell disagree about the phase. An
                    // unpack arm would handle that silently and go untested
                    // until #39 — unreachable code with no test is worse than
                    // a gap with a name.
                    //
                    // #39 is where the pack comes back, with the half that
                    // makes it pay: the writer puts the pack ONLY, keepers
                    // put the members, and reads resolve through the head's
                    // packs. Wiring the read side is that slice's work.
                    if state[0] == 6 {
                        refused_pack += 1;
                        continue;
                    }
                    let c = block_contract(&code, &id);
                    asked.push((id32(c.key().id()), id));
                    msgs.push(OutboundDelegateMsg::PutContractRequest(
                        PutContractRequest::new(
                            c,
                            WrappedState::new(state),
                            RelatedContracts::default(),
                        ),
                    ));
                }
                // The head is a Register record, and writing it means
                // SIGNING it. Everything needed was provisioned by `Install`
                // and none of it is derived here.
                Op::Head { seq, root } => {
                    let Some((rc, rp, sk)) = head_writable(ctx) else {
                        continue;
                    };
                    // A head this delegate cannot sign is one the contract
                    // would refuse on arrival, and the commit would then wait
                    // for a confirmation that is never coming. Dropping it
                    // here reaches the same place without the round trip.
                    let Ok(state) = crate::register::head_state(&rp, &sk, seq, &root) else {
                        continue;
                    };
                    // CREATE once, UPDATE thereafter.
                    //
                    // A PUT carries the contract's CODE, and code rides every
                    // put at every hop (F18/F28). The Register's wasm is
                    // ~157 KiB, which for a small commit is more than half
                    // the uplink — spent re-sending code the node already
                    // has. An update names the contract by id alone.
                    //
                    // The first bump has no choice: the contract does not
                    // exist until something creates it, and creating it is
                    // what carries the code.
                    if head_exists {
                        head_update_bytes += state.len();
                        msgs.push(OutboundDelegateMsg::UpdateContractRequest(
                            UpdateContractRequest::new(
                                ContractInstanceId::new(head_id.unwrap_or_default()),
                                UpdateData::State(State::from(state)),
                            ),
                        ));
                    } else {
                        let container = ContractContainer::from(ContractWasmAPIVersion::V1(
                            WrappedContract::new(
                                std::sync::Arc::new(ContractCode::from(rc.clone())),
                                Parameters::from(rp),
                            ),
                        ));
                        head_put_bytes += rc.len() + state.len();
                        msgs.push(OutboundDelegateMsg::PutContractRequest(
                            PutContractRequest::new(
                                container,
                                WrappedState::new(state),
                                RelatedContracts::default(),
                            ),
                        ));
                    }
                }
                // Read the head: a GET of the Register this delegate was
                // told holds it. Without a Register it cannot be asked for,
                // and the engine hears `HeadMissing` rather than waiting on a
                // read nobody issued.
                Op::ReadHead { .. } => match head_id {
                    Some(id) => msgs.push(OutboundDelegateMsg::GetContractRequest(
                        GetContractRequest::new(ContractInstanceId::new(id)),
                    )),
                    None => no_head = true,
                },
            }
        }
        // Remember what went out, so a MISS coming back can be matched to the
        // block the engine asked for. Written through a shell so it lands in
        // the same context everything else does.
        if !asked.is_empty() {
            let saved3 = {
                let blocks = NodeBlocks::with_code(ctx, code.clone());
                let mut shell = Shell::resume_with(
                    &carried2,
                    Params::default(),
                    blocks,
                    code.is_some(),
                    writable,
                );
                for (contract, block) in &asked {
                    shell.note_request(*contract, *block);
                }
                shell.to_context()
            };
            if let Some(c) = saved3.as_deref() {
                ctx.write(c);
            }
        }
        let carried2 = ctx.read();

        if no_head {
            // The engine must hear this, or a fresh start waits for ever on a
            // head read that was never issued. Handled by re-entering the
            // shell, so the answer goes through the same path a node's would.
            let (more, saved2) = {
                let blocks = NodeBlocks::with_code(ctx, code.clone());
                let mut shell = Shell::resume_with(
                    &carried2,
                    Params::default(),
                    blocks,
                    code.is_some(),
                    writable,
                );
                let more = shell.handle(vec![Inbound::NoHead]);
                (more, shell.to_context())
            };
            if let Some(c) = saved2.as_deref() {
                ctx.write(c);
            }
            for r in more.replies {
                msgs.push(OutboundDelegateMsg::ApplicationMessage(
                    ApplicationMessage::new(r).processed(true),
                ));
            }
        }
        for r in out.replies {
            msgs.push(OutboundDelegateMsg::ApplicationMessage(
                ApplicationMessage::new(r).processed(true),
            ));
        }
        // What this call did, always. A delegate has no log anyone can read,
        // so a break anywhere in the chain is otherwise indistinguishable
        // from any other break.
        let note = protocol::encode_reply(&protocol::Reply::Call {
            saw,
            ops: msgs.len() as u32,
            stranded: out.stranded as u32,
            // `refused_no_code` folds in here: from outside, a put the shell
            // would not build and a message it could not read are the same
            // thing — something that did not happen, counted.
            dropped: (out.dropped.len() + unknown_kind + refused_pack + out.refused_no_code) as u32,
            awaiting: out.awaiting as u32,
            head_put: head_put_bytes as u32,
            head_update: head_update_bytes as u32,
            read_back: out.read_back_hits as u32,
            effects: out.effects as u32,
            note: if node_said.is_empty() {
                trace
            } else {
                node_said
            },
        });
        msgs.push(OutboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(note).processed(true),
        ));
        Ok(msgs)
    }
}

/// Where the Block contract's code is kept.
///
/// The SECRET store, not the context: secrets are on disk and survive a node
/// restart (measured), while the context is process memory with a ten-minute
/// TTL. Code that had to be re-sent after every restart would make the
/// delegate useless exactly when it is left alone, which is the case it
/// exists for.
const BLOCK_CODE: &[u8] = b"block_contract_code";
/// The Register contract's code, and the keyset its instance is created
/// under. Both supplied, neither derived.
const REGISTER_CODE: &[u8] = b"register_contract_code";
const REGISTER_PARAMS: &[u8] = b"register_contract_params";
/// The device signing key, as handed in. TEST ONLY today — generated per run
/// by the driver, never read from disk, removed at the end of the run.
const SIGNING_KEY: &[u8] = b"device_signing_key";
