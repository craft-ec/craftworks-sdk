//! THE `keep` RECORD (KEEPER §3): its binary form, pinned, and its home in the identity's own tree.

use craftworks_sdk::keep::{self, Health, Keep, Repair};
use craftworks_sdk::*;
use testkit::MemStore;

struct At(u64);
impl Env for At {
    fn now_ms(&mut self) -> u64 {
        self.0
    }
    fn rand32(&mut self) -> u32 {
        7
    }
}

fn sample() -> Keep {
    Keep { repair: Repair::Below(3), warn_below: 2, audited_at: 1_790_000_000, health: Health { groups: 40, whole: 37, degraded: 2, damaged: 1 } }
}

/// **The value's bytes are PINNED, version first.** Round-trips every field; a changed byte is a changed meaning.
#[test]
fn the_keep_value_round_trips_and_its_bytes_are_pinned() {
    let k = sample();
    let v = keep::encode(&k);
    assert_eq!(v.len(), keep::LEN);
    assert_eq!(v[0], 1, "the version byte moved");
    assert_eq!(&v[..3], &[1, 5, 2], "version, repair (below 3 = 2 + 3), warn_below");
    assert_eq!(&v[3..11], &1_790_000_000u64.to_le_bytes());
    assert_eq!(&v[11..], &[40, 0, 0, 0, 37, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(keep::decode(&v), Some(k));
    for r in [Repair::Off, Repair::Always, Repair::Below(0), Repair::Below(8)] {
        let k = Keep { repair: r, ..sample() };
        assert_eq!(keep::decode(&keep::encode(&k)), Some(k), "{r:?} did not round-trip");
    }
    // Another version, or another length, is not read as this one.
    let mut newer = v.clone();
    newer[0] = 2;
    assert_eq!(keep::decode(&newer), None, "a newer version's record was read as v1");
    assert_eq!(keep::decode(&v[..v.len() - 1]), None, "a short value was read");
}

/// **One record per asset, in the identity's own tree, under the SYSTEM tag**: set, read back, listed by target;
/// a second set replaces the first (the key is the target).
#[test]
fn a_keep_record_is_set_read_back_and_listed_by_its_target() {
    let mut d = Db::new(MemStore::default(), At(1_750_000_000_000), [1, 2, 3, 4]);
    let (a, b) = ([0xaau8; 32], [0xbbu8; 32]);
    assert_eq!(d.keep_list().unwrap(), vec![], "THE SETUP: records before any was set");
    d.keep_set(&a, &sample()).unwrap();
    d.keep_set(&b, &Keep::OWN_DEFAULT).unwrap();
    let changed = Keep { repair: Repair::Off, ..sample() };
    d.keep_set(&a, &changed).unwrap();
    assert_eq!(d.keep_get(&a).unwrap(), Some(changed));
    assert_eq!(d.keep_list().unwrap(), vec![(a, changed), (b, Keep::OWN_DEFAULT)], "not one record per asset, by target");
    assert_eq!(keep::key(&a)[0], 0x00, "the keep record is not under the SYSTEM tag");
    assert!(d.domains().unwrap().is_empty(), "a keep record was read as a domain");
}
