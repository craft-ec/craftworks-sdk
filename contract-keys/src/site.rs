//! A SITE: a published app's one stable address (builder#117). A site is a Register record under the SAME
//! authority as the person's head, with the label `site:<app>`: params `RG01 ‖ 0 ‖ key ‖ "site:" ‖ app`, a record
//! `(seq = version, value = blake3(web))`. The site contract (freenet-contracts site/) holds it as its web
//! framing's metadata. Mode 0 only, as `register::head_state` is.

use signer_proto::head::{FLAG_RECORD, RECORD_MAGIC, VALUE_MAX};

/// A site label's prefix: `site:` then the app id.
pub const LABEL_PREFIX: &[u8] = b"site:";
const KEY_LEN: usize = 32;
const SIG_LEN: usize = 64;

/// THE ONE relabelling (builder#117): the same authority, another label. `None` when `params` are not a mode-0
/// Register's -- a k-of-n keyset is Phase 6's, refused rather than guessed.
pub fn relabel(params: &[u8], label: &[u8]) -> Option<Vec<u8>> {
    let rest = params.strip_prefix(RECORD_MAGIC)?;
    let (&mode, rest) = rest.split_first()?;
    if mode != 0 || rest.len() < KEY_LEN {
        return None;
    }
    let mut out = params[..RECORD_MAGIC.len() + 1 + KEY_LEN].to_vec();
    out.extend_from_slice(label);
    Some(out)
}

/// `app`'s site params, from the person's Register params: `None` when `app` is not an app id
/// (`core_types::name::app_ok`, the one rule) or the params are not mode 0.
pub fn site_params(register_params: &[u8], app: &str) -> Option<Vec<u8>> {
    if !core_types::name::app_ok(app) {
        return None;
    }
    let mut label = LABEL_PREFIX.to_vec();
    label.extend_from_slice(app.as_bytes());
    relabel(register_params, &label)
}

/// The node's web framing `[meta length u64 BE][meta][web length u64 BE][web]`, parsed exactly: `(meta, web)`.
pub fn framing(state: &[u8]) -> Option<(&[u8], &[u8])> {
    let (m, rest) = state.split_at_checked(8)?;
    let m = usize::try_from(u64::from_be_bytes(m.try_into().ok()?)).ok()?;
    let (meta, rest) = rest.split_at_checked(m)?;
    let (w, rest) = rest.split_at_checked(8)?;
    let w = usize::try_from(u64::from_be_bytes(w.try_into().ok()?)).ok()?;
    (rest.len() == w).then_some((meta, rest))
}

/// The node's web framing around a site record: what a site PUT carries.
pub fn frame(meta: &[u8], web: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + meta.len() + web.len());
    out.extend_from_slice(&(meta.len() as u64).to_be_bytes());
    out.extend_from_slice(meta);
    out.extend_from_slice(&(web.len() as u64).to_be_bytes());
    out.extend_from_slice(web);
    out
}

/// A record `(seq, value)` read out of a Register state ONLY IF it verifies under exactly `params`: mode 0, the
/// plain layout `RG01 | flags = record | terminal = 0 | seq | vlen | value | sig` and nothing after, and the
/// writer's signature over `RG01-sig ‖ blake3(params) ‖ 0 ‖ seq ‖ blake3(value)` (`verify_strict`). Anything it
/// cannot check -- fork evidence, a terminal record, another mode -- is `None`: this is what lets the signer take
/// a page-supplied site contract as truth (architect, builder#117 Q1), so an unverified record never counts.
pub fn verified_record(params: &[u8], state: &[u8]) -> Option<(u64, Vec<u8>)> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let rest = params.strip_prefix(RECORD_MAGIC)?;
    let (&mode, rest) = rest.split_first()?;
    if mode != 0 {
        return None;
    }
    let key = VerifyingKey::from_bytes(rest.get(..KEY_LEN)?.try_into().ok()?).ok()?;
    let rest = state.strip_prefix(RECORD_MAGIC)?;
    let (&flags, rest) = rest.split_first()?;
    let (&terminal, rest) = rest.split_first()?;
    if flags != FLAG_RECORD || terminal != 0 {
        return None;
    }
    let (seq, rest) = rest.split_at_checked(8)?;
    let seq = u64::from_le_bytes(seq.try_into().ok()?);
    let (vlen, rest) = rest.split_at_checked(2)?;
    let vlen = u16::from_le_bytes([vlen[0], vlen[1]]) as usize;
    if vlen > VALUE_MAX || rest.len() != vlen + SIG_LEN {
        return None;
    }
    let (value, sig) = rest.split_at(vlen);
    let sig = Signature::from_bytes(sig.try_into().ok()?);
    let message = crate::register::signed_message(blake3::hash(params).as_bytes(), seq, blake3::hash(value).as_bytes());
    key.verify_strict(&message, &sig).ok()?;
    Some((seq, value.to_vec()))
}
