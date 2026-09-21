//! The one check that keeps every live probe off the owner's own nodes:
//! `probe::node::RESERVED` (7509, 7609). A spawn on either port — the ws port
//! or an isolated node's network port — is REFUSED, by name, before anything
//! is created or started.

use probe::node::{Mode, Node, RESERVED};

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("reserved-ports-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn refused_by_name(r: anyhow::Result<Node>, what: &str) -> String {
    match r {
        Ok(_) => panic!("{what}: a node was STARTED on the owner's port"),
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("belongs to someone else's node"), "{what}: refused, but not BECAUSE the port is reserved: {msg}");
            msg
        }
    }
}

#[test]
fn the_owners_ports_are_refused_as_the_ws_port_before_anything_is_created() {
    assert_eq!(RESERVED, &[7509, 7609]);
    for port in [7509u16, 7609] {
        for mode in [Mode::Local, Mode::IsolatedNetwork { network_port: 7797 }] {
            let dir = fresh_dir(&format!("ws{port}"));
            refused_by_name(Node::spawn_in(port, &dir, mode), &format!("ws port {port}, {mode:?}"));
            assert!(!dir.exists(), "ws port {port}: a directory was created before the refusal");
        }
    }
}

#[test]
fn the_owners_ports_are_refused_as_the_network_port_before_anything_is_created() {
    for net in [7509u16, 7609] {
        let dir = fresh_dir(&format!("net{net}"));
        // A free, unreserved ws port, so ONLY the network port can refuse.
        refused_by_name(Node::spawn_in(7798, &dir, Mode::IsolatedNetwork { network_port: net }), &format!("network port {net}"));
        assert!(!dir.exists(), "network port {net}: a directory was created before the refusal");
    }
}
