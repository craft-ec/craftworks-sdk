//! sdk#266 at the copy: a head this page ADOPTED (never its own commit) puts
//! every loaded range behind, so a read of one is not answered from the copy
//! — it re-asks, with a DELTA from the root the copy stands on.

use craftworks_sdk::{CachedStore, DbError, Loads, Outcome, Reads, StoreError};

fn store() -> CachedStore {
    CachedStore::new(Box::new(|| 0))
}

fn root(n: u8) -> [u8; 32] {
    [n; 32]
}

#[test]
fn a_stale_range_is_not_answered_from_the_copy_and_is_re_asked_as_a_delta() {
    let mut s = store();
    s.on_page(b"r/", b"s/", vec![(b"r/1".to_vec(), b"old".to_vec())], root(1));
    assert_eq!(Reads::scan(&mut s, b"r/", b"s/", false, usize::MAX), Ok(vec![(b"r/1".to_vec(), b"old".to_vec())]), "the range did not load");
    let _ = s.take_outbound();

    s.mark_stale();
    assert_eq!(Reads::scan(&mut s, b"r/", b"s/", false, usize::MAX), Err(StoreError::NotLoaded), "a range behind the adopted head was answered from the copy");
    // A KEY read is the same question one row wide: a record read straight
    // off a stale range would show the value at the head this page has left.
    assert_eq!(Reads::get(&mut s, b"r/1"), Err(StoreError::NotLoaded), "a key in a range behind the adopted head was answered from the copy");
    assert!(s.take_outbound().is_empty(), "marking stale sent a read nobody asked for");

    // The READ is what asks, and what it asks for is a delta.
    let mut loads = Loads::new();
    let r: Result<Vec<(Vec<u8>, Vec<u8>)>, DbError> = Err(DbError::NotLoaded { lo: b"r/".to_vec(), hi: b"s/".to_vec() });
    let Outcome::Wait(_, ticket) = craftworks_sdk::decide_with(&mut loads, &mut s, r, 1_000, None) else {
        panic!("the read was not parked");
    };
    let asked: Vec<protocol::Request> = s
        .take_outbound()
        .iter()
        .filter_map(|f| match protocol::decode_request(f) {
            protocol::Incoming::Ok(env) => Some(env.body),
            _ => None,
        })
        .collect();
    match asked.as_slice() {
        [protocol::Request::ChangesSince { req_id, from, .. }] => {
            assert_eq!(*req_id, ticket, "the delta was not asked under the read's own ticket");
            assert_eq!(*from, root(1), "the delta was not asked FROM the root the copy stands on");
        }
        other => panic!("a stale range was re-asked as {other:?}, not as a delta from what the copy holds"),
    }

    // Its answer completes the read, and the row is the one the OTHER page wrote.
    craftworks_sdk::parking::delta_for_read(&mut loads, &mut s, ticket, vec![(b"r/1".to_vec(), Some(b"theirs".to_vec()))], None, root(2)).expect("the delta answered the read");
    assert_eq!(Reads::scan(&mut s, b"r/", b"s/", false, usize::MAX), Ok(vec![(b"r/1".to_vec(), b"theirs".to_vec())]), "the refreshed range does not hold the value at the adopted head");
    assert!(loads.take_ended().iter().any(|(id, e)| *id == ticket && *e == craftworks_sdk::Ended::Loaded), "the read's ticket never ended, so nothing would wake it");
}

/// A range nobody has ever loaded is not "stale": it is UNKNOWN, and it takes
/// the full load, as before. A delta from a root that never held it would
/// record an empty range as current.
#[test]
fn a_range_never_loaded_still_takes_a_full_load() {
    let mut s = store();
    s.on_page(b"r/", b"s/", vec![], root(1));
    s.mark_stale();
    let _ = s.take_outbound();
    let mut loads = Loads::new();
    let r: Result<Vec<(Vec<u8>, Vec<u8>)>, DbError> = Err(DbError::NotLoaded { lo: b"z/".to_vec(), hi: b"zz/".to_vec() });
    let Outcome::Wait(_, _) = craftworks_sdk::decide_with(&mut loads, &mut s, r, 1_000, None) else {
        panic!("the read was not parked");
    };
    let asked: Vec<protocol::Request> = s
        .take_outbound()
        .iter()
        .filter_map(|f| match protocol::decode_request(f) {
            protocol::Incoming::Ok(env) => Some(env.body),
            _ => None,
        })
        .collect();
    assert!(matches!(asked.as_slice(), [protocol::Request::Range { .. }]), "a range never loaded was asked for as a delta: {asked:?}");
}
