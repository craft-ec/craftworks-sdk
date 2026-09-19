//! Regenerate tests/vectors.txt: `cargo run --example vectors > tests/vectors.txt`
fn main() {
    let inputs: [&[u8]; 5] = [
        b"",
        b"abc",
        &[0u8; 1],
        &[0xff; 33],
        b"\x01posts/\x00\x80\xfe",
    ];
    for i in inputs {
        println!("{} {}", craftworks_sdk::hex(i), craftworks_sdk::cid_hex(i));
    }
}
