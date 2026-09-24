//! A site record VERIFIES only as exactly what it claims (builder#117 Q1): `verified_record` is what lets the
//! signer take a page-named contract as truth, so every way to forge one must read as nothing.
use contract_keys::register::head_state;
use contract_keys::site::{relabel, site_params, verified_record};

fn params() -> (Vec<u8>, [u8; 32]) {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    let mut p = b"RG01".to_vec();
    p.push(0);
    p.extend_from_slice(&sk.verifying_key().to_bytes());
    p.extend_from_slice(b"head");
    (p, sk.to_bytes())
}

#[test]
fn a_signed_site_record_verifies_under_its_own_params_only() {
    let (head, key) = params();
    let site = site_params(&head, "notes").expect("site params");
    let value = [9u8; 32];
    let st = head_state(&site, &key, 4, &value).expect("signs");
    assert_eq!(verified_record(&site, &st), Some((4, value.to_vec())), "THE CONTROL: a real record does not verify");
    assert_eq!(verified_record(&head, &st), None, "verified under the head's params");
    assert_eq!(verified_record(&site_params(&head, "other").unwrap(), &st), None, "verified under another app's params");
    let other = { let sk = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]); let mut p = b"RG01".to_vec(); p.push(0); p.extend_from_slice(&sk.verifying_key().to_bytes()); p.extend_from_slice(b"site:notes"); p };
    assert_eq!(verified_record(&other, &st), None, "verified under another authority");
    // Tampered: the value, the seq, the signature, a trailing byte, the evidence flag, a terminal record.
    let at_value = 4 + 1 + 1 + 8 + 2;
    for (what, i) in [("value", at_value), ("seq", 6), ("signature", st.len() - 1)] {
        let mut t = st.clone();
        t[i] ^= 1;
        assert_eq!(verified_record(&site, &t), None, "a record with its {what} changed verified");
    }
    let mut longer = st.clone();
    longer.push(0);
    assert_eq!(verified_record(&site, &longer), None, "a record with a trailing byte verified");
    let mut evidence = st.clone();
    evidence[4] = 0b11;
    assert_eq!(verified_record(&site, &evidence), None, "a record claiming fork evidence verified (it cannot be checked here)");
    let mut terminal = st.clone();
    terminal[5] = 1;
    assert_eq!(verified_record(&site, &terminal), None, "a terminal record verified");
}

#[test]
fn relabel_keeps_the_authority_and_refuses_what_it_cannot_sign() {
    let (head, _) = params();
    let site = relabel(&head, b"site:notes").expect("mode 0");
    assert_eq!(&site[..37], &head[..37], "the authority changed");
    assert_eq!(&site[37..], b"site:notes");
    let mut quorum = head.clone();
    quorum[4] = 1;
    assert_eq!(relabel(&quorum, b"site:notes"), None, "a k-of-n keyset was relabelled (Phase 6)");
    assert_eq!(site_params(&head, "Not An App"), None, "an app id outside the one rule was taken");
}
