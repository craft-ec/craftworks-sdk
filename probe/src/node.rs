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

pub struct Node {
    child: Option<Child>,
    pub port: u16,
    dir: PathBuf,
}

impl Node {
    pub fn spawn(port: u16, dir: &Path) -> Result<Self> {
        if RESERVED.contains(&port) {
            bail!("port {port} belongs to someone else's node; refusing to use it");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            bail!("something is already listening on {port}; refusing to share it");
        }
        for sub in ["data", "config", "log"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let child = Command::new("freenet")
            .args([
                "local",
                "local",
                "--ws-api-port",
                &port.to_string(),
                "--data-dir",
                &dir.join("data").to_string_lossy(),
                "--config-dir",
                &dir.join("config").to_string_lossy(),
                "--log-dir",
                &dir.join("log").to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning `freenet local local` — is the binary on PATH?")?;
        let mut n = Node {
            child: Some(child),
            port,
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

    pub fn restart(&mut self) -> Result<()> {
        let dir = self.dir.clone();
        let port = self.port;
        self.stop();
        let mut fresh = Node::spawn(port, &dir)?;
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
