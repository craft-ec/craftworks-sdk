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
//!
//! [`decide`] is the rule, pure. [`serve`] is the whole request over a [`Host`] (secrets + the sync read), so every
//! ordering the issue names -- the race, identical re-ask, one signature per prev, a record that was not saved -- is
//! driven natively with nothing but a map. `delegate` (the entry the node calls) is `serve` over the real ctx.

use serde::{Deserialize, Serialize};

/// A head: `(seq, root)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub seq: u64,
    pub root: [u8; 32],
}

/// The head that follows `prev`: `seq` MUST be `prev.seq + 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Next {
    pub seq: u64,
    pub root: [u8; 32],
    /// WRITE-PATH's ledger. Opaque here: its encoding lands with WRITE-PATH step 2 (Q1 on sdk#209), and until then
    /// the rule "a ledger never lowers a session's published_through" cannot be checked and is NOT (stated, not
    /// hidden). It is signed as part of the value: `root` alone when empty -- exactly what every existing head reader
    /// (`register::head_of`) accepts -- else `root ‖ ledger`.
    pub ledger: Vec<u8>,
}

impl Next {
    pub fn head(&self) -> Head {
        Head {
            seq: self.seq,
            root: self.root,
        }
    }
    /// The Register value this head is signed as.
    pub fn value(&self) -> Vec<u8> {
        let mut v = self.root.to_vec();
        v.extend_from_slice(&self.ledger);
        v
    }
}

/// THE ONE RECORD the signer keeps: the last thing it signed, from which prev, and the exact bytes returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub prev: Head,
    pub next: Next,
    pub signed: Vec<u8>,
}

/// Why a request is refused outright, rather than answered with a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    /// `next.seq != prev.seq + 1`.
    NotSuccessor,
    /// No key (or no Register / Block naming) has been provisioned. Named, never a silence.
    NotProvisioned,
    /// The signer knows no head at all -- no record, and the Register is not readable locally -- and `prev` is not
    /// the genesis (seq 0). It cannot tell whether `prev` is current, so it does not sign.
    HeadUnknown,
    /// The ROOT BLOCK of `next.root` is not readable by the sync read: a head pointing at a tree the node does not
    /// hold would publish a hole.
    RootNotHeld,
    /// The record could not be written: NO signature is returned without its record.
    RecordNotSaved,
    /// A DIFFERENT key is already provisioned: a key is never replaced silently.
    KeyAlreadyProvisioned,
    /// The stored key cannot sign for the provisioned Register (not its writer, not mode 0, bad length).
    CannotSign,
    /// The request could not be decoded, or names a version this build does not speak.
    Unreadable,
}

/// The answer the page receives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// `next` follows the CURRENT head; the record is saved; these are the Register state bytes to UPDATE.
    Signed(Vec<u8>),
    /// `prev` is not the current head: rebase onto `current`.
    NotNext {
        current: Head,
    },
    /// `prev` has ALREADY been signed from: the FIRST record's bytes, whatever `next` asked this time. An identical
    /// re-ask is idempotent (the page re-UPDATEs the same bytes); a different next is told what won.
    AlreadySigned(Vec<u8>),
    /// A key and the naming are stored.
    Provisioned,
    Refused(Why),
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
/// (a) `next.seq == prev.seq + 1`, else `Refused(NotSuccessor)`; not provisioned → `Refused(NotProvisioned)`.
/// (b) AT MOST ONE SIGNATURE PER PREV: `record.prev == prev` → `AlreadySigned(record.signed)`, identical re-ask or a
///     different `next` alike. Checked BEFORE (c), so a re-ask after the head moved on still gets its own bytes back.
/// (c) TRUTH = the later of `record.next` and `head_read`, by seq; at EQUAL seq the RECORD (what THIS key signed; a
///     different root there is another holder of the key -- reported by the checkpoint, not resolved here).
///     `prev != truth` → `NotNext{ current: truth }`. No truth at all: only the genesis (`prev.seq == 0`) is signable,
///     else `Refused(HeadUnknown)`.
/// (d) the root block must be held, else `Refused(RootNotHeld)`.
/// Otherwise `Sign`.
pub fn decide(f: &Facts, prev: &Head, next: &Next) -> Decision {
    use Answer::*;
    if next.seq != prev.seq.wrapping_add(1) || prev.seq == u64::MAX {
        return Decision::Reply(Refused(Why::NotSuccessor));
    }
    if !f.provisioned {
        return Decision::Reply(Refused(Why::NotProvisioned));
    }
    if let Some(r) = &f.record {
        if r.prev == *prev {
            return Decision::Reply(AlreadySigned(r.signed.clone()));
        }
    }
    let signed_head = f.record.as_ref().map(|r| r.next.head());
    let truth = match (signed_head, f.head_read) {
        (Some(mine), Some(read)) if read.seq > mine.seq => Some(read),
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

/// The request as the page frames it: a VERSION byte, then bincode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Sign {
        prev: Head,
        next: Next,
    },
    Provision {
        signing_key: Vec<u8>,
        register_code: Vec<u8>,
        register_params: Vec<u8>,
        block_code: Vec<u8>,
    },
}

/// The only version this build speaks.
pub const VERSION: u8 = 1;

pub fn encode_request(r: &Request) -> Vec<u8> {
    let mut out = vec![VERSION];
    out.extend(bincode::serialize(r).expect("a request encodes"));
    out
}

pub fn encode_answer(a: &Answer) -> Vec<u8> {
    let mut out = vec![VERSION];
    out.extend(bincode::serialize(a).expect("an answer encodes"));
    out
}

pub fn decode_answer(bytes: &[u8]) -> Option<Answer> {
    match bytes.split_first() {
        Some((&VERSION, rest)) => bincode::deserialize(rest).ok(),
        _ => None,
    }
}

fn decode_request(bytes: &[u8]) -> Option<Request> {
    match bytes.split_first() {
        Some((&VERSION, rest)) => {
            // Bounded: a request is a head and a ledger, or a provision of two contracts' code.
            use bincode::Options;
            bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .allow_trailing_bytes()
                .with_limit(4 * 1024 * 1024)
                .deserialize(rest)
                .ok()
        }
        _ => None,
    }
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

/// One request, served: gather the facts, decide, and -- on `Sign` -- sign, save the record, reply.
pub fn serve<H: Host>(host: &mut H, request: &[u8]) -> Answer {
    let Some(req) = decode_request(request) else {
        return Answer::Refused(Why::Unreadable);
    };
    match req {
        Request::Provision {
            signing_key,
            register_code,
            register_params,
            block_code,
        } => {
            if let Some(held) = host.get_secret(KEY) {
                if held != signing_key {
                    return Answer::Refused(Why::KeyAlreadyProvisioned);
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
        Request::Sign { prev, next } => sign(host, prev, next),
    }
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
            .and_then(|st| {
                engine_delegate::register::record_of(&st).map(|(seq, v)| (seq, v.to_vec()))
            })
            .and_then(|(seq, v)| {
                v.get(..32).map(|r| Head {
                    seq,
                    root: r.try_into().expect("32"),
                })
            }),
        _ => None,
    };
    let root_held = bcode.as_deref().is_some_and(|c| {
        host.contract_state(&engine_delegate::blocks::contract_for(c, &next.root))
            .is_some_and(|s| !s.is_empty())
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
                engine_delegate::register::head_state(&params, &key, next.seq, &next.value())
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

#[cfg(feature = "freenet-main-delegate")]
mod delegate;
