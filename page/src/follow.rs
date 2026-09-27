//! A SITE'S FOLLOW (sdk#520; H4, sdk#533; craftworks-docs PUBLISH-LIFE.md "A site's FOLLOW"): a machine of its own,
//! BESIDE the site's publication `Life`, one per followed site. A REOPEN follows its site: it reads what is live and
//! shows the node's version, and a newer one from another device -- and it keeps doing so through and after this
//! page's own publish (P8), because it is a holder of the site's read, never a state of the publish.
//!
//! ONE enum, ONE pure transition ([`step`]), one writer BY TYPE ([`Follows`]: a private map whose only `&mut` method is
//! [`Follows::on`]). Its ONLY act is a read (P7, by type: no value, so nothing can be signed, written or PUT from it).

// No catch-all over a state or an event: a new case fails the build until the table has its cell.
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]

use crate::publication::backoff;
use crate::HeadRead;
use std::collections::BTreeMap;

/// A followed site (the doc's enum; no follow = not in the map).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Follow {
    /// The node's answer not had yet. `at`: the next read after a NotFound (never "absent", P7). `floor`: the highest
    /// version this follow has shown (0: none) -- a STORED primary fact (Codex on #543, 4; the architect): a lagging
    /// node's older answer is "not yet", never shown.
    Reading { tries: u32, at: Option<u64>, floor: u64 },
    /// The node shows `version` live (itself the floor).
    Showing { version: u64 },
    /// The site holds a record that is not a site's: the one real refusal a read sees. `floor` is kept through it.
    Refused { why: String, floor: u64 },
}

/// The follow's events (PUBLISH-LIFE E13, E9, E10, E14).
pub(crate) enum FollowEv<'a> {
    /// E13: a reopen follows the site.
    Follow,
    /// E9: a read of the site's register (a GET's answer, or the node's full push): `None` is NotFound.
    Read(Option<&'a HeadRead>),
    /// E10: the follow's backoff came due.
    Due,
    /// E14: THIS page's publish of the site ended at `v` (Published or Superseded): the node's word too.
    Published(u64),
}

/// What the page does for a follow: its ONLY acts, both about its read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowAct {
    /// Read the site (a GET with subscribe), joining one already out.
    Read,
    /// The follow no longer holds its read (a Refused follow): only ITS waiter leaves.
    EndRead,
}

/// Where an event landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FollowCell {
    Transition,
    Nothing,
    /// An event nothing in this state could have caused (¹³, ²): counted, never a panic.
    Impossible,
}

/// The version a site's record shows, or its refusal: a record that is not a site's.
fn version_of(h: &HeadRead) -> Result<u64, String> {
    <[u8; 32]>::try_from(h.value()).map(|_| h.seq).map_err(|_| "the site holds a record that is not a site's".to_string())
}

/// THE TRANSITION (PUBLISH-LIFE's FOLLOW table), pure: from `f` (`None`: not followed), the next state (`None`: it
/// stays), its acts, and the cell.
pub(crate) fn step(f: Option<&Follow>, ev: FollowEv<'_>, now: u64) -> (Option<Follow>, Vec<FollowAct>, FollowCell) {
    use Follow as F;
    use FollowAct::{EndRead, Read};
    use FollowCell::{Impossible, Nothing, Transition};
    let go = |next: Follow, acts: Vec<FollowAct>| (Some(next), acts, Transition);
    let nothing = || (None, Vec::new(), Nothing);
    let impossible = || (None, Vec::new(), Impossible);
    // A read at or below the floor is "not yet" (a lagging node): read again on the backoff, as a NotFound is.
    let not_yet = |tries: u32, floor: u64| go(F::Reading { tries: tries + 1, at: Some(now + backoff(tries + 1)), floor }, vec![]);
    match (f, ev) {
        // ---- not followed ----
        (None, FollowEv::Follow) => go(F::Reading { tries: 0, at: None, floor: 0 }, vec![Read]),
        // ¹³: no follow holds no read; ²: no follow keeps no clock.
        (None, FollowEv::Read(_) | FollowEv::Due) => impossible(),
        (None, FollowEv::Published(_)) => nothing(),
        // ---- Reading ----
        (Some(F::Reading { .. }), FollowEv::Follow) => nothing(),
        (Some(F::Reading { tries, floor, .. }), FollowEv::Read(read)) => match read.map(version_of) {
            Some(Ok(version)) if version > *floor => go(F::Showing { version }, vec![]),
            // A lagging node's answer, at or below what this follow has shown: not yet.
            Some(Ok(_)) => not_yet(*tries, *floor),
            Some(Err(why)) => go(F::Refused { why, floor: *floor }, vec![EndRead]),
            // NotFound: read again on the backoff (P7: never "absent").
            None => not_yet(*tries, *floor),
        },
        (Some(F::Reading { tries, at: Some(at), floor }), FollowEv::Due) if *at <= now => go(F::Reading { tries: *tries, at: None, floor: *floor }, vec![Read]),
        (Some(F::Reading { .. }), FollowEv::Due) => nothing(),
        (Some(F::Reading { floor, .. }), FollowEv::Published(version)) => go(F::Showing { version: version.max(*floor) }, vec![]),
        // ---- Showing ----
        (Some(F::Showing { .. }), FollowEv::Follow) => nothing(),
        (Some(F::Showing { version: v }), FollowEv::Read(read)) => match read.map(version_of) {
            // Another device's publish, shown (the node's word); an older or equal one, or NotFound, changes nothing.
            Some(Ok(version)) if version > *v => go(F::Showing { version }, vec![]),
            Some(Ok(_)) | None => nothing(),
            Some(Err(why)) => go(F::Refused { why, floor: *v }, vec![EndRead]),
        },
        // ²: Showing keeps no clock (the backstop is the subscription owner's, and arrives as E9).
        (Some(F::Showing { .. }), FollowEv::Due) => impossible(),
        (Some(F::Showing { version: v }), FollowEv::Published(version)) if version > *v => go(F::Showing { version }, vec![]),
        (Some(F::Showing { .. }), FollowEv::Published(_)) => nothing(),
        // ---- Refused ----
        (Some(F::Refused { floor, .. }), FollowEv::Follow) => go(F::Reading { tries: 0, at: None, floor: *floor }, vec![Read]),
        // ¹³ / ²: a Refused follow holds no read and keeps no clock.
        (Some(F::Refused { .. }), FollowEv::Read(_) | FollowEv::Due) => impossible(),
        // P8 (the architect on H4): OUR OWN publish at v IS a site's record -- the follow shows it, and its
        // subscription read starts again, so another device's later publish is shown too.
        (Some(F::Refused { floor, .. }), FollowEv::Published(version)) => go(F::Showing { version: version.max(*floor) }, vec![Read]),
    }
}

/// Every followed site, by app. Private map; [`Follows::on`] is the ONE writer.
#[derive(Debug, Default)]
pub(crate) struct Follows {
    map: BTreeMap<String, Follow>,
}

impl Follows {
    pub(crate) fn get(&self, app: &str) -> Option<&Follow> {
        self.map.get(app)
    }

    /// The apps whose follow is `Showing` (a held subscription the backstop and the renewal keep).
    pub(crate) fn showing(&self) -> impl Iterator<Item = &String> {
        self.map.iter().filter(|(_, f)| matches!(f, Follow::Showing { .. })).map(|(a, _)| a)
    }

    /// Each follow's timer, if any (E10): a NotFound's backoff.
    pub(crate) fn due(&self) -> impl Iterator<Item = (&String, u64)> {
        self.map.iter().filter_map(|(a, f)| match f {
            Follow::Reading { at: Some(at), .. } => Some((a, *at)),
            Follow::Reading { at: None, .. } | Follow::Showing { .. } | Follow::Refused { .. } => None,
        })
    }

    /// THE ONE WRITER: `ev` for `app`'s follow, through [`step`].
    pub(crate) fn on(&mut self, app: &str, ev: FollowEv<'_>, now: u64) -> (Vec<FollowAct>, FollowCell) {
        let (next, acts, cell) = step(self.map.get(app), ev, now);
        if let Some(next) = next {
            self.map.insert(app.to_string(), next);
        }
        (acts, cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read at `seq` whose value is `value_len` bytes: 32 is a site's (its bundle hash), anything longer is not.
    fn record(seq: u64, value_len: usize) -> HeadRead {
        HeadRead { seq, value: vec![7u8; value_len] }
    }

    /// THE TABLE, printed and pinned (PUBLISH-LIFE's FOLLOW: 4 x 4 = 16 cells, 5 impossible -- ² 3, ¹³ 2). Each cell is
    /// driven with the event that makes it move where it can (a newer version, a due backoff).
    #[test]
    fn every_follow_cell_is_decided_and_the_counts_are_the_documents() {
        let states: [(&str, Option<Follow>); 4] = [
            ("(none)", None),
            ("Reading", Some(Follow::Reading { tries: 1, at: Some(10), floor: 0 })),
            ("Showing{2}", Some(Follow::Showing { version: 2 })),
            ("Refused", Some(Follow::Refused { why: "x".into(), floor: 0 })),
        ];
        let newer = record(5, 32);
        let mut counts = BTreeMap::<FollowCell, usize>::new();
        println!("| follow | E13 follow | E9 read (v5) | E10 due | E14 published (v5) |");
        for (name, f) in &states {
            let cells: Vec<String> = [FollowEv::Follow, FollowEv::Read(Some(&newer)), FollowEv::Due, FollowEv::Published(5)]
                .into_iter()
                .map(|ev| {
                    let (next, acts, cell) = step(f.as_ref(), ev, 100);
                    *counts.entry(cell).or_default() += 1;
                    format!("{cell:?} {next:?} {acts:?}")
                })
                .collect();
            println!("| **{name}** | {} |", cells.join(" | "));
        }
        println!("{counts:?}");
        assert_eq!(counts.get(&FollowCell::Impossible).copied().unwrap_or(0), 5, "an impossible cell changed: PUBLISH-LIFE's FOLLOW table changes with it");
        assert_eq!(counts.values().sum::<usize>(), 16);
    }

    /// THE FOLLOW MODEL (H4's gate, the architect): 300 seeded runs against a REFERENCE -- the node's site version, moved
    /// by another device's publishes and by THIS page's (E14) -- over reads (the current version, NotFound, a record that
    /// is not a site's), dues, and follows. After every step the follow never shows a version the node never held, never
    /// moves back, and a Refused follow resumes on this page's publish. At the end, with a read of the current version,
    /// a follow that is not Refused shows EXACTLY the node's version (P8). Reach floored: runs through Refused and back,
    /// runs where our own publish moved it, runs moved by another device.
    #[test]
    fn the_follow_model_shows_the_node_through_our_publishes_and_refusals() {
        let (mut via_refused, mut by_ours, mut by_other, mut lag_hidden) = (0usize, 0usize, 0usize, 0usize);
        for seed in 1..=300u64 {
            let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut next = |n: u64| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % n
            };
            let mut fs = Follows::default();
            let (mut node, mut now, mut last, mut was_refused, mut was_refused_ever) = (0u64, 0u64, 0u64, false, false);
            fs.on("app", FollowEv::Follow, now);
            for step_n in 0..40 {
                now += 50 + next(3_000);
                let at = format!("seed {seed} step {step_n}");
                let before = fs.get("app").cloned();
                match next(7) {
                    0 => node += 1, // another device publishes
                    1 => {
                        node += 1; // THIS page publishes: its end is the node's word too
                        fs.on("app", FollowEv::Published(node), now);
                        by_ours += usize::from(before != fs.get("app").cloned());
                    }
                    2 | 3 => {
                        if matches!(before, Some(Follow::Reading { .. } | Follow::Showing { .. })) {
                            // A LAGGING node (Codex on #543, 4): one read in three answers an OLDER version.
                            let lagging = node > 1 && next(3) == 0;
                            let at = if lagging { 1 + next(node - 1) } else { node };
                            let read = (node > 0).then(|| record(at, 32));
                            if lagging && at < last && was_refused_ever {
                                lag_hidden += 1;
                            }
                            fs.on("app", FollowEv::Read(read.as_ref()), now);
                            by_other += usize::from(matches!((&before, fs.get("app")), (Some(Follow::Showing { version: a }), Some(Follow::Showing { version: b })) if b > a));
                        }
                    }
                    4 => {
                        if matches!(before, Some(Follow::Reading { .. } | Follow::Showing { .. })) {
                            fs.on("app", FollowEv::Read(Some(&record(node.max(1), 40))), now);
                            was_refused = true;
                            was_refused_ever = true;
                        }
                    }
                    5 => {
                        if fs.due().any(|(_, t)| t <= now) {
                            fs.on("app", FollowEv::Due, now);
                        }
                    }
                    _ => {
                        fs.on("app", FollowEv::Follow, now);
                    }
                }
                if let Some(Follow::Showing { version }) = fs.get("app") {
                    assert!(*version <= node, "{at}: a version the node never held ({version} > {node})");
                    assert!(*version >= last, "{at}: the follow moved BACK ({last} -> {version})");
                    last = *version;
                    if was_refused {
                        via_refused += 1;
                        was_refused = false;
                    }
                }
            }
            // The end: our own publish, then a read of what is live -- a follow shows EXACTLY the node (P8).
            node += 1;
            fs.on("app", FollowEv::Published(node), now);
            let read = record(node, 32);
            fs.on("app", FollowEv::Read(Some(&read)), now);
            assert_eq!(fs.get("app"), Some(&Follow::Showing { version: node }), "seed {seed}: the follow did not converge on the node");
        }
        println!("follow machine model: back from Refused {via_refused}, moved by our publish {by_ours}, by another device {by_other}, lagging reads below the floor after a refusal {lag_hidden}");
        assert!(via_refused >= 30 && by_ours >= 100 && by_other >= 100 && lag_hidden >= 30, "the model's reach is below its floor");
    }

    /// THE VERSION FLOOR IS KEPT THROUGH A REFUSAL (Codex on #543, 4; the architect: a stored primary fact). Showing
    /// v5, then a record that is not a site's refuses the follow, then a reopen follows again, and a LAGGING node answers
    /// v2: the follow must never show v2 -- the node has shown v5. Before: Refused dropped the floor, and v2 was shown.
    #[test]
    fn a_refusal_keeps_the_floor_and_a_lagging_read_below_it_stays_hidden() {
        let mut fs = Follows::default();
        fs.on("app", FollowEv::Follow, 0);
        fs.on("app", FollowEv::Read(Some(&record(5, 32))), 0);
        assert_eq!(fs.get("app"), Some(&Follow::Showing { version: 5 }), "THE SETUP");
        fs.on("app", FollowEv::Read(Some(&record(6, 40))), 0);
        assert!(matches!(fs.get("app"), Some(Follow::Refused { .. })), "THE SETUP: the follow was not refused");
        fs.on("app", FollowEv::Follow, 0);
        fs.on("app", FollowEv::Read(Some(&record(2, 32))), 1_000);
        assert_ne!(fs.get("app"), Some(&Follow::Showing { version: 2 }), "a lagging v2 was shown after the node had shown v5");
        fs.on("app", FollowEv::Read(Some(&record(7, 32))), 2_000);
        assert_eq!(fs.get("app"), Some(&Follow::Showing { version: 7 }), "THE CONTROL: a version above the floor is shown");
    }

    /// P7, BY TYPE AND BY TABLE: no cell of the follow acts anything but a read (the enum has no other act); and P8:
    /// from Refused, this page's publish at v shows v AND reads again (its subscription restarts).
    #[test]
    fn a_follow_only_reads_and_a_refused_follow_resumes_on_our_publish() {
        let (next, acts, _) = step(Some(&Follow::Refused { why: "x".into(), floor: 0 }), FollowEv::Published(3), 0);
        assert_eq!((next, acts), (Some(Follow::Showing { version: 3 }), vec![FollowAct::Read]), "P8: a Refused follow missed our own publish");
        // NotFound is read again on the backoff, never "absent".
        let (next, _, _) = step(Some(&Follow::Reading { tries: 0, at: None, floor: 0 }), FollowEv::Read(None), 1_000);
        assert!(matches!(next, Some(Follow::Reading { tries: 1, at: Some(at), .. }) if at > 1_000), "{next:?}");
        // A record that is not a site's refuses, and drops ONLY the follow's read.
        let bad = record(1, 40);
        let (next, acts, _) = step(Some(&Follow::Showing { version: 1 }), FollowEv::Read(Some(&bad)), 0);
        assert!(matches!(next, Some(Follow::Refused { .. })) && acts == vec![FollowAct::EndRead], "{next:?} {acts:?}");
    }
}
