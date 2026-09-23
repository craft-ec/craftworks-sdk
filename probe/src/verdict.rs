//! The cold-read probe's verdict, as a pure function of what was measured.
//!
//! Separate from the live run so every arm is exercised on demand: a stall that
//! heals (F52) is rare live, and a verdict only a rare event can reach is one
//! nobody has seen work.

/// How long one cold read is waited for before it is RED.
///
/// 100 s, not 60 (F52, sdk#173): a streamed GET response lost between two
/// nodes is healed by the node itself -- its stream wait times out 60 s after
/// the lost response was SENT, a second GET is served, and the page follows
/// (measured: stream failed at +63.7 s, page at +68.2 s). A 60 s window ended
/// every such run at the one moment the node could not yet have spoken.
pub const COLD_READ_MS: u128 = 100_000;

/// A cold read answered later than this STALLED and recovered. An unstalled
/// cold read was answered in 1-9 s in every one of 11 long-window runs.
pub const STALLED_AFTER_MS: u128 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every check passed and every cold read came back within `STALLED_AFTER_MS`.
    Green,
    /// Every check passed, but a cold read waited past `STALLED_AFTER_MS`: the
    /// slowest one, in whole seconds, and which request it was.
    StalledRecovered { secs: u64, req: u64 },
    /// Why, one line per failed check.
    Red(Vec<String>),
}

/// `answers`: each cold-read request sent, with how many ms after ITS send its
/// page came, or `None` if it never did. `other`: the red lines of every check
/// that is not about time (rows read, stranded effects, TESTs 2 and 3).
pub fn cold_read(answers: &[(u64, Option<u128>)], other: Vec<String>) -> Verdict {
    let mut red = Vec::new();
    if answers.is_empty() {
        red.push("no cold read was sent".to_string());
    }
    for (req, a) in answers {
        match a {
            Some(ms) if *ms <= COLD_READ_MS => {}
            Some(ms) => red.push(format!("req {req} NOT ANSWERED within {} s (its page came at {ms} ms)", COLD_READ_MS / 1000)),
            None => red.push(format!("req {req} NOT ANSWERED within {} s", COLD_READ_MS / 1000)),
        }
    }
    red.extend(other);
    if !red.is_empty() {
        return Verdict::Red(red);
    }
    let slowest = answers.iter().filter_map(|(r, a)| a.map(|ms| (ms, *r))).max();
    match slowest {
        Some((ms, req)) if ms > STALLED_AFTER_MS => Verdict::StalledRecovered { secs: (ms / 1000) as u64, req },
        _ => Verdict::Green,
    }
}

/// Where one write stands, from every reply seen so far: `None` while more is
/// coming, `Some(Ok(()))` once it is `Published`, `Some(Err(why))` once a
/// TERMINAL state came without it.
///
/// Terminal is [`protocol::WriteState::terminal`], not a list kept here: a
/// hand list missed `Unread` (sdk#283), and the live run then waited 120 s on
/// an answer that was already final and read as a stall.
pub fn write_outcome(id: u64, replies: &[protocol::Reply]) -> Option<Result<(), String>> {
    use protocol::{Reply, WriteState};
    fn peel(r: &Reply) -> &Reply {
        match r {
            Reply::Acked { body, .. } => body,
            r => r,
        }
    }
    let mut unread = None;
    let mut last = None;
    for r in replies.iter().map(peel) {
        match r {
            Reply::SessionWriteState { write_id, state, .. } | Reply::WriteState { write_id, state } if *write_id == id => {
                if *state == WriteState::Published {
                    return Some(Ok(()));
                }
                last = Some(*state);
            }
            Reply::Unread { write_id, key, .. } if *write_id == id => unread = Some(String::from_utf8_lossy(key).into_owned()),
            _ => {}
        }
    }
    let state = last.filter(|s| s.terminal())?;
    Some(Err(match unread {
        Some(key) => format!("write {id} ended {state:?} without Published: it changes {key:?}, which it did not read"),
        None => format!("write {id} ended {state:?} without Published"),
    }))
}
