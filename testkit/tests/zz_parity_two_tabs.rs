//! MEASUREMENT (sdk parity under two tabs): does owed parity get WRITTEN, and
//! is it REPORTED, with one tab and with two? Not a gate test.
use craftworks_sdk::store::{Edit, Store};
use protocol::{Reply, WriteState};

fn run(two: bool) {
    let node = testkit::PageNode::new();
    let (mut a, conn_a, _k) = testkit::page_store(&node);
    let other = if two { Some(testkit::page_store(&node)) } else { None };
    let mut now = 1_000u64;
    for i in 0..120u32 {
        let k = format!("n/{i:04}").into_bytes();
        a.apply_batch(&[(k, Edit::Put(vec![b'x'; 400]))]).expect("taken");
        if i % 10 == 9 {
            now += 1_000;
            conn_a.clone().tick_at(now);
            a.writes.tick();
            a.sync();
            if let Some((b, cb, _)) = other.as_ref() {
                let _ = b;
                cb.clone().tick_at(now);
            }
        }
    }
    let count = |c: &testkit::PageConn| {
        let r = c.replies();
        let pub_ = r.iter().filter(|x| matches!(x, Reply::SessionWriteState { state: WriteState::Published, .. } | Reply::WriteState { state: WriteState::Published, .. })).count();
        let par = r.iter().filter(|x| matches!(x, Reply::SessionWriteState { state: WriteState::ParityComplete, .. } | Reply::WriteState { state: WriteState::ParityComplete, .. })).count();
        (pub_, par)
    };
    for step in 0..40 {
        now += 1_000;
        conn_a.clone().tick_at(now);
        a.writes.tick();
        a.sync();
        if let Some((_, cb, _)) = other.as_ref() {
            cb.clone().tick_at(now);
        }
        if step % 10 == 9 || step == 0 {
            let (p, pc) = count(&conn_a);
            let owed_a = conn_a.with_server(|s| s.page.owed_groups());
            let scan_a = conn_a.with_server(|s| format!("{:?}", s.page.parity_scan()));
            let owed_b = other.as_ref().map(|(_, cb, _)| cb.with_server(|s| s.page.owed_groups()));
            println!("two={two} t+{step}s: A published verdicts {p}, parity-complete verdicts {pc}, A owed groups {owed_a}, A scan {scan_a}, B owed {owed_b:?}, node head {:?}", node.head().map(|h| h.0));
        }
    }
}

#[test]
#[ignore = "measurement"]
fn parity_one_tab_then_two() {
    run(false);
    run(true);
}
