//! THE APP NAMESPACE's rules (the forest ruling), natively: what the web
//! Session applies to every name that crosses into it.
use craftworks_sdk::app::{check, own, read, write};

#[test]
fn an_apps_names_are_relative_and_stored_under_its_own_prefix() {
    assert_eq!(write(Some("alpha"), "notes").unwrap(), "alpha.notes");
    assert_eq!(read(Some("alpha"), "notes").unwrap(), "alpha.notes");
    // A watch key keeps its parent band: the prefix goes before the domain.
    assert_eq!(read(Some("alpha"), "notes#00ff").unwrap(), "alpha.notes#00ff");
    assert_eq!(own(Some("alpha"), "alpha.notes").as_deref(), Some("notes"));
}

#[test]
fn another_apps_data_is_read_absolutely_and_never_written() {
    assert_eq!(read(Some("alpha"), "@beta/notes").unwrap(), "beta.notes");
    let e = write(Some("alpha"), "@beta/notes").unwrap_err();
    assert!(e.to_string().contains("another app's"), "{e}");
    assert!(read(Some("alpha"), "@beta").is_err(), "an @ name with no /name was read");
    assert!(read(Some("alpha"), "@BAD/notes").is_err(), "an @ name with a bad app id was read");
}

#[test]
fn a_session_with_no_app_writes_nothing_and_reads_names_as_given() {
    let e = write(None, "notes").unwrap_err();
    assert!(e.to_string().starts_with("refused") || e.to_string().contains("no app"), "{e}");
    assert_eq!(read(None, "notes").unwrap(), "notes");
    assert_eq!(own(None, "whatever.notes").as_deref(), Some("whatever.notes"));
}

/// The listing an app sees is ITS domains only — and a name that merely
/// STARTS with the app id is somebody else's (`alphabet` is not `alpha`).
#[test]
fn an_app_sees_only_its_own_domains_and_a_longer_app_id_is_not_it() {
    for other in ["beta.notes", "alphabet.notes", "alpha-2.notes", "alpha", "notes"] {
        assert_eq!(own(Some("alpha"), other), None, "`{other}` was taken for alpha's");
    }
}

#[test]
fn an_app_id_is_one() {
    for ok in ["a", "notes-example", "app_1", &"x".repeat(32)] {
        assert!(check(ok).is_ok(), "{ok}");
    }
    for bad in ["", "has.dot", "UPPER", "sl/ash", "@x", &"x".repeat(33)] {
        assert!(check(bad).is_err(), "{bad} was taken for an app id");
    }
}
