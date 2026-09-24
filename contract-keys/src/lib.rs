//! The ids and records the page, the signer and the probes share with the
//! contracts: a Block contract's instance id, and the head as a Register
//! record. Moved out of `engine-delegate` so they outlive it (sdk#209's Q8).
//!
//! Sans-IO and off the client-API allowlist: no `freenet-stdlib` in its
//! normal dependencies (only as a dev-dependency, to pin the derivation).

pub mod block;
pub mod register;
pub mod site;

/// A contract's code HASH, as the node takes it (`blake3(code)`,
/// freenet-stdlib's `CodeHash::from_code`). What names an instance is this
/// hash and the params, never the code bytes: a holder that only NAMES
/// contracts (the signer, sdk#334) keeps the hash.
pub fn code_hash(code: &[u8]) -> [u8; 32] {
    *blake3::hash(code).as_bytes()
}

/// A contract INSTANCE id from its code hash and params:
/// `blake3(code_hash ‖ params)`, freenet-stdlib's `generate_id`. The one
/// derivation; `block::contract_for` and the Register's id go through it, and
/// the tests pin it to the stdlib's own `ContractContainer::key()`.
pub fn instance(code_hash: &[u8; 32], params: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(code_hash);
    h.update(params);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    /// The Register's id from its code hash and params IS the node's: pinned
    /// to freenet-stdlib, like the Block's.
    #[test]
    fn an_instance_from_the_code_hash_is_the_node_s() {
        use freenet_stdlib::prelude::*;
        let code = vec![0xA5u8; 2048];
        for params in [b"register params".to_vec(), vec![], vec![7u8; 300]] {
            let id = ContractInstanceId::from_params_and_code(Parameters::from(params.clone()), ContractCode::from(code.clone()));
            assert_eq!(&super::instance(&super::code_hash(&code), &params)[..], &id.as_bytes()[..32]);
        }
    }
}
