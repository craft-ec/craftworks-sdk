//! THE ONE HEAD-JUDGEMENT (sdk#396): every register answer, whoever asked for it (the engine's recovery read, a
//! read-back, a verify, a hint), is judged HERE and nowhere else, before anything acts on it.
//!
//! The rule it holds: a head at a seq where THIS page has signed a record, under another root that this page's
//! record BEATS in the Register's equal-seq tie-break, is never adopted -- the register will hold mine once my UPDATE
//! merges there (the architect's attack on sdk#225, case 1). Before this module four sites asked the question and
//! one (the engine's mid-commit re-read) did not, and the page model caught it (safety gap (a), the 2026-09-24
//! stock-take).
//!
//! Enforced by construction: a head can be ADOPTED (`Event::HeadRead` / `Event::HeadConflict`) only from a
//! [`Heard`], and only [`judge`] makes one -- its fields are private to this module. A head this page's record beats
//! comes back as [`Judged::MineWins`], which carries no `Heard`.

use crate::{beats, HeadRead};
use freenet_prolly::Cid;
use std::collections::BTreeMap;

/// What a register answer IS, to this page.
pub(crate) enum Judged {
    /// The register holds no head.
    NoHead,
    /// A head this page may act on: adopt, confirm, compare. Never one this page's own record beats.
    Head(Heard),
    /// The register's head at `seq` is `root`, and THIS page's record at that seq is another root that WINS the
    /// tie-break: never adopted. `record` is mine, the bytes to land.
    MineWins { seq: u64, root: Cid, record: Vec<u8> },
}

impl Judged {
    /// The (seq, root) the register showed, whatever the verdict: for comparing, never for adopting.
    pub(crate) fn shown(&self) -> Option<(u64, Cid)> {
        match self {
            Judged::NoHead => None,
            Judged::Head(h) => Some(h.pair()),
            Judged::MineWins { seq, root, .. } => Some((*seq, *root)),
        }
    }
}

/// A head the judgement cleared: the only thing a head is adopted from. Made by [`judge`] alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Heard {
    seq: u64,
    root: Cid,
}

impl Heard {
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }
    pub(crate) fn pair(&self) -> (u64, Cid) {
        (self.seq, self.root)
    }
}

/// THE judgement of one register answer `h`, against this page's own signed records (`my_records`: seq -> (root,
/// record bytes)) and the head as just read (`last_head`, whose whole value the tie-break compares).
pub(crate) fn judge(h: Option<(u64, Cid)>, my_records: &BTreeMap<u64, (Cid, Vec<u8>)>, last_head: Option<&HeadRead>) -> Judged {
    let Some((seq, root)) = h else { return Judged::NoHead };
    match winning_record(seq, &root, my_records, last_head) {
        Some(record) => Judged::MineWins { seq, root, record },
        None => Judged::Head(Heard { seq, root }),
    }
}

/// THIS page's own record at `seq`, if it is another root than `root` and WINS the tie-break against it.
fn winning_record(seq: u64, root: &Cid, my_records: &BTreeMap<u64, (Cid, Vec<u8>)>, last_head: Option<&HeadRead>) -> Option<Vec<u8>> {
    let (mine_root, bytes) = my_records.get(&seq)?;
    if mine_root == root {
        return None;
    }
    // Theirs by its whole value, as just read (a bare root if it came some other way); mine by the value the
    // signer signed.
    let theirs = last_head.filter(|h| h.seq == seq && h.root() == *root).map_or_else(|| root.to_vec(), |h| h.value().to_vec());
    let mine = HeadRead::from_record(bytes)?;
    beats(mine.value(), &theirs).then(|| bytes.clone())
}

/// What a SITE's register read IS, to the site this page owes (the module table's Site column). A site adopts
/// nothing: the verdict only ends or continues this page's own publication.
pub(crate) enum SiteJudged {
    /// The register holds exactly my record: Published.
    Mine,
    /// Newer than mine, or the same version won by another bundle: Superseded at that version.
    Superseded(u64),
    /// Older, none, or mine wins the tie-break at my version: not merged yet.
    NotYet,
}

/// THE judgement of a site's register read `read` against the version (`seq`) and bundle hash (`value`) this page
/// signed there -- the same equal-seq tie-break as the head's.
pub(crate) fn judge_site(seq: u64, value: &[u8], read: Option<&HeadRead>) -> SiteJudged {
    match read {
        Some(h) if h.seq == seq && h.value() == value => SiteJudged::Mine,
        Some(h) if h.seq > seq || (h.seq == seq && !beats(value, h.value())) => SiteJudged::Superseded(h.seq),
        _ => SiteJudged::NotYet,
    }
}
