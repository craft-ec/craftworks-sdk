//! Which secrets an `Install` writes, given what the store already holds.
//!
//! The signing key is the one secret that cannot be re-derived, and the
//! first-writer-wins guard in the shell does NOT protect it on its own: that
//! guard tests `head_writable`, which is code AND params AND key, so a store
//! holding a key but missing a code secret reads as "not writable" and lets
//! an `Install` through.

use engine_delegate::entry::{install_writes, Held};

/// **THE CASE THE SHELL GUARD DOES NOT COVER.**
///
/// A key is present and a code secret is not. `head_writable` is false — the
/// triple is incomplete — so the shell admits the `Install`. The codes are
/// filled in and the key is KEPT.
///
/// Replacing it would move the head's contract id and orphan everything
/// signed with the old key, and nobody kept a copy.
#[test]
fn an_install_over_a_partial_store_fills_the_codes_and_keeps_the_key() {
    let w = install_writes(Held {
        block_code: false,
        register_code: false,
        register_params: false,
        signing_key: true,
    });
    assert!(
        !w.signing_key,
        "an Install over a partial store would replace the signing key — the \
         one secret that cannot be re-derived"
    );
    assert!(
        w.block_code && w.register_code && w.register_params,
        "the partial store was left partial, so the delegate still cannot \
         write a head"
    );
}

/// THE NEGATIVE CONTROL: with no key held, the same call DOES write one.
///
/// Without this the test above passes against an `install_writes` that never
/// writes a key at all — which would make provisioning impossible rather
/// than safe.
#[test]
fn control_an_install_on_an_empty_store_writes_the_key() {
    let w = install_writes(Held::default());
    assert!(
        w.signing_key,
        "a first install did not write a key, so nothing can ever be \
         provisioned"
    );
    assert!(w.block_code && w.register_code && w.register_params);
}

/// The key is kept whatever else is held — it is the presence of the KEY that
/// decides, never the completeness of the rest.
#[test]
fn the_key_is_kept_whenever_it_is_present() {
    for (b, r, p) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, false),
        (true, true, true),
    ] {
        let w = install_writes(Held {
            block_code: b,
            register_code: r,
            register_params: p,
            signing_key: true,
        });
        assert!(!w.signing_key, "key replaced with codes held = {b} {r} {p}");
    }
}
