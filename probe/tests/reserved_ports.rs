//! The one check that keeps every live probe off the owner's own nodes:
//! `probe::node::RESERVED` (7509, 7609). A spawn on either port — the ws port
//! or an isolated node's network port — is REFUSED, by name, before anything
//! is created or started.

use probe::node::{Mode, Node, RESERVED};

/// A STUB `freenet` first on PATH for every node these tests could start. The
/// tests exist to show a refusal; a mutant that removes one must not be able
/// to start a REAL node on the owner's port (a mutant run did, sdk#208: the
/// real binary started, exited for want of a gateway, and a readiness probe
/// may have connected to the owner's node). With the stub nothing real can
/// start, and the mutant is still caught -- by the directory it created.
fn stub_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("reserved-ports-stub-{}", std::process::id()))
}

/// Left by the stub when it runs. A refusal that works never starts it, so
/// every test asserts it is ABSENT -- the stub being run at all is the finding.
fn stub_marker() -> std::path::PathBuf {
    stub_dir().join("ran")
}

fn stub_freenet() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let bin = stub_dir();
        std::fs::create_dir_all(&bin).expect("stub dir");
        let f = bin.join("freenet");
        std::fs::write(
            &f,
            format!(
                "#!/bin/sh\ntouch '{}'\necho 'stub freenet: reserved_ports tests never start a real node' >&2\nexit 1\n",
                stub_marker().display()
            ),
        )
        .expect("stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
    });
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    stub_freenet();
    let d = std::env::temp_dir().join(format!("reserved-ports-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn refused_by_name(r: anyhow::Result<Node>, what: &str) -> String {
    assert!(
        !stub_marker().exists(),
        "{what}: a node process was STARTED (the stub ran) -- a refusal let it through"
    );
    match r {
        Ok(_) => panic!("{what}: a node was STARTED on the owner's port"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("belongs to someone else's node"),
                "{what}: refused, but not BECAUSE the port is reserved: {msg}"
            );
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
            refused_by_name(
                Node::spawn_in(port, &dir, mode),
                &format!("ws port {port}, {mode:?}"),
            );
            assert!(
                !dir.exists(),
                "ws port {port}: a directory was created before the refusal"
            );
        }
    }
}

#[test]
fn the_owners_ports_are_refused_as_the_network_port_before_anything_is_created() {
    for net in [7509u16, 7609] {
        let dir = fresh_dir(&format!("net{net}"));
        // A free, unreserved ws port, so ONLY the network port can refuse.
        refused_by_name(
            Node::spawn_in(7798, &dir, Mode::IsolatedNetwork { network_port: net }),
            &format!("network port {net}"),
        );
        assert!(
            !dir.exists(),
            "network port {net}: a directory was created before the refusal"
        );
    }
}

/// The two-node door (live-cold-read's private nodes) refuses the owner's
/// ports the same way, as either port, before anything is created: one door,
/// one tested refusal (sdk#208 review).
#[test]
fn the_private_network_door_refuses_the_owners_ports_before_anything_is_created() {
    for port in [7509u16, 7609] {
        for (ws, net, which) in [(port, 7799u16, "ws"), (7798u16, port, "network")] {
            let dir = fresh_dir(&format!("pn-{which}{port}"));
            refused_by_name(
                Node::spawn_private_network(ws, net, &dir, &[]),
                &format!("private network, {which} port {port}"),
            );
            assert!(
                !dir.exists(),
                "private network, {which} port {port}: a directory was created before the refusal"
            );
        }
    }
}

/// THE CONTROL for the stub: the `freenet` a spawn would run is the stub, not
/// the real binary -- resolved the way the OS does, first on PATH, without
/// running anything. Without it, a PATH that silently kept the real binary
/// first would make the tests above safe only while the refusals hold.
#[test]
fn control_the_freenet_a_spawn_would_run_is_the_stub() {
    stub_freenet();
    let first = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| std::path::Path::new(d).join("freenet"))
        .find(|p| p.is_file())
        .expect("no freenet on PATH at all");
    assert_eq!(
        first,
        stub_dir().join("freenet"),
        "the real freenet is first on PATH: {first:?}"
    );
}
