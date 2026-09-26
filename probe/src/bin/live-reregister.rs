//! sdk#442, LIVE: DOES A NODE KEEP A DELEGATE'S REGISTRATION ACROSS A RESTART? A page that registered the signer reads
//! the node's "no such delegate" as NOT ANSWERED YET (F59's race) and never registers again -- right only if a
//! registration survives the node's restart. On a private node: register the signer, ask it (the control: it
//! answers), SIGKILL and restart the node on the same data, then ask again WITHOUT registering. An answer = kept; the
//! node's delegate error = lost. The node's own words are printed either way. The negative control: the same ask on a
//! FRESH node that never registered it must be the node's error.
//!
//! Exit status is the verdict (0 kept, 1 lost, 2 could not judge). usage: SIGNER_PORT=<port> live-reregister <signer.wasm>
use anyhow::{Context, Result};
use probe::node::{Mode, Node, TempTree};
use probe::signer::{ask, connect, register_delegate};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    std::process::exit(match run().await {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            println!("{}", serde_json::json!({ "could_not_judge": format!("{e:#}") }));
            2
        }
    });
}

async fn run() -> Result<bool> {
    let v = std::process::Command::new("freenet").arg("--version").output().context("freenet --version")?;
    let version = String::from_utf8_lossy(&v.stdout).lines().next().unwrap_or_default().to_string();
    let port: u16 = std::env::var("SIGNER_PORT").ok().and_then(|p| p.parse().ok()).context("SIGNER_PORT=<port> is required; there is no default")?;
    let wasm = std::fs::read(std::env::args().nth(1).context("usage: live-reregister <signer.wasm>")?)?;
    let dir = std::env::temp_dir().join(format!("live-reregister-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let mut node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
    let mut c = connect(&node.ws()).await?;
    let key = register_delegate(&mut c, &wasm).await?;
    // THE CONTROL: registered, it answers. Without this, "lost" below could be a signer that never answered.
    let before = ask(&mut c, &key, &signer::Request::Register).await.context("THE CONTROL: the registered signer did not answer")?;
    drop(c);
    node.restart()?;
    let mut c = connect(&node.ws()).await?;
    let after = ask(&mut c, &key, &signer::Request::Register).await;
    let kept = after.is_ok();
    // THE NEGATIVE CONTROL: the same ask, unregistered, on a FRESH node (another data dir) must come back as the
    // node's delegate error. Without it, "kept" could be an ask that never sees an absent delegate at all.
    let fresh_dir = dir.join("fresh");
    let _fresh_tree = TempTree(fresh_dir.clone());
    let fresh = Node::spawn_in(port + 2, &fresh_dir, Mode::IsolatedNetwork { network_port: port + 3 })?;
    let mut f = connect(&fresh.ws()).await?;
    let absent = ask(&mut f, &key, &signer::Request::Register).await;
    anyhow::ensure!(absent.is_err(), "THE NEGATIVE CONTROL: an unregistered signer on a fresh node answered ({absent:?}): this ask cannot see a lost registration");
    println!(
        "{}",
        serde_json::json!({
            "freenet": version,
            "before_restart": format!("{before:?}"),
            "after_restart_without_registering": match &after { Ok(a) => format!("answered: {a:?}"), Err(e) => format!("{e:#}") },
            "control_fresh_node_unregistered": format!("{:#}", absent.expect_err("ensured")),
            "registration": if kept { "KEPT across the restart" } else { "LOST with the restart" },
        })
    );
    Ok(kept)
}
