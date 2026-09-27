//! AN APP PUBLISH'S LIFE, AS ONE TABLE (sdk#516; craftworks-docs `docs/design/APP-PUBLISH.md` draft 2, the architect's
//! OK). ONE enum whose cases carry their data, ONE pure `step`, one writer BY TYPE ([`AppPublishState`]: a private
//! field whose only `&mut` method is `step`). The rules it holds:
//! - P1 (ordering): the site record is sent only once EVERY set it names has at least `k` pieces acked;
//! - P2: PUBLISHED = the site read back at this version;
//! - P3: BACKED_UP = PUBLISHED and every piece of every set acked;
//! - P6 (the site is read first): before any piece, the site is READ; if it already holds this exact bundle the app is
//!   PUBLISHED at that version, and no piece and no site is sent (backing is the repair pass's, not this page's);
//! - P5: a set that cannot reach `k` (more than `m` finally refused) ends the publish REFUSED before any site; a final
//!   refusal after the set has `k` is recorded and ends nothing (BACKED_UP never comes).
//!
//! What a refusal is FINAL for is page-io's one rule (`put_refused`, sdk#522); this table sees only final ones.
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]

use pieces::PieceSet;

/// One set's progress: its pieces' node keys (in the set's order) and which are acked or finally refused. Whether it
/// is REBUILDABLE (acked ≥ k) or LOST (refused > m) is derived, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetProgress {
    pub(crate) set: PieceSet,
    pub(crate) keys: Vec<String>,
    pub(crate) acked: Vec<bool>,
    pub(crate) refused: Vec<bool>,
}

impl SetProgress {
    pub(crate) fn new(set: PieceSet, keys: Vec<String>) -> SetProgress {
        let n = keys.len();
        SetProgress { set, keys, acked: vec![false; n], refused: vec![false; n] }
    }
    fn acked(&self) -> usize {
        self.acked.iter().filter(|a| **a).count()
    }
    fn refused(&self) -> usize {
        self.refused.iter().filter(|r| **r).count()
    }
    fn rebuildable(&self) -> bool {
        self.acked() >= self.set.k
    }
    fn lost(&self) -> bool {
        self.refused() > self.set.m
    }
    fn whole(&self) -> bool {
        self.acked.iter().all(|a| *a)
    }
    /// The keys still owed an answer (neither acked nor finally refused).
    fn pending(&self) -> impl Iterator<Item = &String> {
        self.keys.iter().zip(self.acked.iter().zip(&self.refused)).filter(|(_, (a, r))| !**a && !**r).map(|(k, _)| k)
    }
}

/// One set's progress, as the Session reports it: pieces acked and FINALLY refused, of how many, and its k.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetLine {
    pub name: String,
    pub acked: usize,
    pub refused: usize,
    pub of: usize,
    pub k: usize,
}

/// THE ONE ENUM (APP-PUBLISH.md). Each case carries its own data. Public to READ (the Session maps each case to its
/// status word, sdk#523's vocabulary, exhaustively); only [`AppPublishState::step`] writes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppPublish {
    /// The site is being READ (P6); nothing is sent yet.
    Checking { sets: Vec<SetProgress> },
    /// Pieces in flight; at least one set is below `k`; the site is NOT sent (P1).
    Pieces { sets: Vec<SetProgress> },
    /// Every set is at `k` or more; the site's publication is in flight (the page's `Publication`).
    Siting { sets: Vec<SetProgress> },
    /// The site is read back at `version`; some piece is still owed. From `Checking` (P6) `sets` is EMPTY: nothing
    /// was sent, nothing is owed, and BACKED_UP never comes from this publish.
    Published { version: u64, sets: Vec<SetProgress> },
    /// Every piece of every set is acked.
    BackedUp { version: u64 },
    /// PUBLISHED, then a person cancelled: the site is live, the remaining piece PUTs withdrawn, BACKED_UP will not
    /// come (the architect: the status never implies it is coming).
    BackupAbandoned { version: u64, sets: Vec<SetProgress> },
    /// A set cannot reach `k` (P5), or the site was refused: in words.
    Refused { why: String },
    /// Another publication of this site is live: reported, never overwritten.
    Superseded { version: u64 },
    /// A person cancelled before it was published.
    Cancelled,
}

/// What moves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublishEvent {
    /// The site's check read (P6): `Some(v)` it holds THIS bundle at `v`; `None` it does not (no site, another bundle).
    SiteRead(Option<u64>),
    /// A piece's PUT acked, by its node key.
    PieceAcked(String),
    /// A piece's PUT FINALLY refused (page-io's one rule), by its node key.
    PieceRefused(String),
    SitePublished(u64),
    SiteSuperseded(u64),
    SiteRefused(String),
    Cancel,
}

/// What page-io carries out, after the state is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublishEffect {
    /// PUT the pieces (P6: the site does not hold this bundle).
    SendPieces,
    /// Send the site (P1: every set is rebuildable).
    SendSite,
    /// Withdraw these pieces' PUTs (a cancel).
    WithdrawPieces(Vec<String>),
    /// Cancel the site's publication.
    CancelSite,
}

/// The cell an event landed in (the counts are pinned against APP-PUBLISH.md's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cell {
    Transition,
    /// Recorded in the case's data; the case is unchanged.
    Record,
    /// (a) An event nothing in this state could have caused: counted, never a panic.
    Impossible,
    /// (i) An answer after the end (a withdrawn PUT's, a late site read).
    Late,
    Nothing,
}

pub(crate) struct Step {
    pub next: Option<AppPublish>,
    pub effects: Vec<PublishEffect>,
    pub cell: Cell,
}

fn go(next: AppPublish, effects: Vec<PublishEffect>) -> Step {
    Step { next: Some(next), effects, cell: Cell::Transition }
}
fn record(next: AppPublish) -> Step {
    Step { next: Some(next), effects: Vec::new(), cell: Cell::Record }
}
fn stay(cell: Cell) -> Step {
    Step { next: None, effects: Vec::new(), cell }
}

/// Mark EVERY place `key` is listed -- in every set, at every position (identical pieces share a node key, and one
/// answer answers them all; `engine::repair::slots_of`, the one lookup) -- acked or finally refused; `None` if no set
/// names it.
fn mark(sets: &[SetProgress], key: &str, acked: bool) -> Option<Vec<SetProgress>> {
    let mut sets = sets.to_vec();
    let mut any = false;
    for s in &mut sets {
        let ixs: Vec<usize> = engine::repair::slots_of(&s.keys, key).collect();
        for i in ixs {
            any = true;
            if acked {
                s.acked[i] = true;
            } else {
                s.refused[i] = true;
            }
        }
    }
    any.then_some(sets)
}

fn pending(sets: &[SetProgress]) -> Vec<String> {
    sets.iter().flat_map(|s| s.pending().cloned()).collect()
}

impl AppPublish {
    /// THE TRANSITION FUNCTION (APP-PUBLISH.md's table). Pure.
    pub(crate) fn step(&self, ev: &PublishEvent) -> Step {
        use AppPublish as S;
        use PublishEffect as F;
        use PublishEvent as E;
        match (self, ev) {
            // ---- Checking: the site is read first; nothing is sent (P6) ----
            (S::Checking { .. }, E::SiteRead(Some(v))) => go(S::Published { version: *v, sets: Vec::new() }, vec![]),
            (S::Checking { sets }, E::SiteRead(None)) => go(S::Pieces { sets: sets.clone() }, vec![F::SendPieces]),
            (S::Checking { .. }, E::PieceAcked(_) | E::PieceRefused(_) | E::SitePublished(_) | E::SiteSuperseded(_) | E::SiteRefused(_)) => stay(Cell::Impossible),
            (S::Checking { .. }, E::Cancel) => go(S::Cancelled, vec![F::CancelSite]),
            (S::Pieces { .. } | S::Siting { .. } | S::Published { .. }, E::SiteRead(_)) => stay(Cell::Late),
            // ---- Pieces: the site is not sent (P1) ----
            (S::Pieces { sets }, E::PieceAcked(key)) => match mark(sets, key, true) {
                Some(sets) if sets.iter().all(SetProgress::rebuildable) => go(S::Siting { sets }, vec![F::SendSite]),
                Some(sets) => record(S::Pieces { sets }),
                None => stay(Cell::Impossible),
            },
            (S::Pieces { sets }, E::PieceRefused(key)) => match mark(sets, key, false) {
                Some(sets) => match sets.iter().find(|s| s.lost()) {
                    Some(lost) => go(S::Refused { why: format!("set {} cannot reach k {}: more than {} of its pieces were refused", lost.set.name, lost.set.k, lost.set.m) }, vec![F::WithdrawPieces(pending(&sets))]),
                    None => record(S::Pieces { sets }),
                },
                None => stay(Cell::Impossible),
            },
            (S::Pieces { .. }, E::SitePublished(_) | E::SiteSuperseded(_) | E::SiteRefused(_)) => stay(Cell::Impossible),
            (S::Pieces { sets }, E::Cancel) => go(S::Cancelled, vec![F::WithdrawPieces(pending(sets))]),
            // ---- Siting: every set is rebuildable ----
            (S::Siting { sets }, E::PieceAcked(key)) => match mark(sets, key, true) {
                Some(sets) => record(S::Siting { sets }),
                None => stay(Cell::Impossible),
            },
            // A set cannot be LOST here, by count: acked ≥ k ⇒ refused ≤ n − k = m (the architect's note).
            (S::Siting { sets }, E::PieceRefused(key)) => match mark(sets, key, false) {
                Some(sets) => record(S::Siting { sets }),
                None => stay(Cell::Impossible),
            },
            (S::Siting { sets }, E::SitePublished(v)) => {
                if sets.iter().all(SetProgress::whole) {
                    go(S::BackedUp { version: *v }, vec![])
                } else {
                    go(S::Published { version: *v, sets: sets.clone() }, vec![])
                }
            }
            (S::Siting { sets }, E::SiteSuperseded(v)) => go(S::Superseded { version: *v }, vec![F::WithdrawPieces(pending(sets))]),
            (S::Siting { sets }, E::SiteRefused(w)) => go(S::Refused { why: w.clone() }, vec![F::WithdrawPieces(pending(sets))]),
            (S::Siting { sets }, E::Cancel) => go(S::Cancelled, vec![F::CancelSite, F::WithdrawPieces(pending(sets))]),
            // ---- Published: the site is live; pieces owed ----
            (S::Published { version, sets }, E::PieceAcked(key)) => match mark(sets, key, true) {
                Some(sets) if sets.iter().all(SetProgress::whole) => go(S::BackedUp { version: *version }, vec![]),
                Some(sets) => record(S::Published { version: *version, sets }),
                None => stay(Cell::Impossible),
            },
            (S::Published { version, sets }, E::PieceRefused(key)) => match mark(sets, key, false) {
                Some(sets) => record(S::Published { version: *version, sets }),
                None => stay(Cell::Impossible),
            },
            (S::Published { .. }, E::SitePublished(_) | E::SiteSuperseded(_) | E::SiteRefused(_)) => stay(Cell::Late),
            (S::Published { version, sets }, E::Cancel) => go(S::BackupAbandoned { version: *version, sets: sets.clone() }, vec![F::WithdrawPieces(pending(sets))]),
            // ---- the ends ----
            (
                S::BackedUp { .. } | S::BackupAbandoned { .. } | S::Refused { .. } | S::Superseded { .. } | S::Cancelled,
                E::SiteRead(_) | E::PieceAcked(_) | E::PieceRefused(_) | E::SitePublished(_) | E::SiteSuperseded(_) | E::SiteRefused(_),
            ) => stay(Cell::Late),
            (S::BackedUp { .. } | S::BackupAbandoned { .. } | S::Refused { .. } | S::Superseded { .. } | S::Cancelled, E::Cancel) => stay(Cell::Nothing),
        }
    }

    /// The live version, where there is one.
    pub fn version(&self) -> Option<u64> {
        match self {
            AppPublish::Published { version, .. } | AppPublish::BackedUp { version } | AppPublish::BackupAbandoned { version, .. } | AppPublish::Superseded { version } => Some(*version),
            AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::Siting { .. } | AppPublish::Refused { .. } | AppPublish::Cancelled => None,
        }
    }

    /// Each set's line (acked, finally refused, of, k), where sets are still tracked.
    pub fn set_lines(&self) -> Vec<SetLine> {
        self.sets().iter().map(|p| SetLine { name: p.set.name.clone(), acked: p.acked(), refused: p.refused(), of: p.keys.len(), k: p.set.k }).collect()
    }

    fn sets(&self) -> &[SetProgress] {
        match self {
            AppPublish::Checking { sets } | AppPublish::Pieces { sets } | AppPublish::Siting { sets } | AppPublish::Published { sets, .. } | AppPublish::BackupAbandoned { sets, .. } => sets,
            AppPublish::BackedUp { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => &[],
        }
    }

    /// What it says in words: a refusal's, and -- PUBLISHED with a piece FINALLY refused after k (P5) -- that
    /// BACKED_UP will not come, naming the sets (the architect's R4: the status never implies it is coming).
    pub fn said(&self) -> Option<String> {
        match self {
            AppPublish::Refused { why } => Some(why.clone()),
            AppPublish::Published { sets, .. } => {
                let short: Vec<String> = sets.iter().filter(|s| s.refused() > 0).map(|s| format!("set {}: {} of {} pieces refused", s.set.name, s.refused(), s.keys.len())).collect();
                (!short.is_empty()).then(|| format!("published, and BACKED_UP will not come: {}", short.join("; ")))
            }
            AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::Siting { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => None,
        }
    }

    /// Does this publish still OWE `key` an answer (a piece of a set it tracks, neither acked nor refused)? A shared
    /// piece is withdrawn only when no publish owes it (the architect's R1).
    pub(crate) fn owes(&self, key: &str) -> bool {
        match self {
            AppPublish::Pieces { sets } | AppPublish::Siting { sets } | AppPublish::Published { sets, .. } => sets.iter().any(|s| s.pending().any(|k| k == key)),
            // Checking: nothing is sent yet, so nothing is owed an answer.
            AppPublish::Checking { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => false,
        }
    }

    /// Every piece key this publish still owes an answer.
    pub(crate) fn owed_keys(&self) -> Vec<String> {
        self.sets().iter().flat_map(|s| s.pending().cloned()).collect::<Vec<_>>().into_iter().filter(|k| self.owes(k)).collect()
    }

    /// An END that owes nothing (its routing may go).
    pub(crate) fn ended(&self) -> bool {
        match self {
            AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => true,
            AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::Siting { .. } | AppPublish::Published { .. } => false,
        }
    }

    /// Is this publish still going (pieces or site in flight)? A second publish of the app is refused meanwhile.
    pub(crate) fn in_flight(&self) -> bool {
        match self {
            AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::Siting { .. } => true,
            AppPublish::Published { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => false,
        }
    }

    /// Is the site's CHECK read in flight (P6)?
    pub(crate) fn checking(&self) -> bool {
        match self {
            AppPublish::Checking { .. } => true,
            AppPublish::Pieces { .. } | AppPublish::Siting { .. } | AppPublish::Published { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => false,
        }
    }

    /// Is the site's publication in flight (the only state whose site answers mean something)?
    pub(crate) fn siting(&self) -> bool {
        match self {
            AppPublish::Siting { .. } => true,
            AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::Published { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled => false,
        }
    }
}

/// ONE WRITER, BY TYPE: an app publish's state, private; it starts only as [`AppPublishState::start`] and changes only
/// by [`AppPublishState::step`].
pub(crate) struct AppPublishState(AppPublish);

impl AppPublishState {
    /// A new publish: the site is read first (P6); its sets' pieces are PUT only if it does not hold this bundle.
    pub(crate) fn start(sets: Vec<SetProgress>) -> Self {
        AppPublishState(AppPublish::Checking { sets })
    }
    pub(crate) fn get(&self) -> &AppPublish {
        &self.0
    }
    /// THE ONE WRITER: the table's step, applied. Returns its effects and cell.
    pub(crate) fn step(&mut self, ev: &PublishEvent) -> (Vec<PublishEffect>, Cell) {
        let s = self.0.step(ev);
        if let Some(next) = s.next {
            self.0 = next;
        }
        (s.effects, s.cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pieces::NamedPiece;

    fn set(name: &str, k: usize, m: usize) -> SetProgress {
        let pieces = (0..k + m).map(|i| NamedPiece { address: format!("{name}-{i}"), sha256: [i as u8; 32] }).collect();
        SetProgress::new(PieceSet { name: name.into(), k, m, pieces }, (0..k + m).map(|i| format!("{name}-{i}")).collect())
    }

    /// ONE KEY IN TWO PLACES (the architect on sdk#542: first-match by id is the defect twice): identical pieces are
    /// one node key, so a set can list it twice and two sets can share it -- an answer for that key answers EVERY
    /// place. Acked: both of the set's slots and the other set's are acked; refused: likewise.
    #[test]
    fn an_answer_for_a_repeated_key_marks_every_place_it_is_listed() {
        let mut a = set("a", 2, 1);
        a.keys[2] = a.keys[0].clone();
        let mut b = set("b", 2, 1);
        b.keys[1] = a.keys[0].clone();
        let key = a.keys[0].clone();
        for acked in [true, false] {
            let sets = mark(&[a.clone(), b.clone()], &key, acked).expect("a set names the key");
            let marks = |s: &SetProgress| if acked { s.acked.clone() } else { s.refused.clone() };
            assert_eq!((marks(&sets[0]), marks(&sets[1])), (vec![true, false, true], vec![false, true, false]), "an answer for a repeated key marked only its first place (acked = {acked})");
        }
    }

    /// `n` of `s`'s pieces acked, `r` refused (from the end).
    fn with(mut s: SetProgress, n: usize, r: usize) -> SetProgress {
        for i in 0..n {
            s.acked[i] = true;
        }
        let len = s.keys.len();
        for i in 0..r {
            s.refused[len - 1 - i] = true;
        }
        s
    }

    /// One state per ROW of APP-PUBLISH.md's table, with data so the sample event takes the cell's arrow: a set one
    /// ack short of k (so an ack completes it), and one refusal short of losing it.
    fn rows() -> Vec<(&'static str, AppPublish)> {
        let near = || vec![with(set("core", 3, 2), 2, 2)];
        let whole_but_one = || vec![with(set("core", 3, 2), 4, 0)];
        vec![
            ("Checking", AppPublish::Checking { sets: near() }),
            ("Pieces", AppPublish::Pieces { sets: near() }),
            ("Siting", AppPublish::Siting { sets: whole_but_one() }),
            ("Published", AppPublish::Published { version: 2, sets: whole_but_one() }),
            ("BackedUp", AppPublish::BackedUp { version: 2 }),
            ("BackupAbandoned", AppPublish::BackupAbandoned { version: 2, sets: whole_but_one() }),
            ("Refused", AppPublish::Refused { why: "w".into() }),
            ("Superseded", AppPublish::Superseded { version: 3 }),
            ("Cancelled", AppPublish::Cancelled),
        ]
    }

    /// The row's sample events: the ack goes to the one owed piece (`core-2` for Pieces' near set, `core-4` for the
    /// whole-but-one sets), the refusal to one still owed.
    fn events(row: &str) -> Vec<PublishEvent> {
        let (ack, refuse) = match row {
            "Pieces" => ("core-2", "core-2"),
            _ => ("core-4", "core-4"),
        };
        vec![
            PublishEvent::SiteRead(Some(2)),
            PublishEvent::PieceAcked(ack.into()),
            PublishEvent::PieceRefused(refuse.into()),
            PublishEvent::SitePublished(2),
            PublishEvent::SiteSuperseded(3),
            PublishEvent::SiteRefused("w".into()),
            PublishEvent::Cancel,
        ]
    }

    /// EVERY CELL IS DECIDED, and the counts are APP-PUBLISH.md's (docs 5d43310, P6: 9 × 7 = 63): transitions 11,
    /// record-only 3, impossible 8, late 36, nothing 5. The test prints the table, row by row.
    #[test]
    fn every_publish_cell_is_decided_and_the_counts_are_the_documents() {
        let mut counts = std::collections::BTreeMap::new();
        println!("| state \\ event | SiteRead | PieceAcked | PieceRefused | SitePublished | SiteSuperseded | SiteRefused | Cancel |");
        for (name, state) in rows() {
            let cells: Vec<String> = events(name)
                .iter()
                .map(|e| {
                    let s = state.step(e);
                    *counts.entry(s.cell).or_insert(0usize) += 1;
                    match (&s.cell, &s.next) {
                        (Cell::Transition, Some(n)) => format!("→ {}", format!("{n:?}").split([' ', '{']).next().unwrap_or_default()),
                        (c, _) => format!("{c:?}"),
                    }
                })
                .collect();
            println!("| **{name}** | {} |", cells.join(" | "));
        }
        println!("{counts:?}");
        let n = |c| counts.get(&c).copied().unwrap_or(0);
        assert_eq!([n(Cell::Transition), n(Cell::Record), n(Cell::Impossible), n(Cell::Late), n(Cell::Nothing)], [11, 3, 8, 36, 5], "a cell changed: APP-PUBLISH.md's table changes with it");
    }

    use crate::seeded::Rng;

    /// THE MODEL: 300 seeded publishes of two sets (k 3 + m 2, k 2 + m 2), driven by a REFERENCE world. Each owed piece
    /// is answered once (acked, or FINALLY refused with a small chance); the site answers only after it was SENT, and
    /// once; a person may cancel. After EVERY step: P1 (the site is sent only when every set is rebuildable, and never
    /// twice), P3 (BACKED_UP only when every piece is acked), P5 (a set lost before the site ends it Refused; one
    /// after is recorded only), and no impossible cell. Its reach is floored on the cells its checks are about.
    #[test]
    fn the_publish_model_holds_p1_p3_p5_on_every_step() {
        let (mut sent_sites, mut lost_before, mut refused_after, mut backed, mut abandoned, mut current) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let sets = vec![set("core", 3, 2), set("prov", 2, 2)];
            let mut owed: Vec<String> = sets.iter().flat_map(|s| s.keys.clone()).collect();
            let mut st = AppPublishState::start(sets);
            // P6: the site is read first. A site already holding this bundle -> Published, NOTHING sent or owed.
            let holds = rng.pick(5) == 0;
            let (effects, cell) = st.step(&PublishEvent::SiteRead(holds.then_some(1)));
            assert_eq!(cell, Cell::Transition, "seed {seed}: the check read did not move the publish");
            if holds {
                assert_eq!(st.get(), &AppPublish::Published { version: 1, sets: Vec::new() }, "seed {seed}: P6 -- a current site was not published at its version");
                assert!(effects.is_empty() && st.get().owed_keys().is_empty(), "seed {seed}: P6 -- a current site sent or owed something: {effects:?}");
                current += 1;
                continue;
            }
            assert_eq!(effects, vec![PublishEffect::SendPieces], "seed {seed}: P6 -- a site that does not hold this bundle did not send its pieces");
            let (mut site_sent, mut site_answered) = (false, false);
            for i in 0..60 {
                let before = st.get().clone();
                let ev = match rng.pick(10) {
                    0 if rng.pick(4) == 0 => PublishEvent::Cancel,
                    1 | 2 if site_sent && !site_answered => {
                        site_answered = true;
                        match rng.pick(6) {
                            0 => PublishEvent::SiteSuperseded(9),
                            1 => PublishEvent::SiteRefused("the signer said no".into()),
                            _ => PublishEvent::SitePublished(2),
                        }
                    }
                    _ if !owed.is_empty() => {
                        let key = owed.remove(rng.pick(owed.len()));
                        if rng.pick(6) == 0 { PublishEvent::PieceRefused(key) } else { PublishEvent::PieceAcked(key) }
                    }
                    _ => continue,
                };
                let (effects, cell) = st.step(&ev);
                let now = st.get().clone();
                let at = format!("seed {seed} step {i}: {before:?} x {ev:?}");
                assert_ne!(cell, Cell::Impossible, "{at}: a real world's event landed in an impossible cell");
                if effects.contains(&PublishEffect::SendSite) {
                    assert!(!site_sent, "{at}: the site was sent twice");
                    let AppPublish::Siting { sets } = &now else { panic!("{at}: the site was sent from {now:?}") };
                    assert!(sets.iter().all(SetProgress::rebuildable), "{at}: P1 -- the site was sent before every set reached k");
                    site_sent = true;
                    sent_sites += 1;
                }
                if matches!(now, AppPublish::BackedUp { .. }) && !matches!(before, AppPublish::BackedUp { .. }) {
                    backed += 1;
                    let all: Vec<SetProgress> = match &before {
                        AppPublish::Siting { sets } | AppPublish::Published { sets, .. } => mark(sets, match &ev { PublishEvent::PieceAcked(k) => k, PublishEvent::SiteRead(_) | PublishEvent::PieceRefused(_) | PublishEvent::SitePublished(_) | PublishEvent::SiteSuperseded(_) | PublishEvent::SiteRefused(_) | PublishEvent::Cancel => "" }, true).unwrap_or_else(|| sets.clone()),
                        other @ (AppPublish::Checking { .. } | AppPublish::Pieces { .. } | AppPublish::BackedUp { .. } | AppPublish::BackupAbandoned { .. } | AppPublish::Refused { .. } | AppPublish::Superseded { .. } | AppPublish::Cancelled) => panic!("{at}: BACKED_UP from {other:?}"),
                    };
                    assert!(all.iter().all(SetProgress::whole), "{at}: P3 -- BACKED_UP with a piece owed");
                }
                if let PublishEvent::PieceRefused(_) = ev {
                    match (&before, &now) {
                        (AppPublish::Pieces { .. }, AppPublish::Refused { .. }) => lost_before += 1,
                        (AppPublish::Siting { .. } | AppPublish::Published { .. }, _) => {
                            refused_after += 1;
                            assert!(matches!(now, AppPublish::Siting { .. } | AppPublish::Published { .. }), "{at}: P5 -- a refusal after k ended the publish");
                        }
                        _ => {}
                    }
                }
                if matches!(now, AppPublish::BackupAbandoned { .. }) && !matches!(before, AppPublish::BackupAbandoned { .. }) {
                    abandoned += 1;
                }
                // A set that is LOST while pieces are in flight ends it Refused, before any site (P5).
                if let AppPublish::Pieces { sets } = &now {
                    assert!(!sets.iter().any(SetProgress::lost), "{at}: P5 -- a lost set left the publish going");
                }
            }
        }
        println!("reached: current (nothing sent) {current}, sites sent {sent_sites}, lost before the site {lost_before}, refused after k {refused_after}, backed up {backed}, backup abandoned {abandoned}");
        assert!(current > 0 && sent_sites > 0 && lost_before > 0 && refused_after > 0 && backed > 0 && abandoned > 0, "the model never reached a cell its checks are about");
    }
}
