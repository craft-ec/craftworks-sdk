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
    /// A request this build understands.
    Do(Request),
    /// Answer with this and do nothing else.
    Answer(Reply),
}

pub fn serve(bytes: &[u8]) -> Served {
    match protocol::decode_request(bytes) {
        Incoming::Ok(env) => Served::Do(env.body),
        Incoming::Unsupported(got) => Served::Answer(Reply::Unsupported {
            got,
            known: protocol::KNOWN.to_vec(),
        }),
        Incoming::Dropped(reason) => Served::Answer(Reply::Dropped { reason }),
    }
}
