//! Refuse a delegate wasm that imports a host function the pinned node does
//! not define.
//!
//! An artefact gate, run on every delegate build. It WILL fire when
//! freenet-stdlib is bumped and the new version calls something the pinned
//! node has not got — that is its job, not a false alarm.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: import-gate <delegate.wasm>");
        std::process::exit(2);
    });
    let wasm = std::fs::read(&path)?;
    let all = probe::imports(&wasm).map_err(anyhow::Error::msg)?;
    let bad = probe::not_defined(&wasm).map_err(anyhow::Error::msg)?;
    let delegate_imports: Vec<_> = all
        .iter()
        .filter(|(m, _)| m.starts_with("freenet_delegate"))
        .collect();
    println!(
        "{path}: {} import(s), {} from freenet_delegate*",
        all.len(),
        delegate_imports.len()
    );
    for (m, f) in &delegate_imports {
        println!("  {m}::{f}");
    }
    // An unwired delegate compiles to a module with NO imports, and a
    // subset check passes over it happily. Demand at least the context
    // functions, which every delegate the macro wires up will import.
    if delegate_imports.is_empty() {
        anyhow::bail!(
            "{path} imports no delegate host functions at all. That is what an \
             UNWIRED delegate looks like — check the crate declares the \
             `freenet-main-delegate` feature — and a subset check passes over \
             it without complaint."
        );
    }
    if bad.is_empty() {
        println!(
            "ok: every delegate import is one of the {} the node defines",
            probe::DEFINED_BY_NODE.len()
        );
        Ok(())
    } else {
        for b in &bad {
            println!("REFUSED: {b} is not defined by the pinned node");
        }
        anyhow::bail!(
            "{} import(s) the node does not define: the delegate would fail to \
             INSTANTIATE, for every message, not only when it reached the call",
            bad.len()
        )
    }
}
