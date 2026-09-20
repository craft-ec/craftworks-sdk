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

/// Why a delegate wasm is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a wasm module this gate can read. Never a pass: a module it cannot
    /// parse is one whose imports it has not seen.
    Malformed(String),
    /// No `freenet_delegate*` imports at all.
    ///
    /// What an UNWIRED delegate looks like — the `#[delegate]` macro emits the
    /// entry point only behind the `freenet-main-delegate` feature, and
    /// without it the crate compiles to a module with no exports and no
    /// imports. A subset check passes over that happily, which is how a gate
    /// once ran green over nothing at all.
    Unwired,
    /// Imports the pinned node does not define. The delegate fails to
    /// INSTANTIATE, so it never runs at all.
    NotDefined(Vec<String>),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed(e) => write!(f, "not a readable wasm module: {e}"),
            Refusal::Unwired => write!(
                f,
                "no delegate host imports at all — that is an UNWIRED \
                 delegate; check the crate declares the `freenet-main-delegate` \
                 feature. A subset check passes over it without complaint."
            ),
            Refusal::NotDefined(bad) => write!(
                f,
                "{} import(s) the pinned node does not define ({}): the \
                 delegate would fail to INSTANTIATE, so it never runs",
                bad.len(),
                bad.join(", ")
            ),
        }
    }
}

/// What a delegate wasm imports, once both halves of the gate have passed.
#[derive(Debug)]
pub struct Checked {
    /// Every import in the module.
    pub total: usize,
    /// The `freenet_delegate*` ones, which is what the node resolves.
    pub delegate: Vec<(String, String)>,
}

/// The whole gate, in one call.
///
/// Both halves live here rather than in a caller, because they are not
/// independent: the subset check is VACUOUS over a module with no imports,
/// so a caller that ran only that half would report a clean pass over an
/// unwired delegate. Splitting them between a library and a binary meant any
/// second caller had to know to do both, and the one that mattered did not.
pub fn check(wasm: &[u8]) -> Result<Checked, Refusal> {
    let all = imports(wasm).map_err(Refusal::Malformed)?;
    let delegate: Vec<(String, String)> = all
        .iter()
        .filter(|(m, _)| m.starts_with("freenet_delegate"))
        .cloned()
        .collect();
    if delegate.is_empty() {
        return Err(Refusal::Unwired);
    }
    let bad: Vec<String> = delegate
        .iter()
        .filter(|(_, f)| !DEFINED_BY_NODE.contains(&f.as_str()))
        .map(|(m, f)| format!("{m}::{f}"))
        .collect();
    if !bad.is_empty() {
        return Err(Refusal::NotDefined(bad));
    }
    Ok(Checked {
        total: all.len(),
        delegate,
    })
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

    /// The gate is one call, and each half of it refuses on its own.
    ///
    /// The halves are tested together because they are not independent: the
    /// subset check is VACUOUS over a module with no imports, so a gate that
    /// ran only that half would pass an unwired delegate. Each case below
    /// must fail for its OWN reason, which is why the refusal is a typed
    /// value and not a string anyone has to match on.
    #[test]
    fn the_gate_refuses_an_unwired_delegate_and_an_undefined_import_separately() {
        // 1. Unwired: no delegate imports at all. A subset check is happy.
        let unwired = module_importing("env", "something_else");
        assert_eq!(
            check(&unwired).unwrap_err(),
            Refusal::Unwired,
            "a module with no delegate imports was not refused as unwired, \
             which is exactly the module a subset check passes over"
        );

        // 2. An import the node does not define. Instantiation fails.
        let bad = module_importing("freenet_delegate_v1", "schedule_wakeup");
        assert!(
            !DEFINED_BY_NODE.contains(&"schedule_wakeup"),
            "this case needs a function the node does NOT define; pick another"
        );
        match check(&bad) {
            Err(Refusal::NotDefined(v)) => assert_eq!(
                v,
                vec!["freenet_delegate_v1::schedule_wakeup".to_string()],
                "the refusal did not name the import that caused it"
            ),
            other => panic!("an undefined import was not refused: {other:?}"),
        }

        // 3. The positive case, or the two refusals above could be a gate
        //    that says no to everything.
        let good = module_importing("freenet_delegate_v1", DEFINED_BY_NODE[0]);
        let c = check(&good).expect("a module importing only defined functions");
        assert_eq!(c.delegate.len(), 1);
        assert_eq!(c.delegate[0].1, DEFINED_BY_NODE[0]);
    }

    /// The probe's Freenet client stack must never reach the engine or the SDK.
    ///
    /// `probe` links freenet-stdlib with `net`, tokio and a websocket client.
    /// The engine is a sans-IO state machine compiled INTO a delegate: if any
    /// of that arrived in its tree it would be linked into the wasm, and the
    /// first sign would be a delegate that no longer instantiates — a long way
    /// from the edit that caused it. One shared workspace makes this a
    /// one-line-in-a-Cargo.toml mistake, so it is asserted rather than
    /// remembered.
    #[test]
    fn the_probes_client_stack_is_not_a_dependency_of_the_engine_or_the_sdk() {
        const FORBIDDEN: [&str; 4] = ["freenet-stdlib", "tokio-tungstenite", "tokio", "anyhow"];
        // The RUNNING directory, not the building one. Cargo runs a test
        // binary with its cwd at the package root, which is true of the tree
        // being tested; `CARGO_MANIFEST_DIR` is baked in at build time, so a
        // binary served from a shared `CARGO_TARGET_DIR` names whichever
        // worktree built it — and pointing `cargo tree` at a worktree that no
        // longer exists fails the gate on a missing directory rather than on
        // a dependency.
        let here = std::env::current_dir().expect("a working directory");
        let root = here
            .parent()
            .expect("the workspace root is the probe's parent");
        let mut checked = 0usize;
        // `protocol` is here too, and for a sharper reason than the others:
        // it is carried by BOTH wasm binaries — the delegate's and the
        // browser SDK's — so a dependency added to it is one every one of
        // them pays for, on a download every new node makes.
        const GUARDED: [&str; 3] = ["engine", "craftworks-sdk", "protocol"];
        for pkg in GUARDED {
            let out = std::process::Command::new(env!("CARGO"))
                .args(["tree", "-p", pkg, "--edges", "normal", "--prefix", "none"])
                .current_dir(root)
                .output()
                .expect("cargo tree must run: a gate that cannot check has not checked");
            // Not a skip. A tree that could not be produced is a tree nobody
            // has looked at, and in a log that reads exactly like a clean one.
            assert!(
                out.status.success(),
                "cargo tree -p {pkg} failed, so the dependency was NOT checked:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let tree = String::from_utf8_lossy(&out.stdout);
            assert!(
                tree.lines().filter(|l| !l.trim().is_empty()).count() > 1,
                "cargo tree -p {pkg} printed almost nothing, so this asserts \
                 nothing about its dependencies"
            );
            for f in FORBIDDEN {
                assert!(
                    !tree.lines().any(|l| l.split_whitespace().next() == Some(f)),
                    "{f} is in {pkg}'s dependency tree. The engine is compiled \
                     into a delegate; the probe's client stack must not follow \
                     it there."
                );
            }
            checked += 1;
        }
        // Derived from the list, not written as a number: the last time
        // this was a literal, adding a package to the list made the gate
        // fail on its own floor rather than on anything it guards.
        assert_eq!(
            checked,
            GUARDED.len(),
            "only {checked} of {} guarded package(s) were checked",
            GUARDED.len()
        );
    }
}
