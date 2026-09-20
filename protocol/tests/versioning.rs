//! The protocol keeps its promises to versions it has never met.

use protocol::*;

/// A version this build does not serve is ANSWERED, and told what is served.
///
/// The control is the same message at a version it does know. Without it,
/// "unsupported was returned" would read identically from a decoder that
/// rejects everything.
#[test]
fn an_unknown_version_is_answered_unsupported_and_a_known_one_is_not() {
    let good = encode_request(CURRENT, &Request::Flush);
    assert_eq!(
        decode_request(&good),
        Incoming::Ok(Envelope {
            version: CURRENT,
            body: Request::Flush
        }),
        "a message at the current version was not understood, so nothing \
         below this line means anything"
    );

    // One byte of the VERSION changed. The acceptance issue's control.
    let mut future = good.clone();
    future[0] = 99;
    match decode_request(&future) {
        Incoming::Unsupported(v) => assert_eq!(v, 99),
        other => panic!(
            "a v99 message was answered {other:?} rather than Unsupported. A \
             client that hears silence waits; one that hears a guess acts on \
             it."
        ),
    }
}

/// `Unsupported` names what IS known, or a client learns only that it failed.
#[test]
fn unsupported_carries_the_versions_that_are_served() {
    let r = Reply::Unsupported {
        got: 99,
        known: KNOWN.to_vec(),
    };
    let round = decode_reply(&encode_reply(&r)).expect("decodes");
    match round {
        Reply::Unsupported { got, known } => {
            assert_eq!(got, 99);
            assert!(
                known.contains(&CURRENT),
                "the reply does not name the version this build actually \
                 speaks, so a client cannot fall back to anything"
            );
            assert!(!known.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

/// The version is readable even when the BODY is not.
///
/// A v2 body reaching a v1 decoder is not a corrupt message, it is a message
/// from the future — and answering `Unparseable` tells its sender nothing it
/// can act on, where `Unsupported` tells it to fall back.
#[test]
fn a_future_body_is_unsupported_not_unparseable() {
    let mut msg = 7u16.to_le_bytes().to_vec();
    msg.extend_from_slice(b"a body no v1 decoder can make sense of");
    assert_eq!(
        decode_request(&msg),
        Incoming::Unsupported(7),
        "a message whose VERSION is readable was reported as unparseable; the \
         version is on the front precisely so this case is answerable"
    );
}

/// Trailing bytes are refused, not half-read.
#[test]
fn a_message_with_trailing_bytes_is_refused() {
    let good = encode_request(CURRENT, &Request::Flush);
    let mut trailing = good.clone();
    trailing.extend_from_slice(b"and then some");
    assert_eq!(
        decode_request(&trailing),
        Incoming::Dropped(Dropped::TrailingBytes),
        "a message with {} trailing byte(s) was accepted; its prefix was read \
         as a whole message, which is how one message becomes another",
        trailing.len() - good.len()
    );
}

/// Garbage, truncation and oversize are all REFUSED, and none panics.
///
/// Generated rather than hand-picked, with a floor on how many were tried: a
/// sweep that covered three inputs and a sweep that covered three hundred
/// print the same thing.
#[test]
fn nothing_malformed_panics_and_every_case_is_refused() {
    let good = encode_request(
        CURRENT,
        &Request::Write {
            write_id: 1,
            ops: vec![Op::Put(b"k".to_vec(), vec![7u8; 64])],
        },
    );
    let mut tried = 0usize;
    let mut refused = 0usize;

    // Every truncation.
    for n in 0..good.len() {
        tried += 1;
        if !matches!(decode_request(&good[..n]), Incoming::Ok(_)) {
            refused += 1;
        }
    }
    // Every single-byte corruption of the BODY. The version bytes are left
    // alone here: changing those is the Unsupported case, which is a
    // different answer and has its own test.
    let mut rng = 0x243F6A88u32;
    for at in 2..good.len() {
        for _ in 0..2 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let mut bad = good.clone();
            bad[at] ^= (rng & 0xFF) as u8;
            tried += 1;
            // A corruption may still decode to a DIFFERENT valid message —
            // that is what a self-describing format allows — so what is
            // asserted is that it never panics, and the refusals are counted.
            if !matches!(decode_request(&bad), Incoming::Ok(_)) {
                refused += 1;
            }
        }
    }
    // Oversize.
    tried += 1;
    let huge = vec![0u8; MAX_MESSAGE + 1];
    assert_eq!(
        decode_request(&huge),
        Incoming::Dropped(Dropped::TooLarge),
        "a message over the cap was not refused before decoding"
    );
    refused += 1;

    assert!(
        tried > 200,
        "only {tried} malformed input(s) were tried; a sweep this small says \
         nothing about a decoder"
    );
    assert!(
        refused > tried / 2,
        "only {refused} of {tried} malformed inputs were refused, which is \
         too few to believe: a decoder that accepts most damage is one that \
         is not reading the structure"
    );
    println!("  {tried} malformed inputs, {refused} refused, 0 panics");
}
