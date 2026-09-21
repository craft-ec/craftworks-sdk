//! The NATIVE TWIN of M1's L3 (`probe/src/bin/live-cold-read.rs`): the same
//! tree, the same reads in the same order, on a cold node -- so the live GET
//! count has a number to EQUAL (M1 DoD, B: a difference is a finding about
//! the fake node or the real one).
//!
//! The tree is L3's to the byte: 300 `w/` rows and 60 each of `a/`, `b/`,
//! `c/`, 1000 B values (the prefix byte, the index in the first four), written
//! in sorted chunks of 40. A prolly tree is content-defined, so the same rows
//! make the same tree. The live test 1 runs AFTER tests 2 and 3 on the same
//! node, whose store keeps what they fetched, so this does too; only test 1's
//! GETs are counted.

use protocol::{Bound, Op, Reply, Request, WriteState};
use testkit::full_node::{Conn, FullNode, Served};

fn rows() -> Vec<(String, u32)> {
    let mut rows: Vec<(String, u32)> = (0..300).map(|i| ("w".to_string(), i)).collect();
    for p in ["a", "b", "c"] {
        rows.extend((0..60).map(|i| (p.to_string(), i)));
    }
    rows
}

fn range(req_id: u64, prefix: &str, after: Option<Vec<u8>>) -> Request {
    let mut hi = prefix.as_bytes().to_vec();
    *hi.last_mut().unwrap() += 1;
    Request::Range {
        req_id,
        lo: Bound::Included(prefix.as_bytes().to_vec()),
        hi: Bound::Excluded(hi),
        reverse: false,
        after,
        max_entries: 256,
    }
}

/// (rows, more) of `req_id`'s page, if one came.
fn page(r: &[Vec<u8>], req_id: u64) -> Option<(usize, bool)> {
    r.iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .find_map(|x| match x {
            Reply::Page {
                req_id: id,
                entries,
                cursor,
                ..
            } if id == req_id => Some((entries.len(), cursor.is_some())),
            _ => None,
        })
}

fn tab(node: &FullNode) -> Conn {
    let mut c = node.connect();
    c.one_answer_per_call();
    c.client(&Request::Identity);
    c
}

#[test]
fn l3s_tree_read_cold_in_l3s_order() {
    let a = FullNode::new();
    let mut w = a.connect();
    w.client(&Request::Identity);
    for (n, chunk) in rows().chunks(40).enumerate() {
        let mut ops: Vec<Op> = chunk
            .iter()
            .map(|(p, i)| {
                let mut v = vec![p.as_bytes()[0]; 1000];
                v[..4].copy_from_slice(&i.to_le_bytes());
                Op::Put(format!("{p}/{i:04}").into_bytes(), v)
            })
            .collect();
        ops.sort_by(|x, y| match (x, y) {
            (Op::Put(k1, _), Op::Put(k2, _)) => k1.cmp(k2),
            _ => std::cmp::Ordering::Equal,
        });
        let id = 100 + n as u64;
        let r = w.client(&Request::Write { write_id: id, ops });
        assert!(
            r.iter()
                .filter_map(|b| protocol::decode_reply(b).ok())
                .any(|x| matches!(x, Reply::WriteState { write_id, state: WriteState::Published } if write_id == id)),
            "setup: write {id} never published"
        );
    }
    let b = FullNode::cold_over(&a);

    // Tests 2 and 3, for what they leave in B's store.
    let (mut t1, mut t2) = (tab(&b), tab(&b));
    assert_eq!(
        page(&t1.client(&range(1, "a/", None)), 1),
        Some((60, false))
    );
    assert_eq!(
        page(&t1.client(&range(7, "b/", None)), 7),
        Some((60, false))
    );
    assert_eq!(
        page(&t2.client(&range(7, "c/", None)), 7),
        Some((60, false))
    );

    // Test 1, on a fresh connection, paging 256 at a time.
    let mut t = tab(&b);
    let before = t.served(Served::Get);
    let (mut got, mut after, mut req) = (0usize, None, 20u64);
    loop {
        let (n, more) = page(&t.client(&range(req, "w/", after.clone())), req)
            .unwrap_or_else(|| panic!("req {req} was never answered"));
        got += n;
        if !more || n == 0 {
            break;
        }
        after = Some(format!("w/{:04}", got - 1).into_bytes());
        req += 1;
    }
    let gets = t.served(Served::Get) - before;
    assert_eq!(got, 300, "read {got} of 300");
    assert_eq!(t.max_stranded(), 0, "{:?}", t.strand_details());
    println!("  native twin of L3 test 1: 300 of 300, {gets} GETs, 0 stranded");
}
