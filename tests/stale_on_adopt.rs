//! sdk#266 at the copy: a head this page ADOPTED (never its own commit) puts
//! every loaded range behind, so a read of one is not answered from the copy
//! — it re-asks, with a DELTA from the root the copy stands on.

use craftworks_sdk::{CachedStore, DbError, Loads, Outcome, Reads, StoreError};

fn store() -> CachedStore {
    testkit::cached_store().0
}

/// What a read of a range answers, as `Db` hands it to `decide_with`.
type Rows = Result<Vec<(Vec<u8>, Vec<u8>)>, DbError>;

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
    let r: Rows = Err(DbError::NotLoaded { lo: b"r/".to_vec(), hi: b"s/".to_vec() });
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
    let r: Rows = Err(DbError::NotLoaded { lo: b"z/".to_vec(), hi: b"zz/".to_vec() });
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

/// A page LOADS a wide range (the whole key space, on an opening session) and
/// READS a narrow one (a domain). The delta that answers the read makes the
/// span it covered current — and leaves the rest of the wide range stale,
/// rather than either keeping the whole thing stale (the read would re-ask
/// for ever) or calling the whole thing current (the rest was never re-asked).
#[test]
fn a_narrow_read_refreshes_its_own_span_of_a_wide_stale_range() {
    let mut s = store();
    s.on_page(b"", &[0xFF; 8], vec![(b"r/1".to_vec(), b"old".to_vec()), (b"z/1".to_vec(), b"other".to_vec())], root(1));
    let _ = Reads::scan(&mut s, b"r/", b"s/", false, usize::MAX);
    let _ = s.take_outbound();
    s.mark_stale();

    let mut loads = Loads::new();
    let r: Rows = Err(DbError::NotLoaded { lo: b"r/".to_vec(), hi: b"s/".to_vec() });
    let Outcome::Wait(_, ticket) = craftworks_sdk::decide_with(&mut loads, &mut s, r, 1_000, None) else {
        panic!("the read was not parked");
    };
    let _ = s.take_outbound();
    craftworks_sdk::parking::delta_for_read(&mut loads, &mut s, ticket, vec![(b"r/1".to_vec(), Some(b"theirs".to_vec()))], None, root(2)).expect("the delta answered the read");

    assert_eq!(
        Reads::scan(&mut s, b"r/", b"s/", false, usize::MAX),
        Ok(vec![(b"r/1".to_vec(), b"theirs".to_vec())]),
        "the span the delta covered is still stale: the read re-asks, is answered, and finds it stale again — for ever"
    );
    assert_eq!(
        Reads::scan(&mut s, b"z/", &[0xFF; 8], false, usize::MAX),
        Err(StoreError::NotLoaded),
        "a span the delta never covered was called current on the strength of another range's answer"
    );

    // And the span still stale asks from the root IT was last current at —
    // not from the root the first answer moved the copy to. Asked from there,
    // the engine says nothing changed, and rows the page has never seen are
    // recorded as current (measured in a browser: y scanned 0 rows for ever).
    let r: Rows = Err(DbError::NotLoaded { lo: b"z/".to_vec(), hi: [0xFF; 8].to_vec() });
    let Outcome::Wait(_, _) = craftworks_sdk::decide_with(&mut loads, &mut s, r, 2_000, None) else {
        panic!("the second stale span was not parked");
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
        [protocol::Request::ChangesSince { from, .. }] => assert_eq!(*from, root(1), "the second stale span asked from the root the FIRST answer moved the copy to"),
        other => panic!("the second stale span was not re-asked as a delta: {other:?}"),
    }
}

/// THE STATE THE BROWSER WAS IN (sdk#266). `Loads` refuses to ask for a span
/// it has already loaded while no read has succeeded since — "going round
/// without progress" — and the answer is a TICKETLESS `NotLoaded`, which the
/// page cannot wait on. After an adopted head that guard is wrong: the span
/// was loaded, and the copy has since learned it is behind. Asking again IS
/// the progress.
///
/// Measured before this: `y.scan` threw `code=NOT_LOADED wait=undefined`
/// every 250 ms for 30 s while the node answered nothing, because nothing was
/// ever asked. The unit tests above passed throughout — their `Loads` was
/// fresh, so the guard was never armed.
#[test]
fn a_stale_range_is_re_asked_even_though_it_was_loaded() {
    let mut s = store();
    let mut loads = Loads::new();
    // Arm the guard exactly as a completed load arms it.
    let (ticket, send) = loads.want(b"r/", b"s/", 0).expect("a first ticket");
    assert!(send, "the first want did not ask");
    let at = protocol::At { seq: 1, root: root(1) };
    let done = loads.on_page(ticket, vec![(b"r/1".to_vec(), b"old".to_vec())], None, at);
    assert!(matches!(done, craftworks_sdk::loads::Page::Complete { .. }), "the load did not complete");
    s.on_page(b"r/", b"s/", vec![(b"r/1".to_vec(), b"old".to_vec())], root(1));
    assert_eq!(loads.want(b"r/", b"s/", 0), None, "the guard is not armed, so this test proves nothing");

    s.mark_stale();
    let r: Rows = Err(DbError::NotLoaded { lo: b"r/".to_vec(), hi: b"s/".to_vec() });
    let out = craftworks_sdk::decide_with(&mut loads, &mut s, r, 1_000, None);
    let Outcome::Wait(_, again) = out else {
        panic!("a stale range answered a TICKETLESS NotLoaded: the page cannot wait on it, and nothing re-asks");
    };
    assert_ne!(again, ticket, "the re-ask took the finished load's ticket");
    let asked: Vec<protocol::Request> = s
        .take_outbound()
        .iter()
        .filter_map(|f| match protocol::decode_request(f) {
            protocol::Incoming::Ok(env) => Some(env.body),
            _ => None,
        })
        .collect();
    assert!(matches!(asked.as_slice(), [protocol::Request::ChangesSince { .. }]), "the re-ask did not go out: {asked:?}");
}
