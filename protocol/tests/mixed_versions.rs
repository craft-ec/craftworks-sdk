//! What happens when the two sides of the wire disagree about what exists.
//!
//! v2 adds `Reply::CallBytes`. Both directions are tested, because they fail
//! differently and only one of them is loud.
//!
//! The failure that matters is not a crash. It is a reader that decodes
//! something into the WRONG THING and carries on — the same silent
//! re-interpretation as a data file whose meaning changed while every field
//! stayed where it was.

use protocol::{decode_reply, decode_request, encode_reply, encode_request, Dropped, Incoming};
use protocol::{Reply, Request};

/// NEWER WRITER, OLDER READER: a message from the future must FAIL to decode,
/// not decode into something else.
///
/// This is the whole reason `CallBytes` is a new VARIANT and not new fields on
/// `Reply::Call`. Appending a field changes an existing message: a reader that
/// does not know about it does not error, it reads a different message.
/// Appending a variant gives it a new wire tag, and an unknown tag cannot be
/// mistaken for a known one.
#[test]
fn a_reply_from_a_newer_build_is_refused_rather_than_misread() {
    let mut bytes = encode_reply(&Reply::CallBytes {
        put_bytes: 101_641,
        puts: 2,
        gets: 1,
    });
    // bincode writes the variant index at the front. Push it past every
    // variant this build has — which is exactly what an older reader sees.
    bytes[0] = bytes[0].wrapping_add(50);

    match decode_reply(&bytes) {
        Err(Dropped::Unparseable) | Err(Dropped::TrailingBytes) => {}
        Err(other) => panic!("refused, but for the wrong reason: {other:?}"),
        Ok(r) => panic!(
            "a message from a newer build decoded as an EXISTING variant. \
             That is the silent misreading the variant scheme exists to \
             prevent: {r:?}"
        ),
    }
}

/// OLDER WRITER, NEWER READER: every message that existed before still decodes
/// to exactly what it was.
///
/// The half that would break silently if someone INSERTED a variant rather
/// than appending one — nothing about the new message would look wrong, and
/// every older one would quietly become its neighbour.
#[test]
fn every_older_reply_still_decodes_to_itself() {
    let olds = [
        Reply::Unsupported {
            got: 99,
            known: protocol::KNOWN.to_vec(),
        },
        Reply::Dropped {
            reason: Dropped::TooLarge,
        },
    ];
    for r in olds {
        let round = decode_reply(&encode_reply(&r)).expect("an older reply still decodes");
        assert_eq!(round, r, "a variant was renumbered by the append");
    }
}

/// A v1 REQUEST is still served, and is answered at the version it asked in.
///
/// Bumping CURRENT must not strand a client that has not been updated: v1 is
/// still in KNOWN, so a v1 request is understood rather than refused.
#[test]
fn a_v1_request_is_still_understood_after_the_bump() {
    assert!(protocol::KNOWN.contains(&1), "v1 must remain served");
    assert_eq!(protocol::CURRENT, 2);

    let v1 = encode_request(1, &Request::Flush);
    match decode_request(&v1) {
        Incoming::Ok(env) => {
            assert_eq!(env.version, 1, "the version is READ, not assumed");
            assert_eq!(env.body, Request::Flush);
        }
        other => panic!("a v1 request was not understood: {other:?}"),
    }
}

/// And a version nobody serves is still answered rather than guessed at.
#[test]
fn an_unserved_version_is_still_refused_with_the_list() {
    let mut future = encode_request(protocol::CURRENT, &Request::Flush);
    future[0] = 99;
    match decode_request(&future) {
        Incoming::Unsupported(v) => assert_eq!(v, 99),
        other => panic!("a v99 request was answered {other:?} rather than Unsupported"),
    }
}

/// The new message round-trips, so the tests above are not passing because
/// nothing was added.
#[test]
fn the_new_reply_round_trips() {
    let r = Reply::CallBytes {
        put_bytes: 101_641,
        puts: 2,
        gets: 1,
    };
    assert_eq!(decode_reply(&encode_reply(&r)).expect("decodes"), r);
}
