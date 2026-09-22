//! `page::record_head` is a second reader of the Register record's layout
//! (the page does not link the delegate). It must agree with the first,
//! `engine_delegate::register::head_of`, on every record the signer makes —
//! and refuse what that refuses.

use engine_delegate::register::{head_of, head_state};

fn params() -> Vec<u8> {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME)
}

#[test]
fn record_head_agrees_with_the_delegates_reader() {
    let p = params();
    for (seq, b) in [(1u64, 3u8), (2, 250), (u64::MAX - 1, 0)] {
        let st = head_state(&p, &[7u8; 32], seq, &[b; 32]).expect("signs");
        assert_eq!(page::record_head(&st), head_of(&st), "seq {seq}");
        assert_eq!(page::record_head(&st), Some((seq, [b; 32])));
    }
}

#[test]
fn record_head_refuses_what_is_not_a_record() {
    let p = params();
    let st = head_state(&p, &[7u8; 32], 4, &[1; 32]).expect("signs");
    for bad in [&st[..3], &st[..12], b"RG02xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".as_slice(), &[][..]] {
        assert_eq!(page::record_head(bad), None);
        assert_eq!(head_of(bad), None);
    }
}
