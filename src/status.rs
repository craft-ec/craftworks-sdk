//! THE SDK'S STATUS VOCABULARY (sdk#518): every word a session's status methods answer, each set owned by ONE enum
//! with `code()` and `ALL`. The session emits through these (`web/src/session.rs`), `status_words` exports them as one
//! list, and an app imports that list (`sdk.status`) -- never a copy of its own. `RowState` (store.rs) is the row's.

use crate::store::RowState;

/// Where an app's contract PUT stands (`Session::put_status`'s `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutStatus {
    None,
    Pending,
    Put,
    Refused,
    Cancelled,
}

impl PutStatus {
    pub const ALL: [PutStatus; 5] = [PutStatus::None, PutStatus::Pending, PutStatus::Put, PutStatus::Refused, PutStatus::Cancelled];

    pub fn code(self) -> &'static str {
        match self {
            PutStatus::None => "none",
            PutStatus::Pending => "pending",
            PutStatus::Put => "put",
            PutStatus::Refused => "refused",
            PutStatus::Cancelled => "cancelled",
        }
    }
}

/// How an app's site publication stands (`Session::site_status`'s `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteStatus {
    None,
    Publishing,
    Published,
    Superseded,
    Refused,
    Cancelled,
}

impl SiteStatus {
    pub const ALL: [SiteStatus; 6] =
        [SiteStatus::None, SiteStatus::Publishing, SiteStatus::Published, SiteStatus::Superseded, SiteStatus::Refused, SiteStatus::Cancelled];

    pub fn code(self) -> &'static str {
        match self {
            SiteStatus::None => "none",
            SiteStatus::Publishing => "publishing",
            SiteStatus::Published => "published",
            SiteStatus::Superseded => "superseded",
            SiteStatus::Refused => "refused",
            SiteStatus::Cancelled => "cancelled",
        }
    }
}

/// May this session write a head (`Session::can_write`'s `answer`)?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanWrite {
    Yes,
    No,
    Unknown,
}

impl CanWrite {
    pub const ALL: [CanWrite; 3] = [CanWrite::Yes, CanWrite::No, CanWrite::Unknown];

    pub fn code(self) -> &'static str {
        match self {
            CanWrite::Yes => "yes",
            CanWrite::No => "no",
            CanWrite::Unknown => "unknown",
        }
    }
}

/// What the node's signer answered when asked which head it signs for (`Session::asked`'s `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskedState {
    Pending,
    Register,
    NoKey,
    NoSigner,
    Refused,
}

impl AskedState {
    pub const ALL: [AskedState; 5] = [AskedState::Pending, AskedState::Register, AskedState::NoKey, AskedState::NoSigner, AskedState::Refused];

    pub fn code(self) -> &'static str {
        match self {
            AskedState::Pending => "pending",
            AskedState::Register => "register",
            AskedState::NoKey => "nokey",
            AskedState::NoSigner => "nosigner",
            AskedState::Refused => "refused",
        }
    }
}

/// Every list, as the JSON `status_words` exports: `{"rowState":[..], "putStatus":[..], "siteStatus":[..],
/// "canWrite":[..], "asked":[..]}`.
pub fn words() -> serde_json::Value {
    fn list<T: Copy>(all: &[T], code: fn(T) -> &'static str) -> Vec<&'static str> {
        all.iter().map(|w| code(*w)).collect()
    }
    serde_json::json!({
        "rowState": list(&RowState::ALL, RowState::code),
        "putStatus": list(&PutStatus::ALL, PutStatus::code),
        "siteStatus": list(&SiteStatus::ALL, SiteStatus::code),
        "canWrite": list(&CanWrite::ALL, CanWrite::code),
        "asked": list(&AskedState::ALL, AskedState::code),
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
        let want: [(&str, Vec<&str>); 5] = [
            ("rowState", RowState::ALL.iter().map(|w| w.code()).collect()),
            ("putStatus", PutStatus::ALL.iter().map(|w| w.code()).collect()),
            ("siteStatus", SiteStatus::ALL.iter().map(|w| w.code()).collect()),
            ("canWrite", CanWrite::ALL.iter().map(|w| w.code()).collect()),
            ("asked", AskedState::ALL.iter().map(|w| w.code()).collect()),
        ];
        assert_eq!(v.as_object().map(|o| o.len()), Some(want.len()), "words() carries a list no enum owns: {v}");
        for (name, list) in want {
            let got: Vec<&str> = v[name].as_array().expect(name).iter().map(|w| w.as_str().expect("a word")).collect();
            assert_eq!(got, list, "{name} is not its enum's ALL");
            assert_eq!(list.iter().collect::<BTreeSet<_>>().len(), list.len(), "{name} says a word twice");
        }
    }

    /// Lost is the rolled-back state, and nothing else.
    #[test]
    fn lost_is_rolled_back_only() {
        for s in RowState::ALL {
            assert_eq!(s.is_lost(), s == RowState::RolledBack, "{}", s.code());
        }
    }
}
