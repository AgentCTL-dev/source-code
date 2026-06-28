// SPDX-License-Identifier: BUSL-1.1
//! Socket discovery for the stock-unix substrate (RFC 0002 §6.1 / RFC 0008).
//!
//! The operator renders each agent pod with a per-pod hostPath subdir
//! (`<root>/<pod-uid>/`, via `subPathExpr`) into which the agent binds its
//! management socket (`mgmt.sock`). The node-agent — mounting the same hostPath
//! root — **discovers** those sockets; it does not allocate anything (RFC 0002
//! §6: "discovery, not allocation").

use std::fs;
use std::path::{Path, PathBuf};

/// A discovered per-pod management socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredAgent {
    /// The pod UID (the subdir name; also the descriptor join key, RFC 0002 §3).
    pub pod_uid: String,
    /// The host-side path to the agent's management socket.
    pub socket: PathBuf,
}

/// The conventional socket file name an agent binds inside its per-pod subdir.
pub const SOCKET_NAME: &str = "mgmt.sock";

/// Discover `<root>/<pod-uid>/mgmt.sock` sockets under `root`. A missing root is
/// not an error (the DaemonSet may start before any agent lands) — it yields an
/// empty list. Results are sorted by pod UID for stable output.
pub fn discover(root: &Path) -> std::io::Result<Vec<DiscoveredAgent>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let socket = entry.path().join(SOCKET_NAME);
        if socket.exists() {
            out.push(DiscoveredAgent {
                pod_uid: entry.file_name().to_string_lossy().into_owned(),
                socket,
            });
        }
    }
    out.sort_by(|a, b| a.pod_uid.cmp(&b.pod_uid));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("acc-disc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn pod_with_socket(root: &Path, uid: &str) -> UnixListener {
        let dir = root.join(uid);
        fs::create_dir_all(&dir).unwrap();
        UnixListener::bind(dir.join(SOCKET_NAME)).unwrap()
    }

    #[test]
    fn missing_root_is_empty_not_error() {
        let root = std::env::temp_dir().join("acc-disc-does-not-exist-xyz");
        assert_eq!(discover(&root).unwrap(), vec![]);
    }

    #[test]
    fn discovers_per_pod_sockets_sorted() {
        let root = tmp();
        let _a = pod_with_socket(&root, "uid-b");
        let _b = pod_with_socket(&root, "uid-a");
        // a subdir with no socket yet is skipped
        fs::create_dir_all(root.join("uid-c-no-socket")).unwrap();

        let found = discover(&root).unwrap();
        let uids: Vec<_> = found.iter().map(|d| d.pod_uid.as_str()).collect();
        assert_eq!(uids, ["uid-a", "uid-b"]); // sorted, socketless skipped
        assert!(found[0].socket.ends_with("uid-a/mgmt.sock"));

        let _ = fs::remove_dir_all(&root);
    }
}
