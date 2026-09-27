//! THE SDK'S STATUS VOCABULARY (sdk#518): every word a session's status methods answer, each set owned by ONE enum
//! with `code()` and `ALL`. The session emits through these (`web/src/session.rs`), `status_words` exports them as one
//! list, and an app imports that list (`sdk.status`) -- never a copy of its own. `RowState` (store.rs) is the row's.

use crate::store::RowState;

use core_types::vocabulary;

vocabulary! {
    /// Where an app's contract PUT stands (`Session::put_status`'s `state`).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PutStatus {
        None => "none",
        Pending => "pending",
        Put => "put",
        Refused => "refused",
        Cancelled => "cancelled",
    }
}

vocabulary! {
    /// How an app's site publication stands (`Session::site_status`'s `state`).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SiteStatus {
        None => "none",
        Publishing => "publishing",
        Published => "published",
        Superseded => "superseded",
        Refused => "refused",
        Cancelled => "cancelled",
    }
}

vocabulary! {
    /// Where an app's PUBLISH stands (`Session::app_publish_status`'s `state`, sdk#516; APP-PUBLISH.md): PUBLISHED at
    /// the site's read-back, BACKED_UP at every piece.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum AppPublishStatus {
        None => "none",
        Pieces => "pieces",
        Siting => "siting",
        Published => "published",
        BackedUp => "backed_up",
        BackupAbandoned => "backup_abandoned",
        Refused => "refused",
        Superseded => "superseded",
        Cancelled => "cancelled",
    }
}

vocabulary! {
    /// May this session write a head (`Session::can_write`'s `answer`)?
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CanWrite {
        Yes => "yes",
        No => "no",
        Unknown => "unknown",
    }
}

vocabulary! {
    /// What the node's signer answered when asked which head it signs for (`Session::asked`'s `state`).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum AskedState {
        Pending => "pending",
        Register => "register",
        NoKey => "nokey",
        NoSigner => "nosigner",
        Refused => "refused",
    }
}

vocabulary! {
    /// How a whole-tree REPAIR pass ended (`Session::repair_report`'s `outcome`, sdk#479): HEALTHY nothing was missing;
    /// REPAIRED everything missing was put back; PARTIAL some is not back yet; DAMAGED a group could not be solved;
    /// CANCELLED the pass was stopped (its counts are what it did).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RepairOutcome {
        Healthy => "healthy",
        Repaired => "repaired",
        Partial => "partial",
        Damaged => "damaged",
        Cancelled => "cancelled",
    }
}

impl RepairOutcome {
    /// THE one derivation of a pass's word from its numbers (the tab only labels it). DAMAGED wins over everything but
    /// a cancel: a group no read could solve is lost data, whatever else was put back.
    pub fn of(missing: u64, put_back: u64, rejected: u64, damaged: u64, cancelled: bool) -> RepairOutcome {
        if cancelled {
            RepairOutcome::Cancelled
        } else if damaged > 0 {
            RepairOutcome::Damaged
        } else if missing == 0 {
            RepairOutcome::Healthy
        } else if rejected == 0 && put_back >= missing {
            RepairOutcome::Repaired
        } else {
            RepairOutcome::Partial
        }
    }
}

/// Every list, as the JSON `status_words` exports: `{"rowState":[..], "putStatus":[..], "siteStatus":[..],
/// "appPublishStatus":[..], "canWrite":[..], "asked":[..], "groupHealth":[..], "repairOutcome":[..]}` (groupHealth the engine's, sdk#524).
pub fn words() -> serde_json::Value {
    fn list<T: Copy>(all: &[T], code: fn(T) -> &'static str) -> Vec<&'static str> {
        all.iter().map(|w| code(*w)).collect()
    }
    serde_json::json!({
        "rowState": list(&RowState::ALL, RowState::code),
        "putStatus": list(&PutStatus::ALL, PutStatus::code),
        "siteStatus": list(&SiteStatus::ALL, SiteStatus::code),
        "appPublishStatus": list(&AppPublishStatus::ALL, AppPublishStatus::code),
        "canWrite": list(&CanWrite::ALL, CanWrite::code),
        "asked": list(&AskedState::ALL, AskedState::code),
        "groupHealth": list(&engine::repair::GroupHealth::ALL, engine::repair::GroupHealth::code),
        "repairOutcome": list(&RepairOutcome::ALL, RepairOutcome::code),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// ONE LIST: `words()` is each enum's ALL, each word once, and nothing else.
    #[test]
    fn words_is_every_enum_s_all_each_word_once() {
        let v = words();
        let want: [(&str, Vec<&str>); 8] = [
            ("rowState", RowState::ALL.iter().map(|w| w.code()).collect()),
            ("putStatus", PutStatus::ALL.iter().map(|w| w.code()).collect()),
            ("siteStatus", SiteStatus::ALL.iter().map(|w| w.code()).collect()),
            ("appPublishStatus", AppPublishStatus::ALL.iter().map(|w| w.code()).collect()),
            ("canWrite", CanWrite::ALL.iter().map(|w| w.code()).collect()),
            ("asked", AskedState::ALL.iter().map(|w| w.code()).collect()),
            ("groupHealth", engine::repair::GroupHealth::ALL.iter().map(|w| w.code()).collect()),
            ("repairOutcome", RepairOutcome::ALL.iter().map(|w| w.code()).collect()),
        ];
        assert_eq!(v.as_object().map(|o| o.len()), Some(want.len()), "words() carries a list no enum owns: {v}");
        for (name, list) in want {
            let got: Vec<&str> = v[name].as_array().expect(name).iter().map(|w| w.as_str().expect("a word")).collect();
            assert_eq!(got, list, "{name} is not its enum's ALL");
            assert_eq!(list.iter().collect::<BTreeSet<_>>().len(), list.len(), "{name} says a word twice");
        }
    }

    /// Every word names its own variant back (`vocabulary!` builds `code`, `ALL` and `from_code` from one list, so
    /// no variant can be missing from `ALL`: a variant added to the list is in all three at once).
    #[test]
    fn every_word_names_its_variant_back() {
        fn round<T: Copy + PartialEq + std::fmt::Debug>(all: &[T], code: fn(T) -> &'static str, from: fn(&str) -> Option<T>) {
            for w in all {
                assert_eq!(from(code(*w)), Some(*w), "{w:?}");
            }
            assert_eq!(from("NOT_A_WORD"), None);
        }
        round(&RowState::ALL, RowState::code, RowState::from_code);
        round(&PutStatus::ALL, PutStatus::code, PutStatus::from_code);
        round(&SiteStatus::ALL, SiteStatus::code, SiteStatus::from_code);
        round(&CanWrite::ALL, CanWrite::code, CanWrite::from_code);
        round(&AskedState::ALL, AskedState::code, AskedState::from_code);
        round(&engine::repair::GroupHealth::ALL, engine::repair::GroupHealth::code, engine::repair::GroupHealth::from_code);
        round(&RepairOutcome::ALL, RepairOutcome::code, RepairOutcome::from_code);
    }

    /// Lost is the rolled-back state, and nothing else.
    #[test]
    fn lost_is_rolled_back_only() {
        for s in RowState::ALL {
            assert_eq!(s.is_lost(), s == RowState::RolledBack, "{}", s.code());
        }
    }

    /// Each outcome, and the order the rules decide in (cancel > damaged > healthy > repaired > partial).
    #[test]
    fn a_repair_outcome_is_derived_in_one_order() {
        use RepairOutcome::*;
        assert_eq!(RepairOutcome::of(0, 0, 0, 0, false), Healthy);
        assert_eq!(RepairOutcome::of(3, 3, 0, 0, false), Repaired);
        assert_eq!(RepairOutcome::of(3, 2, 0, 0, false), Partial);
        assert_eq!(RepairOutcome::of(3, 3, 1, 0, false), Partial, "a refused put-back is not repaired");
        assert_eq!(RepairOutcome::of(3, 3, 0, 1, false), Damaged, "a damaged group wins over put-backs");
        assert_eq!(RepairOutcome::of(3, 3, 0, 1, true), Cancelled, "a cancel wins over everything");
        assert_eq!(RepairOutcome::of(0, 0, 0, 0, true), Cancelled);
    }
}
