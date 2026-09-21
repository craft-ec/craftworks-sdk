//! Serving the protocol: every version still in use, and a named refusal for
//! the rest.
//!
//! The shell's own `wire` module was an ad-hoc `Request`/`Reply` for a test
//! driver. This is the same conversation, but versioned — which is the
//! difference between a protocol that can grow and one that has to be
//! replaced.

use protocol::{Incoming, Reply, Request};

/// Read what a client sent, and say what to do about it.
///
/// Three outcomes, and the middle one is the point: a version this build
/// does not serve gets an ANSWER naming what it does serve. Silence leaves
/// the client waiting; a guess leaves it wrong.
pub enum Served {
    /// A request this build understands, and the version the CLIENT spoke.
    ///
    /// The version is carried, not discarded, because it is the only thing
    /// that says what the client can read back. A reply carries no version of
    /// its own — there is no envelope on the reply side — so "may I send this
    /// message?" can only be answered from what the client said on the way in.
    /// Dropping it here is what would make a v2-only message reach a v1
    /// reader.
    ///
    /// And the SESSION that sent it (v4; [`protocol::LEGACY_SESSION`] before):
    /// every tab shares this delegate, so the frame is the only place that
    /// says whose it is (craftworks-sdk#146).
    Do(Request, u16, u64),
    /// Answer with this and do nothing else.
    Answer(Reply),
}

pub fn serve(bytes: &[u8]) -> Served {
    match protocol::decode_request(bytes) {
        Incoming::Ok(env) => Served::Do(env.body, env.version, env.session),
        Incoming::Unsupported(got) => Served::Answer(Reply::Unsupported {
            got,
            known: protocol::KNOWN.to_vec(),
        }),
        Incoming::Dropped(reason) => Served::Answer(Reply::Dropped { reason }),
    }
}
