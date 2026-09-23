//! # The SIGNER (sdk#209)
//!
//! ENGINE-SHAPE §4–§6 (owner's GO, 2026-09-23). The head Register is NOT compare-and-set (F56, measured: two writers
//! at one seq are both told success; the lower value hash wins; fork evidence sticks). With one key per device, the
//! only thing that can keep that key from FORKING its own head is whoever holds the key refusing to sign twice. That
//! is this delegate, and it is ALL it is:
//!
//! **ONE verb:** `sign(prev, next) → Signed(state) | NotNext{current} | AlreadySigned(first state) | Refused(why)`
//! (plus `Provision`, which stores the key and what naming the Register and a block needs).
//!
//! - A LOCAL guard so one key never forks, NOT the arbiter. The head it signs is a CLAIM; finality is the attested
//!   CHECKPOINT (owner, 2026-09-23): once a seq is checkpointed it is CLOSED, a late competing claim is rejected there,
//!   and the page rebases onto the attested root.
//! - **No GETs, no contract ops, no tree logic, no memory beyond ONE record.** It reads only by the node's
//!   SYNCHRONOUS local read (`get_contract_state`), so it can never park (F50: only a cold GET/SUBSCRIBE parks a
//!   delegate), never strand, and has nothing to settle.
//! - **But it shares the executor's ONE default queue** (F51; `fair_queue.rs`: a delegate request names no contract,
//!   so it goes to the default queue on the serial `contract_handling` loop, capped at 100 waiting). It waits behind
//!   whatever iteration of another delegate's chain is running, and a burst from another delegate can get a sign
//!   request REFUSED "queue full" with no answer. The page treats a refusal as "ask again" (as sdk#196 1b does): a
//!   re-ask is safe by rule (b), which answers the first record.
//! - **The record is on disk before the reply is built.** `sign` replies `Signed` only after `set_secret(RECORD)`
//!   returned true, and the node's `store_secret` writes a tmp file, `sync_all`s and atomically renames BEFORE it
//!   returns (freenet-core v0.2.135 `wasm_runtime/secrets_store/store.rs:790–832`, read by the architect, sdk#210
//!   review). So "record lost after a signature left" -- the one ordering that would sign twice -- is excluded by
//!   SOURCE; `live-signer` confirms it once (SIGKILL the instant `Signed` arrived, restart, `AlreadySigned(first)`).
//!   A crash INSIDE the write loses a signature nobody received: no fork.
//!
//! [`decide`] is the rule, pure. [`serve`] is the whole request over a [`Host`] (secrets + the sync read), so every
//! ordering the issue names -- the race, identical re-ask, one signature per prev, a record that was not saved -- is
//! driven natively with nothing but a map. `delegate` (the entry the node calls) is `serve` over the real ctx.

use serde::{Deserialize, Serialize};

// The wire types live in `signer-proto`, so the page can speak them without linking this delegate.
pub use signer_proto::{
    decode_answer, decode_request, encode_answer, encode_request, request_id, Answer, Head, Next,
    Request, Why, MAGIC, MAX_PUT_BLOCKS, UNATTRIBUTED,
};

/// THE ONE RECORD the signer keeps: the last thing it signed, from which prev, and the exact bytes returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub prev: Head,
    pub next: Next,
    pub signed: Vec<u8>,
}

/// Everything the rule consults, and nothing else.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub provisioned: bool,
    /// Its one record (`None`: it has never signed).
    pub record: Option<Record>,
    /// The head by the SYNCHRONOUS local read of the Register (`None`: not held locally).
    pub head_read: Option<Head>,
    /// Whether the root block of `next.root` is held (the same sync read).
    pub root_held: bool,
}

/// What the entry must DO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Sign `next`, write the record -- and reply `Signed` ONLY if that write succeeded.
    Sign,
    /// Reply with this and change nothing.
    Reply(Answer),
}

/// THE RULE, pure. Applied in this order:
/// (a) `next.seq == prev.seq + 1`, else `Refused(NotSuccessor)`; not provisioned → `Refused(NotProvisioned)`; a value
///     whose ledger is not the format (`signer_proto::head::check`) → `Refused(BadLedger)`.
/// (b) AT MOST ONE SIGNATURE PER PREV: `record.prev == prev` → `AlreadySigned(record.signed)`, identical re-ask or a
///     different `next` alike. Checked BEFORE (c), so a re-ask after the head moved on still gets its own bytes back.
/// (c) TRUTH = the later of `record.next` and `head_read`, by seq, and at an EQUAL seq the Register's (a read AHEAD
///     is another holder moving the head: the page rebases, nothing is displaced; a read BEHIND is the page's UPDATE
///     not landed yet: the record wins; a read at the record's seq with ANOTHER root is another device of this
///     identity whose head the Register's tie-break kept -- the same identity never forks (owner, sdk#225), so that
///     head is the truth, this signer signs on from it only, and never displaces it: sdk#210 review §1).
///     `prev != truth` → `NotNext{ current: truth }`. No truth at all: only the genesis (`prev.seq == 0`) is signable,
///     else `Refused(HeadUnknown)`. Stated limit: a node that does not HOLD the Register (a second device, a fresh
///     node) with no record signs seq 1 for a key whose head may be further on elsewhere. Harmless at the Register
///     (the higher seq wins) but the page's UPDATE then loses silently, and it learns so only from the head
///     subscription.
/// (d) the root block must be held AND hash to `next.root`, else `Refused(RootNotHeld)`.
/// Otherwise `Sign`.
pub fn decide(f: &Facts, prev: &Head, next: &Next) -> Decision {
    use Answer::*;
    if next.seq != prev.seq.wrapping_add(1) || prev.seq == u64::MAX {
        return Decision::Reply(Refused(Why::NotSuccessor));
    }
    if !f.provisioned {
        return Decision::Reply(Refused(Why::NotProvisioned));
    }
    // The value is the ONE format (signer_proto::head) or nothing is signed:
    // this is the one place a malformed ledger can be stopped.
    if signer_proto::head::check(&next.value()).is_err() {
        return Decision::Reply(Refused(Why::BadLedger));
    }
    if let Some(r) = &f.record {
        if r.prev == *prev {
            return Decision::Reply(AlreadySigned(r.signed.clone()));
        }
    }
    let signed_head = f.record.as_ref().map(|r| r.next.head());
    let truth = match (signed_head, f.head_read) {
        (Some(mine), Some(read)) if read.seq >= mine.seq => Some(read),
        (Some(mine), _) => Some(mine),
        (None, read) => read,
    };
    match truth {
        Some(t) if t != *prev => return Decision::Reply(NotNext { current: t }),
        None if prev.seq != 0 => return Decision::Reply(Refused(Why::HeadUnknown)),
        _ => {}
    }
    if !f.root_held {
        return Decision::Reply(Refused(Why::RootNotHeld));
    }
    Decision::Sign
}

/// Secret-store names.
pub const KEY: &[u8] = b"signer_key";
pub const RECORD: &[u8] = b"signer_record";
pub const REGISTER_CODE: &[u8] = b"signer_register_code";
pub const REGISTER_PARAMS: &[u8] = b"signer_register_params";
pub const BLOCK_CODE: &[u8] = b"signer_block_code";

/// What the signer reaches, and nothing more: its secret store and the node's SYNCHRONOUS local read.
pub trait Host {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool;
    fn contract_state(&self, instance_id: &[u8; 32]) -> Option<Vec<u8>>;
}

/// The Register's instance id, from its code and params.
pub fn register_id(code: &[u8], params: &[u8]) -> [u8; 32] {
    use freenet_stdlib::prelude::*;
    let id = ContractInstanceId::from_params_and_code(
        Parameters::from(params.to_vec()),
        ContractCode::from(code.to_vec()),
    );
    let mut out = [0u8; 32];
    out.copy_from_slice(&id.as_bytes()[..32]);
    out
}

/// `(block id, state)` pairs to PUT under the Block contract, in order.
pub type Puts = Vec<([u8; 32], Vec<u8>)>;

/// One request, served, and what the entry must PUT for it (only `PutBlocks` puts anything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The request's id, echoed on the answer (SG02): what attributes it with several requests in flight.
    pub id: u32,
    pub answer: Answer,
    /// `(block id, state)` to PUT under the Block contract, in order; the entry builds each contract from the code in
    /// its secret store and answers every `PutContractResponse` with `Answer::Put`.
    pub puts: Puts,
}

/// WHO IS ASKING, as the node attests it: a SERVED APP (`WebApp`) or anything else (the builder's page, a native
/// tool: unattested). Only `SignSite` consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A web app the node serves (freenet's `MessageOrigin::WebApp`).
    WebApp,
    /// Not a served app: the builder's own page, or a native tool.
    Unattested,
}

/// One request, answered: `serve_full` without the puts or the id. What every non-`PutBlocks` request needs.
pub fn serve<H: Host>(host: &mut H, request: &[u8]) -> Answer {
    serve_full(host, request).answer
}

/// [`serve_from`] for an UNATTESTED caller: the builder's page and the native fixtures that stand in for it. The
/// node's own entry (`delegate.rs`) never uses this: it passes the origin the node attests.
pub fn serve_full<H: Host>(host: &mut H, request: &[u8]) -> Served {
    serve_from(host, request, Origin::Unattested)
}

/// The bytes the entry sends back for a served request: its answer, under ITS id.
pub fn reply(served: &Served) -> Vec<u8> {
    encode_answer(served.id, &served.answer)
}

/// One request, served: gather the facts, decide, and -- on `Sign` -- sign, save the record, reply; on `PutBlocks`
/// name each block's contract and hand the entry the PUTs. `origin` is who asks, as the node attests it.
pub fn serve_from<H: Host>(host: &mut H, request: &[u8], origin: Origin) -> Served {
    let Some((id, req)) = decode_request(request) else {
        return Served {
            id: request_id(request),
            answer: Answer::Refused(Why::Unreadable),
            puts: Vec::new(),
        };
    };
    let answer = match req {
        Request::PutBlocks { states } => {
            let (answer, puts) = put_blocks(host, states);
            return Served { id, answer, puts };
        }
        Request::Held { contracts } => held(host, &contracts),
        Request::Provision {
            signing_key,
            register_code,
            register_params,
            block_code,
        } => provision(
            host,
            signing_key,
            register_code,
            register_params,
            block_code,
        ),
        Request::Sign { prev, next } => sign(host, prev, next),
        // The Register it signs for, only if it holds a key for it: params left behind without a key name nothing.
        Request::Register => Answer::Register {
            params: host.get_secret(KEY).and(host.get_secret(REGISTER_PARAMS)),
        },
        Request::SignSite { app, version, bundle } => sign_site(host, origin, &app, version, bundle),
    };
    Served {
        id,
        answer,
        puts: Vec::new(),
    }
}

/// Store the key and what naming the Register and a block needs: refused for another key, or another Register.
fn provision<H: Host>(
    host: &mut H,
    signing_key: Vec<u8>,
    register_code: Vec<u8>,
    register_params: Vec<u8>,
    block_code: Vec<u8>,
) -> Answer {
    if let Some(held) = host.get_secret(KEY) {
        if held != signing_key {
            return Answer::Refused(Why::KeyAlreadyProvisioned);
        }
        // The same key re-provisioned: only for the SAME Register. The record belongs to the Register it was
        // signed for; another Register's requests must never be answered from it.
        let same = host
            .get_secret(REGISTER_PARAMS)
            .is_none_or(|p| p == register_params)
            && host
                .get_secret(REGISTER_CODE)
                .is_none_or(|c| c == register_code);
        if !same {
            return Answer::Refused(Why::RegisterChanged);
        }
    }
    let ok = host.set_secret(KEY, &signing_key)
        && host.set_secret(REGISTER_CODE, &register_code)
        && host.set_secret(REGISTER_PARAMS, &register_params)
        && host.set_secret(BLOCK_CODE, &block_code);
    if ok {
        Answer::Provisioned
    } else {
        Answer::Refused(Why::NotProvisioned)
    }
}

/// READ-LOCAL: whether this node holds each contract's state, by the host's synchronous local read -- never a
/// network fetch, so it cannot park. Needs no provisioning: it reads, and says nothing but present / absent.
fn held<H: Host>(host: &H, contracts: &[[u8; 32]]) -> Answer {
    if contracts.is_empty() || contracts.len() > MAX_PUT_BLOCKS {
        return Answer::Refused(Why::BlockCount {
            max: MAX_PUT_BLOCKS as u32,
            got: contracts.len() as u32,
        });
    }
    Answer::Held {
        present: contracts
            .iter()
            .map(|c| host.contract_state(c).is_some())
            .collect(),
    }
}

/// PUT-WITH-CODE: the page's block STATES, each named by its own hash, PUT under the Block contract from inside the
/// node. Nothing is remembered: the answer names the contracts in order, and each node answer is relayed as it comes.
/// Refused whole if any state is not a block, or the count is outside one return's worth.
///
/// Stated limit (F35, measured): in LOCAL mode the node DROPS a delegate-originated PUT, so this verb is for a network
/// node -- the page on its OWN node PUTs directly (the code is a local copy there).
fn put_blocks<H: Host>(host: &mut H, states: Vec<Vec<u8>>) -> (Answer, Puts) {
    let refuse = |w: Why| (Answer::Refused(w), Vec::new());
    let Some(code) = host.get_secret(BLOCK_CODE) else {
        return refuse(Why::NotProvisioned);
    };
    if states.is_empty() || states.len() > MAX_PUT_BLOCKS {
        return refuse(Why::BlockCount {
            max: MAX_PUT_BLOCKS as u32,
            got: states.len() as u32,
        });
    }
    let mut puts = Vec::with_capacity(states.len());
    let mut contracts = Vec::with_capacity(states.len());
    for (i, st) in states.into_iter().enumerate() {
        let Some((&kind, body)) = st.split_first() else {
            return refuse(Why::NotABlock { index: i as u32 });
        };
        let id = freenet_prolly::block_id(kind, body);
        contracts.push(contract_keys::block::contract_for(&code, &id));
        puts.push((id, st));
    }
    (Answer::Putting { contracts }, puts)
}

fn sign<H: Host>(host: &mut H, prev: Head, next: Next) -> Answer {
    let (key, rcode, rparams, bcode) = (
        host.get_secret(KEY),
        host.get_secret(REGISTER_CODE),
        host.get_secret(REGISTER_PARAMS),
        host.get_secret(BLOCK_CODE),
    );
    let provisioned = key.is_some() && rcode.is_some() && rparams.is_some() && bcode.is_some();
    let record: Option<Record> = host
        .get_secret(RECORD)
        .and_then(|b| bincode::deserialize(&b).ok());
    let head_read = match (&rcode, &rparams) {
        (Some(c), Some(p)) => host
            .contract_state(&register_id(c, p))
            // TOLERANT (signer_proto::head): the root is the value's first 32
            // bytes, whatever ledger follows.
            .and_then(|st| {
                let (seq, v) = signer_proto::head::record_of(&st)?;
                Some(Head { seq, root: signer_proto::head::read_value(v)?.root })
            }),
        _ => None,
    };
    // Held AND the right block: the state is `kind ‖ body` and must hash to the root it is named by (as
    // entry.rs::block_state names a block), not merely be present.
    let root_held = bcode.as_deref().is_some_and(|c| {
        host.contract_state(&contract_keys::block::contract_for(c, &next.root))
            .is_some_and(|s| matches!(s.split_first(), Some((&k, body)) if freenet_prolly::block_id(k, body) == next.root))
    });
    let facts = Facts {
        provisioned,
        record,
        head_read,
        root_held,
    };
    match decide(&facts, &prev, &next) {
        Decision::Reply(a) => a,
        Decision::Sign => {
            let (Some(key), Some(params)) = (key, rparams) else {
                return Answer::Refused(Why::NotProvisioned);
            };
            let Ok(signed) =
                contract_keys::register::head_state(&params, &key, next.seq, &next.value())
            else {
                return Answer::Refused(Why::CannotSign);
            };
            let rec = Record {
                prev,
                next,
                signed: signed.clone(),
            };
            // THE ORDER: the record is written BEFORE the signature leaves. A reply without its record could be signed
            // again from the same prev after a restart -- two signatures at one seq, the fork this exists to prevent.
            let bytes = bincode::serialize(&rec).expect("a record encodes");
            if host.set_secret(RECORD, &bytes) {
                Answer::Signed(signed)
            } else {
                Answer::Refused(Why::RecordNotSaved)
            }
        }
    }
}

/// The record a signer keeps PER SITE: the last version it signed for `site:<app>`, which bundle, and the exact
/// bytes returned. One per label, so each app's versions are guarded on their own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRecord {
    pub version: u64,
    pub bundle: [u8; 32],
    pub signed: Vec<u8>,
}

/// Its secret-store name.
pub fn site_record_key(app: &str) -> Vec<u8> {
    [b"signer_site/".as_slice(), app.as_bytes()].concat()
}

/// The app id rule (the SDK's `app::check`, the site contract's label): 1-32 of `[a-z0-9_-]`.
pub fn app_id_ok(app: &str) -> bool {
    (1..=32).contains(&app.len()) && app.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// The site's params: this signer's Register params with the label `site:<app>` instead of its own. `None` unless
/// they are ONE key's (`RG01 ‖ 0 ‖ key ‖ label`): a signer holds one key.
pub fn site_params(register_params: &[u8], app: &str) -> Option<Vec<u8>> {
    let head = register_params.get(..4 + 1 + 32)?;
    (head.starts_with(b"RG01") && head[4] == 0).then(|| [head, b"site:".as_slice(), app.as_bytes()].concat())
}

/// THE SITE RULE, pure (the head's sign-if-next, per label): sign a version only AFTER the last one signed for
/// this site; the same version with the same bundle returns the same bytes; anything else says what was
/// recorded, so the page asks at `recorded + 1` and never loops on a version it cannot get (architect, #117).
pub fn decide_site(record: Option<&SiteRecord>, version: u64, bundle: &[u8; 32]) -> Result<(), Answer> {
    if version == 0 {
        return Err(Answer::Refused(Why::NotSuccessor));
    }
    match record {
        None => Ok(()),
        Some(r) if version > r.version => Ok(()),
        Some(r) if version == r.version && &r.bundle == bundle => Err(Answer::AlreadySigned(r.signed.clone())),
        Some(r) => Err(Answer::Refused(Why::SiteNotNext { recorded: r.version })),
    }
}

/// `SignSite`: refused from a served app (rule 13 until single sign-on); then the site rule; then sign, SAVE the
/// record, and only then reply -- a reply without its record could sign the version again (as `sign`'s order).
fn sign_site<H: Host>(host: &mut H, origin: Origin, app: &str, version: u64, bundle: [u8; 32]) -> Answer {
    if origin == Origin::WebApp {
        return Answer::Refused(Why::FromApp);
    }
    if !app_id_ok(app) {
        return Answer::Refused(Why::BadAppId);
    }
    let (Some(key), Some(rparams)) = (host.get_secret(KEY), host.get_secret(REGISTER_PARAMS)) else {
        return Answer::Refused(Why::NotProvisioned);
    };
    let Some(params) = site_params(&rparams, app) else {
        return Answer::Refused(Why::CannotSign);
    };
    let name = site_record_key(app);
    let record: Option<SiteRecord> = host.get_secret(&name).and_then(|b| bincode::deserialize(&b).ok());
    if let Err(a) = decide_site(record.as_ref(), version, &bundle) {
        return a;
    }
    let Ok(signed) = contract_keys::register::head_state(&params, &key, version, &bundle) else {
        return Answer::Refused(Why::CannotSign);
    };
    let rec = SiteRecord { version, bundle, signed: signed.clone() };
    if host.set_secret(&name, &bincode::serialize(&rec).expect("a site record encodes")) {
        Answer::Signed(signed)
    } else {
        Answer::Refused(Why::RecordNotSaved)
    }
}

#[cfg(feature = "freenet-main-delegate")]
mod delegate;
