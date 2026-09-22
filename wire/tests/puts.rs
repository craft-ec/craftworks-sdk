//! A PUT of a contract the app names (builder#104): its key as the node names
//! it, the node's answers matched to it BY THAT KEY, and nothing else's.

use freenet_stdlib::client_api::{ClientError, ContractError, ContractResponse, ErrorKind, HostResponse, RequestError};
use wire::puts::{contract, PutState, Puts};
use wire::{unframe, AckKind, Incoming, Reassembler};

const CODE: &[u8] = b"any code: the key hashes whatever it is given";

fn put_ack(key: freenet_stdlib::prelude::ContractKey) -> Vec<u8> {
    let r: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }));
    bincode::serialize(&r).expect("encodes")
}

fn put_refusal(key: freenet_stdlib::prelude::ContractKey, cause: &str) -> Vec<u8> {
    let e: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Put { key, cause: cause.to_string().into() })).into();
    bincode::serialize(&Err::<HostResponse, ClientError>(e)).expect("encodes")
}

/// The key `contract` returns is the one the node's ack NAMES — or no ack
/// could ever be matched — and, for the webapp contract, the address the
/// node serves the container under.
#[test]
fn the_key_is_the_one_the_nodes_ack_names_and_the_webapp_address() {
    let state = b"a container".to_vec();
    let params = wire::webapp::params(&state);
    let (key, c, s) = contract(CODE, &params, &state);
    assert_eq!(s.as_ref(), state.as_slice());
    assert_eq!(unframe(&mut Reassembler::new(), &put_ack(c.key())), Incoming::Ack(AckKind::Put(key.clone())));
    assert_eq!(key, wire::webapp::address(CODE, &state));
    // Params are part of the key: a different one is a different contract.
    assert_ne!(contract(CODE, b"other", &state).0, key);
}

/// A PUT the node refuses NAMING the contract is `PutFailed` with that key and
/// the node's cause. A GET refusal is still `GetFailed`, and one naming
/// nothing is still `Refused`.
#[test]
fn a_put_refusal_naming_its_contract_is_put_failed_with_its_key() {
    let (key, c, _) = contract(CODE, b"p", b"s");
    assert_eq!(
        unframe(&mut Reassembler::new(), &put_refusal(c.key(), "invalid put")),
        Incoming::PutFailed { key, said: "invalid put".into() }
    );
    let get: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Get { key: c.key(), cause: "x".into() })).into();
    let get = bincode::serialize(&Err::<HostResponse, ClientError>(get)).expect("encodes");
    assert!(matches!(unframe(&mut Reassembler::new(), &get), Incoming::GetFailed { .. }));
    let bare = bincode::serialize(&Err::<HostResponse, ClientError>(wire::_test::client_error("no"))).expect("encodes");
    assert!(matches!(unframe(&mut Reassembler::new(), &bare), Incoming::Refused(_)));
}

/// Answers settle the PUT they NAME. Someone else's is not consumed (so the
/// caller's other paths still see it) and changes nothing; a second answer
/// for one already settled is consumed and does not flip it.
#[test]
fn answers_settle_the_put_they_name_and_nothing_else() {
    let mut p = Puts::default();
    assert_eq!(p.state("a"), None);
    p.begin("a".into());
    p.begin("b".into());
    assert_eq!(p.state("a"), Some(&PutState::Pending));

    assert!(!p.acked("someone-else"), "a foreign ack was consumed");
    assert!(!p.refused("someone-else", "no"), "a foreign refusal was consumed");
    assert_eq!((p.state("a"), p.state("b")), (Some(&PutState::Pending), Some(&PutState::Pending)));

    assert!(p.acked("a"));
    assert_eq!((p.state("a"), p.state("b")), (Some(&PutState::Put), Some(&PutState::Pending)), "an ack settled the wrong put");
    assert!(p.refused("b", "invalid put"));
    assert_eq!(p.state("b"), Some(&PutState::Refused("invalid put".into())));

    assert!(p.acked("b"), "a duplicate answer to our own put was not consumed");
    assert_eq!(p.state("b"), Some(&PutState::Refused("invalid put".into())), "a late answer flipped a settled put");
    assert!(p.refused("a", "late"));
    assert_eq!(p.state("a"), Some(&PutState::Put));

    // A re-PUT starts over: its own answer is the one that counts.
    p.begin("b".into());
    assert_eq!(p.state("b"), Some(&PutState::Pending));
    assert!(p.acked("b"));
    assert_eq!(p.state("b"), Some(&PutState::Put));
}

/// A dropped socket: what was waiting is `Unanswered` — not pending (its
/// answer is gone with the old socket), not refused — and is still settled by
/// an answer on the new socket. What was already answered keeps its answer.
#[test]
fn a_dropped_connection_leaves_waiting_puts_unanswered_and_still_settleable() {
    let mut p = Puts::default();
    p.begin("waiting".into());
    p.begin("sent-later".into());
    p.begin("done".into());
    assert!(p.acked("done"));
    p.connection_lost();
    assert_eq!(p.state("waiting"), Some(&PutState::Unanswered));
    assert_eq!(p.state("done"), Some(&PutState::Put), "a settled put was reopened by a drop");
    assert!(p.acked("sent-later"));
    assert_eq!(p.state("sent-later"), Some(&PutState::Put), "an answer on the new socket did not settle it");
}
