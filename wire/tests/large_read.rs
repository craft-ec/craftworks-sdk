//! A READ OVER 4 MiB IS READ (the builder's 5.9 MB site, engineer1 from the page's own lines): the node chunks a large
//! answer (stdlib's `chunk_response`, its own chunker), the page reassembles it, and it is the answer -- not
//! `TooLarge`, discarded and re-asked for ever.

use freenet_stdlib::client_api::streaming::chunk_response;
use freenet_stdlib::client_api::{ClientError, ContractResponse, HostResponse};
use freenet_stdlib::prelude::WrappedState;
use wire::{unframe, Incoming, Reassembler};

/// `state` answered to a GET of a site contract, framed as the node frames it: chunked, each chunk its own frame.
fn chunked_get(state: &[u8]) -> ([u8; 32], Vec<Vec<u8>>) {
    let (_, contract, _) = wire::puts::contract(b"site code", b"site params", &[]);
    let key = contract.key();
    let whole = bincode::serialize(&Ok::<HostResponse, ClientError>(HostResponse::ContractResponse(ContractResponse::GetResponse {
        key,
        contract: None,
        state: WrappedState::new(state.to_vec()),
    })))
    .unwrap();
    let frames = chunk_response(whole, 1).into_iter().map(|c| bincode::serialize(&Ok::<HostResponse, ClientError>(c)).unwrap()).collect();
    (key.id().as_bytes().try_into().expect("32 bytes"), frames)
}

#[test]
#[should_panic(expected = "a site read of 5.9 MB was not read")] // PINNED: flipped by the fix
fn a_site_read_over_four_mib_arrives_as_its_state() {
    let state: Vec<u8> = (0..5_900_000u32).map(|i| (i % 251) as u8).collect();
    let (id, frames) = chunked_get(&state);
    assert!(frames.len() > 1, "THE SETUP: the answer was not chunked");
    let mut r = Reassembler::new();
    let mut last = None;
    for f in &frames {
        last = Some(unframe(&mut r, f));
    }
    match last {
        Some(Incoming::Got { id: got, state: s }) => assert!(got == id && s == state, "the site read arrived as another answer"),
        other => panic!("a site read of 5.9 MB was not read: the last frame came out as {other:?}"),
    }
}
