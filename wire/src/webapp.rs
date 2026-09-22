//! The ARTEFACTS web container (craftworks-builder#104): the SDK build's own
//! artefacts, framed as freenet serves a web container, under the `webapp`
//! contract (freenet-contracts#47).
//!
//! An app names its artefacts by hash and carries none of them (ARCHITECTURE
//! §19). This is where they live: one container per SDK BUILD, the same bytes
//! for every app of that build, so it is PUT once and every later PUT of it is
//! the same contract. It ships in `pkg/web` beside the files it holds, with its
//! address in `artefacts.json` — builders never rebuild it, which is why its
//! bytes need to be reproducible per toolchain (like the wasm), not across
//! machines.
//!
//! - [`tar`]: a deterministic ustar — sorted paths, mode 0644, uid/gid 0,
//!   mtime 0, no owner names — so the same files give the same bytes.
//! - [`container`]: the node's framing, `[u64 BE meta len][meta][u64 BE web
//!   len][web]`, exactly what `webapp` validates.
//! - [`address`]: the contract instance id the node serves it under, derived by
//!   freenet-stdlib itself from the `webapp` code and `params = blake3(state)`.

use freenet_stdlib::prelude::*;

const BLOCK: usize = 512;

/// A deterministic ustar archive of `files`, sorted by path whatever order
/// they are given in. Regular files only; a path must fit ustar's 100 bytes.
pub fn tar(files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&(&str, &[u8])> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = Vec::new();
    for (path, body) in sorted {
        let name = path.as_bytes();
        if name.is_empty() || name.len() > 100 || path.contains("..") || path.starts_with('/') {
            return Err(format!("{path:?} is not a plain relative path of at most 100 bytes"));
        }
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name);
        let octal = |h: &mut [u8; BLOCK], at: usize, width: usize, v: u64| {
            let s = format!("{v:0w$o}\0", w = width - 1);
            h[at..at + width].copy_from_slice(s.as_bytes());
        };
        octal(&mut h, 100, 8, 0o644); // mode
        octal(&mut h, 108, 8, 0); // uid
        octal(&mut h, 116, 8, 0); // gid
        octal(&mut h, 124, 12, body.len() as u64); // size
        octal(&mut h, 136, 12, 0); // mtime
        h[156] = b'0'; // a regular file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // The checksum is summed with its own field as eight spaces.
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

/// The node's web-container framing (freenet 0.2.136 `app_packaging.rs`).
pub fn container(metadata: &[u8], web: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + metadata.len() + web.len());
    v.extend_from_slice(&(metadata.len() as u64).to_be_bytes());
    v.extend_from_slice(metadata);
    v.extend_from_slice(&(web.len() as u64).to_be_bytes());
    v.extend_from_slice(web);
    v
}

/// `webapp`'s params for a state: its blake3.
pub fn params(state: &[u8]) -> [u8; 32] {
    *blake3::hash(state).as_bytes()
}

/// The address the node serves `state` under (`/v1/contract/web/<address>/`),
/// as freenet-stdlib derives it from the `webapp` code and the params.
pub fn address(webapp_code: &[u8], state: &[u8]) -> String {
    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(webapp_code.to_vec())),
        Parameters::from(params(state).to_vec()),
    )))
    .key()
    .id()
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read back what `tar` wrote: (path, size, checksum-ok) per entry.
    fn entries(t: &[u8]) -> Vec<(String, usize, bool)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + BLOCK <= t.len() && t[at..at + BLOCK].iter().any(|&b| b != 0) {
            let h = &t[at..at + BLOCK];
            let name = String::from_utf8_lossy(&h[..100]).trim_end_matches('\0').to_string();
            let size = usize::from_str_radix(std::str::from_utf8(&h[124..135]).unwrap(), 8).unwrap();
            let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
            let mut blank = h.to_vec();
            blank[148..156].copy_from_slice(b"        ");
            let sum: u32 = blank.iter().map(|&b| b as u32).sum();
            out.push((name, size, sum == stored));
            at += BLOCK + size.div_ceil(BLOCK) * BLOCK;
        }
        out
    }

    #[test]
    fn the_same_files_in_any_order_make_the_same_bytes() {
        let a = tar(&[("b.wasm", b"bbb"), ("a.wasm", &[7u8; 700])]).unwrap();
        let b = tar(&[("a.wasm", &[7u8; 700]), ("b.wasm", b"bbb")]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len() % BLOCK, 0);
        assert_eq!(
            entries(&a),
            vec![("a.wasm".into(), 700, true), ("b.wasm".into(), 3, true)],
            "the archive does not read back as written, with valid checksums"
        );
        // THE CONTROL: different content, different bytes.
        assert_ne!(a, tar(&[("a.wasm", &[8u8; 700]), ("b.wasm", b"bbb")]).unwrap());
    }

    #[test]
    fn a_path_a_node_would_refuse_to_unpack_is_refused_here() {
        for bad in ["../x", "/etc/x", "", &"p".repeat(101)] {
            assert!(tar(&[(bad, b"x")]).is_err(), "{bad:?} was archived");
        }
    }

    #[test]
    fn the_framing_is_the_nodes_and_the_address_is_stdlibs() {
        let s = container(b"{}", b"web");
        assert_eq!(&s[..8], &2u64.to_be_bytes());
        assert_eq!(&s[10..18], &3u64.to_be_bytes());
        assert_eq!(s.len(), 8 + 2 + 8 + 3);
        let a = address(b"code", &s);
        assert_eq!(a, address(b"code", &s), "the address is not a function of its inputs");
        assert_ne!(a, address(b"other code", &s));
        assert_ne!(a, address(b"code", &container(b"{}", b"wEb")));
    }
}
