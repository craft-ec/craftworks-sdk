//! Regenerate the frozen vectors: `cargo run --example vectors`.
//!
//! Writes both files, because the two kinds of id are only meaningful beside
//! each other: `tests/vectors.txt` is `BLAKE3(bytes)` for each input, and
//! `tests/block_vectors.txt` is the id each input has as a block of each kind.
//! Same inputs, so the two files can be read side by side and the difference
//! seen for what it is.
use craftworks_sdk::{hex, BlockId, ContentHash};

fn main() {
    let inputs: [&[u8]; 5] = [
        b"",
        b"abc",
        &[0u8; 1],
        &[0xff; 33],
        b"\x01posts/\x00\x80\xfe",
    ];

    let content: String = inputs
        .iter()
        .map(|i| format!("{} {}\n", hex(i), hex(&ContentHash::of(i).bytes())))
        .collect();
    let blocks: String = inputs
        .iter()
        .map(|i| {
            format!(
                "raw {0} {1}\nnode {0} {2}\n",
                hex(i),
                BlockId::raw(i),
                BlockId::node(i)
            )
        })
        .collect();

    for (path, body) in [
        ("tests/vectors.txt", content),
        ("tests/block_vectors.txt", blocks),
    ] {
        std::fs::write(path, &body).unwrap();
        println!("{path}: {} lines", body.lines().count());
    }
}
