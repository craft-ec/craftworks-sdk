//! Checks a delegate's wasm against the host functions a pinned node defines.

pub mod node;

/// Every delegate host function freenet 0.2.135 registers in its linker.
///
/// Taken from the REGISTRATIONS in `wasm_runtime/engine/wasmtime_engine.rs`,
/// not from a grep of the file: that file also contains module source strings
/// for the node's own refusal test, which import names the node deliberately
/// does NOT define, and counting those inflates the list.
///
/// freenet-stdlib 0.10.0 declares SEVEN more than this — put_contract_state,
/// update_contract_state, subscribe_contract, subscribe_contract_checked,
/// list_subscriptions, list_subscriptions_len, schedule_wakeup. A call to any
/// of them that survives optimisation puts an import in the module, and the
/// node refuses the module at INSTANTIATION: not at the call, and not only on
/// the branch that reaches it. The delegate simply never loads.
pub const DEFINED_BY_NODE: &[&str] = &[
    "__frnt__delegate__create_delegate",
    "__frnt__delegate__ctx_len",
    "__frnt__delegate__ctx_read",
    "__frnt__delegate__ctx_write",
    "__frnt__delegate__get_contract_state",
    "__frnt__delegate__get_contract_state_len",
    "__frnt__delegate__get_secret",
    "__frnt__delegate__get_secret_len",
    "__frnt__delegate__has_secret",
    "__frnt__delegate__list_secrets",
    "__frnt__delegate__list_secrets_len",
    "__frnt__delegate__remove_secret",
    "__frnt__delegate__set_secret",
];

/// The imports a wasm module declares, as `(module, field)`.
///
/// A hand-rolled walk of the binary's section table rather than a parser
/// dependency: the gate must keep working when the toolchain moves, and the
/// import section's shape is fixed by the spec.
pub fn imports(wasm: &[u8]) -> Result<Vec<(String, String)>, String> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return Err("not a wasm module".into());
    }
    let mut at = 8;
    while at < wasm.len() {
        let id = wasm[at];
        at += 1;
        let (size, used) = leb(wasm, at)?;
        at += used;
        let end = at + size as usize;
        if end > wasm.len() {
            return Err("a section runs past the end of the module".into());
        }
        if id == 2 {
            return parse_imports(&wasm[at..end]);
        }
        at = end;
    }
    // No import section at all is legal, and is the strongest possible pass.
    Ok(Vec::new())
}

fn parse_imports(mut s: &[u8]) -> Result<Vec<(String, String)>, String> {
    let (count, used) = leb(s, 0)?;
    s = &s[used..];
    let mut out = Vec::new();
    for _ in 0..count {
        let (m, rest) = name(s)?;
        let (f, rest) = name(rest)?;
        // kind byte, then its type index / limits — skipped, since the gate
        // only cares which names are imported.
        let kind = *rest.first().ok_or("truncated import")?;
        let rest = &rest[1..];
        let rest = match kind {
            0x00 => skip_leb(rest)?,                          // func: type index
            0x01 => skip_table(rest)?,                        // table
            0x02 => skip_limits(rest)?,                       // memory
            0x03 => rest.get(2..).ok_or("truncated global")?, // global: type + mut
            k => return Err(format!("unknown import kind {k}")),
        };
        out.push((m, f));
        s = rest;
    }
    Ok(out)
}

fn name(s: &[u8]) -> Result<(String, &[u8]), String> {
    let (len, used) = leb(s, 0)?;
    let start = used;
    let end = start + len as usize;
    let bytes = s.get(start..end).ok_or("truncated name")?;
    Ok((
        String::from_utf8(bytes.to_vec()).map_err(|_| "a name is not utf-8")?,
        &s[end..],
    ))
}

fn leb(s: &[u8], at: usize) -> Result<(u32, usize), String> {
    let mut v: u32 = 0;
    let mut shift = 0;
    let mut used = 0;
    loop {
        let b = *s.get(at + used).ok_or("truncated LEB128")?;
        v |= ((b & 0x7f) as u32) << shift;
        used += 1;
        if b & 0x80 == 0 {
            return Ok((v, used));
        }
        shift += 7;
        if shift > 31 {
            return Err("LEB128 too long".into());
        }
    }
}

fn skip_leb(s: &[u8]) -> Result<&[u8], String> {
    let (_, used) = leb(s, 0)?;
    Ok(&s[used..])
}

fn skip_limits(s: &[u8]) -> Result<&[u8], String> {
    let flags = *s.first().ok_or("truncated limits")?;
    let s = skip_leb(&s[1..])?;
    if flags & 1 == 1 {
        skip_leb(s)
    } else {
        Ok(s)
    }
}

fn skip_table(s: &[u8]) -> Result<&[u8], String> {
    skip_limits(s.get(1..).ok_or("truncated table")?)
}

/// Imports this node would refuse.
pub fn not_defined(wasm: &[u8]) -> Result<Vec<String>, String> {
    Ok(imports(wasm)?
        .into_iter()
        .filter(|(m, f)| {
            m.starts_with("freenet_delegate") && !DEFINED_BY_NODE.contains(&f.as_str())
        })
        .map(|(m, f)| format!("{m}::{f}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module importing exactly one function, built by hand.
    ///
    /// Hand-built rather than compiled, so the control needs no toolchain and
    /// cannot be quietly optimised away — the delegate's own imports vanished
    /// once already when a missing feature left the crate with no entry point,
    /// and the gate passed over the empty module without complaint.
    fn module_importing(module: &str, field: &str) -> Vec<u8> {
        let mut w = Vec::from(*b"\0asm");
        w.extend_from_slice(&[1, 0, 0, 0]);
        // type section: one () -> ()
        w.extend_from_slice(&[1, 4, 1, 0x60, 0, 0]);
        // import section
        let mut body = vec![1u8];
        body.push(module.len() as u8);
        body.extend_from_slice(module.as_bytes());
        body.push(field.len() as u8);
        body.extend_from_slice(field.as_bytes());
        body.extend_from_slice(&[0x00, 0x00]); // func, type 0
        w.push(2);
        w.push(body.len() as u8);
        w.extend_from_slice(&body);
        w
    }

    #[test]
    fn the_gate_reads_the_imports_that_are_there() {
        let m = module_importing("freenet_delegate_ctx", "__frnt__delegate__ctx_len");
        assert_eq!(
            imports(&m).unwrap(),
            vec![(
                "freenet_delegate_ctx".to_string(),
                "__frnt__delegate__ctx_len".to_string()
            )],
            "the parser did not read an import that is plainly there"
        );
        assert!(
            not_defined(&m).unwrap().is_empty(),
            "ctx_len is one the node defines"
        );
    }

    /// THE CONTROL. A gate that has never refused anything is not a gate.
    #[test]
    fn the_gate_refuses_a_host_function_the_node_does_not_define() {
        for field in [
            "__frnt__delegate__schedule_wakeup",
            "__frnt__delegate__put_contract_state",
            "__frnt__delegate__update_contract_state",
            "__frnt__delegate__subscribe_contract",
            "__frnt__delegate__subscribe_contract_checked",
            "__frnt__delegate__list_subscriptions",
            "__frnt__delegate__list_subscriptions_len",
        ] {
            let m = module_importing("freenet_delegate_contracts", field);
            let bad = not_defined(&m).unwrap();
            assert_eq!(
                bad.len(),
                1,
                "{field}: freenet-stdlib 0.10.0 declares it and the pinned node \
                 does not define it, so a module importing it must be refused"
            );
        }
    }

    /// A module with no imports at all passes — and that is exactly the state
    /// an unwired delegate is in, so the gate alone is not evidence the
    /// delegate was built. Whatever runs this must also assert the artefact
    /// imports what it is supposed to.
    #[test]
    fn an_empty_module_passes_which_is_why_the_caller_must_check_the_count() {
        let empty: Vec<u8> = b"\0asm\x01\0\0\0".to_vec();
        assert!(imports(&empty).unwrap().is_empty());
        assert!(not_defined(&empty).unwrap().is_empty());
    }
}
