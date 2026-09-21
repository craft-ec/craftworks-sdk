//! A freenet node this process starts, and will stop.
//!
//! Shaped after `freenet-harness`'s `kill9.rs`, deliberately: that file
//! already worked out how to spawn a private node and reap it, and
//! rediscovering it would have been a second answer to a settled question.
//!
//! It REFUSES the ports other people's nodes are on. A probe that measured
//! someone else's node would produce numbers that look fine and mean nothing.

use anyhow::{bail, Context, Result};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Ports this probe must never touch: the user's own nodes.
pub const RESERVED: &[u16] = &[7509, 7609];

const BOOT: Duration = Duration::from_secs(45);

/// How the node is run.
///
/// Not a detail: a delegate-originated PUT is silently dropped in `Local` and
/// works in `Network`, measured both ways on the same binary. Anything that
/// exercises a delegate WRITE must run on a `Network` node, which also means
/// a local-mode node cannot stand in for one in an acceptance run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `freenet local local`.
    Local,
    /// `freenet network --is-gateway --skip-load-from-network`, bound to
    /// loopback.
    ///
    /// `--is-gateway` runs ISOLATED under `--skip-load-from-network`: no
    /// remote gateway index, no on-disk gateway cache, and no `--gateway`
    /// entries, so it connects to nothing. Its network port is 127.0.0.1, so
    /// nothing it does leaves this machine. It is network MODE without being
    /// on the network, which is the only way to ask a delegate-write question
    /// without joining anything.
    IsolatedNetwork { network_port: u16 },
}

pub struct Node {
    child: Option<Child>,
    pub port: u16,
    pub mode: Mode,
    dir: PathBuf,
}

impl Node {
    pub fn spawn(port: u16, dir: &Path) -> Result<Self> {
        Self::spawn_in(port, dir, Mode::Local)
    }

    pub fn spawn_in(port: u16, dir: &Path, mode: Mode) -> Result<Self> {
        if RESERVED.contains(&port) {
            bail!("port {port} belongs to someone else's node; refusing to use it");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            bail!("something is already listening on {port}; refusing to share it");
        }
        // The network port too — BEFORE anything is created or spawned. It was
        // checked after the directories were made, so a refused network port
        // left a tree behind.
        if let Mode::IsolatedNetwork { network_port } = mode {
            if RESERVED.contains(&network_port) {
                bail!("network port {network_port} belongs to someone else's node");
            }
            if TcpStream::connect(("127.0.0.1", network_port)).is_ok() {
                bail!("something is already listening on {network_port}");
            }
        }
        for sub in ["data", "config", "log"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        // The node takes `--log-level`, NOT `RUST_LOG`: setting RUST_LOG in
        // this process changes nothing there, which is how an earlier attempt
        // to read the node's own account of a delegate PUT came back empty
        // and looked like "the node said nothing".
        let mut args: Vec<String> = match mode {
            Mode::Local => vec!["local".into(), "local".into()],
            Mode::IsolatedNetwork { network_port } => {
                vec![
                    "network".into(),
                    "--is-gateway".into(),
                    "--skip-load-from-network".into(),
                    // Loopback on both the listen and the advertised address:
                    // a gateway that advertises 127.0.0.1 is reachable only
                    // from this machine, and with no gateway entries and no
                    // index fetch it dials nothing.
                    "--network-address".into(),
                    "127.0.0.1".into(),
                    "--network-port".into(),
                    network_port.to_string(),
                    "--public-network-address".into(),
                    "127.0.0.1".into(),
                    "--public-network-port".into(),
                    network_port.to_string(),
                    "--ws-api-address".into(),
                    "127.0.0.1".into(),
                ]
            }
        };
        args.extend([
            "--ws-api-port".to_string(),
            port.to_string(),
            "--data-dir".into(),
            dir.join("data").to_string_lossy().into_owned(),
            "--config-dir".into(),
            dir.join("config").to_string_lossy().into_owned(),
            "--log-dir".into(),
            dir.join("log").to_string_lossy().into_owned(),
        ]);
        if let Ok(level) = std::env::var("PROBE_LOG_LEVEL") {
            args.push("--log-level".into());
            args.push(level);
        }
        let child = Command::new("freenet")
            .args(&args)
            // Captured to the temp tree rather than discarded: the node's
            // FILE log carries no DEBUG lines even at `--log-level debug`, so
            // the only place its own account of a delegate PUT can be is the
            // console. Discarding it is why an earlier attempt concluded "the
            // node said nothing" when nobody had looked where it speaks.
            .stdout(Stdio::from(std::fs::File::create(
                dir.join("log/console.out"),
            )?))
            .stderr(Stdio::from(std::fs::File::create(
                dir.join("log/console.err"),
            )?))
            .spawn()
            .context("spawning `freenet local local` — is the binary on PATH?")?;
        let mut n = Node {
            child: Some(child),
            port,
            mode,
            dir: dir.to_path_buf(),
        };
        n.wait_ready()?;
        Ok(n)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + BOOT;
        while Instant::now() < deadline {
            if let Some(c) = self.child.as_mut() {
                if let Ok(Some(status)) = c.try_wait() {
                    bail!("the node exited before it was ready ({status})");
                }
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                std::thread::sleep(Duration::from_millis(300));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("the node did not accept connections within {BOOT:?}")
    }

    /// SIGKILL then REAP: killing without waiting leaves a zombie holding the
    /// port against the next spawn.
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Stop and start again on the same data, in the SAME mode.
    ///
    /// The mode is carried deliberately. Restarting through `Node::spawn`
    /// would come back as `Local` whatever it was before, and a network node
    /// that quietly became a local one mid-run is a probe whose later answers
    /// are about a different system from its earlier ones — with nothing in
    /// the output to say so.
    pub fn restart(&mut self) -> Result<()> {
        let dir = self.dir.clone();
        let port = self.port;
        let mode = self.mode;
        self.stop();
        let mut fresh = Node::spawn_in(port, &dir, mode)?;
        self.child = fresh.child.take();
        Ok(())
    }

    pub fn ws(&self) -> String {
        format!(
            "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
            self.port
        )
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Removes its directory whatever happens, including on an early return.
pub struct TempTree(pub PathBuf);

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
