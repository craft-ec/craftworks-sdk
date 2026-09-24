//! THE DECODER a published app's starter carries (sdk#347). The loader has fetched pieces and checked each against
//! its sha256; this rebuilds the bundle from any `k` of them and splits it into its files, with the SAME code the
//! SDK cut them with (`pieces`, over `freenet_prolly::rs`). JavaScript only moves bytes in and out.
//!
//! The ABI, all `u32` and little-endian, over this module's own memory:
//! - `alloc(len) -> ptr`: room for the request.
//! - `open(ptr, len) -> status`: the request is `k, m, payload, bundle_len`, then `k + m` entries of `len` and
//!   that many bytes, `len = u32::MAX` for a piece that is absent. `0` is success; anything else is a refusal whose
//!   words are at `error_ptr()`/`error_len()`.
//! - after a success: `count()` files; for file `i`, `name_ptr(i)`/`name_len(i)` (UTF-8) and
//!   `data_ptr(i)`/`data_len(i)`; and the rebuilt bundle itself at `bundle_ptr()`/`bundle_len()`.
//!
//! One opened bundle at a time: the page loads the SDK once.

use std::cell::RefCell;

thread_local! {
    static OPENED: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
    static BUNDLE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Decode a request (the ABI above) into the bundle's files: the native form of `open`, tested directly.
/// A rebuilt bundle and its files, by name.
pub type Opened = (Vec<u8>, Vec<(String, Vec<u8>)>);

pub fn open_request(req: &[u8]) -> Result<Opened, String> {
    let mut at = 0usize;
    let u32_at = |at: &mut usize| -> Result<u32, String> {
        let b = req.get(*at..*at + 4).ok_or("the request ends early")?;
        *at += 4;
        Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    let k = u32_at(&mut at)? as usize;
    let m = u32_at(&mut at)? as usize;
    let payload = u32_at(&mut at)? as usize;
    let bundle_len = u32_at(&mut at)? as usize;
    if k == 0 || k > 256 || m == 0 || m > 256 {
        return Err("k or m out of range".into());
    }
    let mut have: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
    for _ in 0..k + m {
        let n = u32_at(&mut at)?;
        if n == u32::MAX {
            have.push(None);
            continue;
        }
        let n = n as usize;
        let b = req.get(at..at + n).ok_or("a piece runs past the request")?;
        at += n;
        have.push(Some(b.to_vec()));
    }
    if at != req.len() {
        return Err("trailing bytes after the pieces".into());
    }
    let b = pieces::join(k, m, payload, bundle_len, &have).map_err(|e| why(&e))?;
    let files = pieces::unbundle(&b).map_err(|e| why(&e))?;
    Ok((b, files))
}

/// A refusal in words, without the formatting machinery (which is most of a small wasm's size).
fn why(e: &pieces::Error) -> String {
    match e {
        pieces::Error::NotABundle(w) => format_static("not a bundle: ", w),
        pieces::Error::DuplicateName(_) => "not a bundle: a duplicate name".into(),
        pieces::Error::TooManyPieces { .. } => "too many pieces".into(),
        pieces::Error::Shape(w) => format_static("the pieces do not fit: ", w),
        pieces::Error::Rs(pieces::RsError::NotEnough(_)) => "NotEnough: fewer than k pieces".into(),
        pieces::Error::Rs(_) => "the pieces do not decode".into(),
    }
}

fn format_static(a: &str, b: &str) -> String {
    let mut s = String::with_capacity(a.len() + b.len());
    s.push_str(a);
    s.push_str(b);
    s
}

#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn alloc(len: u32) -> *mut u8 {
    let mut v = vec![0u8; len as usize];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` and `len` are what `alloc` returned and the caller wrote: the request is read once and freed.
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub unsafe extern "C" fn open(ptr: *mut u8, len: u32) -> u32 {
    let req = Vec::from_raw_parts(ptr, len as usize, len as usize);
    match open_request(&req) {
        Ok((bundle, files)) => {
            OPENED.with(|o| *o.borrow_mut() = files);
            BUNDLE.with(|b| *b.borrow_mut() = bundle);
            0
        }
        Err(e) => {
            ERROR.with(|x| *x.borrow_mut() = e);
            OPENED.with(|o| o.borrow_mut().clear());
            1
        }
    }
}

#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn count() -> u32 {
    OPENED.with(|o| o.borrow().len() as u32)
}

fn file<R>(i: u32, f: impl FnOnce(&(String, Vec<u8>)) -> R, none: R) -> R {
    OPENED.with(|o| o.borrow().get(i as usize).map(f).unwrap_or(none))
}

#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn name_ptr(i: u32) -> *const u8 {
    file(i, |f| f.0.as_ptr(), std::ptr::null())
}
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn name_len(i: u32) -> u32 {
    file(i, |f| f.0.len() as u32, 0)
}
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn data_ptr(i: u32) -> *const u8 {
    file(i, |f| f.1.as_ptr(), std::ptr::null())
}
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn data_len(i: u32) -> u32 {
    file(i, |f| f.1.len() as u32, 0)
}
/// The rebuilt bundle itself: what the post-load repair re-derives a missing piece from.
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn bundle_ptr() -> *const u8 {
    BUNDLE.with(|b| b.borrow().as_ptr())
}
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn bundle_len() -> u32 {
    BUNDLE.with(|b| b.borrow().len() as u32)
}

#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn error_ptr() -> *const u8 {
    ERROR.with(|x| x.borrow().as_ptr())
}
#[cfg_attr(target_arch = "wasm32", no_mangle)]
pub extern "C" fn error_len() -> u32 {
    ERROR.with(|x| x.borrow().len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request as the loader writes it.
    pub(crate) fn request(c: &pieces::Cut, keep: impl Fn(usize) -> bool) -> Vec<u8> {
        let mut r = Vec::new();
        for v in [c.k, c.m, c.payload, c.bundle_len] {
            r.extend_from_slice(&(v as u32).to_le_bytes());
        }
        for (i, p) in c.pieces.iter().enumerate() {
            if keep(i) {
                r.extend_from_slice(&(p.len() as u32).to_le_bytes());
                r.extend_from_slice(p);
            } else {
                r.extend_from_slice(&u32::MAX.to_le_bytes());
            }
        }
        r
    }

    #[test]
    fn a_request_with_m_pieces_missing_opens_to_the_files() {
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("index.js", b"export {}".to_vec()),
            ("sdk.wasm", (0..200_000u32).map(|i| (i * 7) as u8).collect()),
        ];
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_slice())).collect();
        let b = pieces::bundle(&refs).unwrap();
        let c = pieces::cut(&b, 16_384, 8).unwrap();
        let (rebuilt, got) = open_request(&request(&c, |i| i % 3 != 0 || i >= c.k + 8 - 1)).unwrap();
        assert_eq!(rebuilt, b);
        assert_eq!(
            got,
            files
                .iter()
                .map(|(n, b)| (n.to_string(), b.clone()))
                .collect::<Vec<_>>()
        );
        // Too many missing: refused in words.
        let err = open_request(&request(&c, |i| i > 8)).unwrap_err();
        assert!(err.contains("NotEnough"), "{err}");
        // A request that lies about its lengths: refused, not a panic.
        let mut r = request(&c, |_| true);
        r.truncate(r.len() - 1);
        assert!(open_request(&r).is_err());
        assert!(open_request(&[1, 2, 3]).is_err());
    }
}
