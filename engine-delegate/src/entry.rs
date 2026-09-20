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
            InboundDelegateMsg::ApplicationMessage(_) => crate::wire::Saw::Client,
            InboundDelegateMsg::GetContractResponse(_) => crate::wire::Saw::GetResponse,
            InboundDelegateMsg::PutContractResponse(_) => crate::wire::Saw::PutResponse,
            _ => crate::wire::Saw::Other,
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
        let (out, saved, installing) = {
            let blocks = NodeBlocks::with_code(ctx, code.clone());
            let mut shell = Shell::resume_with(&carried, Params::default(), blocks, code.is_some());
            let out = shell.handle(vec![msg]);
            (out, shell.to_context(), shell.installed)
        };
        // The install is the one thing the shell cannot do itself: only the
        // entry point has a secret store. Done AFTER the blocks are dropped,
        // since writing a secret needs the ctx mutably.
        if installing.is_some() {
            if let InboundDelegateMsg::ApplicationMessage(m) = &inbound {
                if let Some(crate::wire::Request::Install {
                    block_code,
                    register_code,
                    register_params,
                    signing_key,
                }) = crate::wire::request(m.payload.as_ref())
                {
                    ctx.set_secret(BLOCK_CODE, &block_code);
                    ctx.set_secret(REGISTER_CODE, &register_code);
                    ctx.set_secret(REGISTER_PARAMS, &register_params);
                    // The signing key. Stored as given and never derived
                    // from; the delegate's only use for it is to sign a
                    // Register record, and today it is a key a driver made
                    // for one run and will remove afterwards.
                    ctx.set_secret(SIGNING_KEY, &signing_key.0);
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
                    // A PACK is a TRANSPORT, and nothing on the node unpacks
                    // one. What a reader asks for is a MEMBER, by its own id,
                    // and a member that exists only inside a pack is a block
                    // the engine cannot read back — which is exactly what
                    // happened: the write published and the very next read of
                    // it answered `Unavailable(root)`.
                    //
                    // So the members are put under their own ids. The pack
                    // existed to beat a low per-call PUT limit, and that
                    // limit turned out not to exist — 64 PUTs from one
                    // process() return are accepted, measured — so unpacking
                    // here costs nothing the pack was buying.
                    if state[0] == 6 {
                        for (mid, mbytes) in engine::pack::members(&bytes) {
                            let Some(mstate) = block_state(&mid, &mbytes) else {
                                unknown_kind += 1;
                                continue;
                            };
                            let mc = block_contract(&code, &mid);
                            asked.push((id32(mc.key().id()), mid));
                            msgs.push(OutboundDelegateMsg::PutContractRequest(
                                PutContractRequest::new(
                                    mc,
                                    WrappedState::new(mstate),
                                    RelatedContracts::default(),
                                ),
                            ));
                        }
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
                    let (Some(rc), Some(rp), Some(sk)) = (
                        ctx.get_secret(REGISTER_CODE),
                        ctx.get_secret(REGISTER_PARAMS),
                        ctx.get_secret(SIGNING_KEY),
                    ) else {
                        continue;
                    };
                    // A head this delegate cannot sign is one the contract
                    // would refuse on arrival, and the commit would then wait
                    // for a confirmation that is never coming. Dropping it
                    // here reaches the same place without the round trip.
                    let Ok(state) = crate::register::head_state(&rp, &sk, seq, &root) else {
                        continue;
                    };
                    let container =
                        ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                            std::sync::Arc::new(ContractCode::from(rc)),
                            Parameters::from(rp),
                        )));
                    msgs.push(OutboundDelegateMsg::PutContractRequest(
                        PutContractRequest::new(
                            container,
                            WrappedState::new(state),
                            RelatedContracts::default(),
                        ),
                    ));
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
                let mut shell =
                    Shell::resume_with(&carried2, Params::default(), blocks, code.is_some());
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
                let mut shell =
                    Shell::resume_with(&carried2, Params::default(), blocks, code.is_some());
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
        let note = crate::wire::reply(&crate::wire::Reply::Call {
            saw,
            ops: msgs.len(),
            stranded: out.stranded,
            no_code: out.refused_no_code,
            dropped: out.dropped.len() + unknown_kind,
            awaiting: out.awaiting,
            read_back: out.read_back_hits,
            effects: out.effects,
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
