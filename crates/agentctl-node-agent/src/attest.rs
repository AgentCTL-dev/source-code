// SPDX-License-Identifier: BUSL-1.1
//! Pod→socket attestation via `SO_PEERCRED` (RFC 0002 §7 / RFC 0015).
//!
//! On the stock-unix substrate the node-agent (a DaemonSet, `hostPID: true`)
//! CONNECTS as a client to a per-pod management socket at
//! `<root>/<pod_uid>/mgmt.sock`. Because the socket lives on a shared hostPath, a
//! malicious co-tenant could plant a socket in another pod's subdir to
//! impersonate it — tricking the control plane into driving the wrong agent.
//!
//! The defence: read the connected peer's **kernel-attested** credentials
//! (`SO_PEERCRED` → the server pid) and confirm the process serving the socket
//! actually belongs to pod `<pod_uid>`. We resolve the peer pid's pod UID from
//! its cgroup membership (`/proc/<pid>/cgroup`) and compare it against the
//! requested UID.
//!
//! This module holds the **pure** parts (cgroup parsing + the decision), which
//! are unit-tested here. The live wiring (reading the peer pid off a connected
//! socket, denying on a confirmed mismatch) lives in the node-agent binary —
//! end-to-end attestation needs `hostPID` + `/proc` and a real socket, so it is
//! not unit-testable.

/// Read the connected peer's **kernel-attested** pid (`SO_PEERCRED`) off a
/// connected `AF_UNIX` stream by raw fd.
///
/// `SO_PEERCRED` reports the credentials of the process on the OTHER end of the
/// socket, so the SAME getsockopt works from either side:
///
/// * the management bridge dials a per-pod socket → the peer is the agent SERVER
///   ([`crate::ManagementClient::peer_pid`]);
/// * the infer-proxy ACCEPTS connections on its own socket → the peer is the
///   agent CLIENT that dialed in ([`crate::infer`]).
///
/// `std::os::unix::net::UnixStream::peer_cred` is still nightly-only
/// (`peer_credentials_unix_socket`), so we read `SO_PEERCRED` directly with
/// `getsockopt` (Linux). A non-positive pid (e.g. the kernel could not attribute
/// the peer) is reported as `None`.
pub fn peer_pid_of_fd(fd: std::os::unix::io::RawFd) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid, correctly sized out-params for the
    // SO_PEERCRED getsockopt on a connected AF_UNIX stream; `fd` is a borrowed
    // raw fd owned by the caller's live socket and outlives the call.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 || cred.pid <= 0 {
        return None;
    }
    Some(cred.pid as u32)
}

/// Resolve the Kubernetes pod UID from the text of a `/proc/<pid>/cgroup` file.
///
/// Handles BOTH cgroup drivers:
///
/// * **cgroupfs** — `.../kubepods/besteffort/pod<UID-with-hyphens>/<container>`
/// * **systemd**  — `.../kubepods-besteffort-pod<UID_with_underscores>.slice/<container>.scope`
///   (the UID uses `_` not `-`, and the pod token ends in `.slice`)
///
/// cgroup v1 lines are `N:controller:/path`; cgroup v2 is `0::/path` — we scan
/// each line's path field. We look for a `pod` token immediately followed by a
/// 36-char UUID (`8-4-4-4-12`, hex with `-` or `_` separators), normalize the
/// separators to `-`, lowercase the hex, and return it. Returns `None` if no
/// pod UID is present.
pub fn pod_uid_from_cgroup(content: &str) -> Option<String> {
    for line in content.lines() {
        // cgroup v1: "N:controller:/path"; v2: "0::/path". The path is whatever
        // follows the second ':' (v2 leaves the controller field empty). Fall
        // back to the whole line if the line is not in the expected shape.
        let path = line.splitn(3, ':').nth(2).unwrap_or(line);
        if let Some(uid) = scan_path_for_pod_uid(path) {
            return Some(uid);
        }
    }
    None
}

/// Read `/proc/<pid>/cgroup` and resolve its pod UID. Returns `None` on any IO
/// error (e.g. the pid is gone, or `/proc` is not the host's because `hostPID`
/// is unset) or if no pod UID is found.
pub fn pod_uid_for_pid(pid: u32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    pod_uid_from_cgroup(&content)
}

/// Scan a single cgroup path for the first `pod<UUID>` token.
fn scan_path_for_pod_uid(path: &str) -> Option<String> {
    // Every "pod" occurrence is a candidate (e.g. the systemd token
    // `kubepods-besteffort-pod<UID>.slice` contains a spurious "pod" inside
    // "kubepods"); the UUID validator is the real filter, so try each in order.
    for (idx, _) in path.match_indices("pod") {
        let rest = &path[idx + "pod".len()..];
        // Take the leading run of UUID characters (hex + '-'/'_'); a '.' (e.g.
        // ".slice") or '/' (path separator) ends the candidate — this strips a
        // trailing ".slice" for free on the systemd driver.
        let candidate: String = rest
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-' || *c == '_')
            .collect();
        if let Some(uid) = normalize_uuid(&candidate) {
            return Some(uid);
        }
    }
    None
}

/// Validate `s` as a `8-4-4-4-12` UUID whose separators are `-` or `_`, and
/// return it normalized (separators → `-`, hex lowercased). `None` if it does
/// not match the shape exactly.
fn normalize_uuid(s: &str) -> Option<String> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut chars = s.chars();
    let mut out = String::with_capacity(36);
    for (gi, &len) in GROUPS.iter().enumerate() {
        for _ in 0..len {
            let c = chars.next()?;
            if !c.is_ascii_hexdigit() {
                return None;
            }
            out.push(c.to_ascii_lowercase());
        }
        // A separator follows every group but the last.
        if gi + 1 < GROUPS.len() {
            match chars.next() {
                Some('-') | Some('_') => out.push('-'),
                _ => return None,
            }
        }
    }
    // Exactly 36 chars: anything trailing means this was not a bare UUID.
    if chars.next().is_some() {
        return None;
    }
    Some(out)
}

/// The outcome of comparing a kernel-attested peer UID against the requested one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attestation {
    /// The peer's resolved pod UID matches the requested one. Carries the UID.
    Attested(String),
    /// The peer belongs to a DIFFERENT pod — a confirmed impersonation attempt.
    Mismatch {
        /// The pod UID the caller asked us to drive.
        expected: String,
        /// The pod UID the socket's server process actually belongs to.
        got: String,
    },
    /// The peer's pod UID could not be resolved (no peer pid, or `/proc` lookup
    /// failed — typically because `hostPID` is unset). Attestation is skipped.
    Unresolved,
}

/// Decide attestation from the requested UID and the resolved peer UID:
/// `Some(x)` equal to `expected` → [`Attestation::Attested`]; `Some(x)`
/// different → [`Attestation::Mismatch`]; `None` → [`Attestation::Unresolved`].
pub fn attest_decision(expected_uid: &str, resolved: Option<&str>) -> Attestation {
    match resolved {
        Some(got) if got == expected_uid => Attestation::Attested(got.to_string()),
        Some(got) => Attestation::Mismatch {
            expected: expected_uid.to_string(),
            got: got.to_string(),
        },
        None => Attestation::Unresolved,
    }
}

/// How a confirmed attestation mismatch is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestMode {
    /// Deny the request (HTTP 403) on a confirmed mismatch. The default.
    Enforce,
    /// Log the mismatch as a security event but still drive the agent.
    Warn,
    /// Attestation disabled — never resolve, never deny.
    Off,
}

impl AttestMode {
    /// Parse the mode from `AGENTCTL_ATTEST_MODE` (default [`AttestMode::Enforce`];
    /// `"warn"`/`"off"`/`"enforce"`, case-insensitive). An unset or unrecognized
    /// value is treated as `Enforce` (fail-safe).
    pub fn from_env() -> Self {
        std::env::var("AGENTCTL_ATTEST_MODE")
            .ok()
            .map(|v| Self::parse(&v))
            .unwrap_or(AttestMode::Enforce)
    }

    /// Parse a mode string (case-insensitive); unknown values → `Enforce`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "warn" => AttestMode::Warn,
            "off" => AttestMode::Off,
            _ => AttestMode::Enforce,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "3d8f9e2a-1b2c-4d5e-6f70-8a9b0c1d2e3f";

    #[test]
    fn cgroupfs_driver_hyphenated_uid() {
        // cgroup v1, cgroupfs driver: the pod token is `pod<UID-with-hyphens>`.
        let content = "\
12:pids:/kubepods/besteffort/pod3d8f9e2a-1b2c-4d5e-6f70-8a9b0c1d2e3f/3c1f0a1b2c3d4e5f60718293a4b5c6d7e8f90123456789abcdef0123456789ab
11:memory:/kubepods/besteffort/pod3d8f9e2a-1b2c-4d5e-6f70-8a9b0c1d2e3f/3c1f0a1b2c3d4e5f60718293a4b5c6d7e8f90123456789abcdef0123456789ab
";
        assert_eq!(pod_uid_from_cgroup(content).as_deref(), Some(UID));
    }

    #[test]
    fn systemd_driver_underscored_slice() {
        // cgroup v1, systemd driver: `kubepods-besteffort-pod<UID_>.slice` — the
        // UID uses '_' and the token ends in `.slice` (both must be handled). The
        // spurious "pod" inside "kubepods" must not derail the scan.
        let content = "\
11:memory:/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod3d8f9e2a_1b2c_4d5e_6f70_8a9b0c1d2e3f.slice/cri-containerd-9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00.scope
";
        assert_eq!(pod_uid_from_cgroup(content).as_deref(), Some(UID));
    }

    #[test]
    fn cgroup_v2_unified_line() {
        // cgroup v2 unified: a single "0::/path" line, systemd driver.
        let content = "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod3d8f9e2a_1b2c_4d5e_6f70_8a9b0c1d2e3f.slice/cri-containerd-aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899.scope\n";
        assert_eq!(pod_uid_from_cgroup(content).as_deref(), Some(UID));
    }

    #[test]
    fn uppercase_uid_is_lowercased() {
        let content = "0::/kubepods/besteffort/pod3D8F9E2A-1B2C-4D5E-6F70-8A9B0C1D2E3F/container\n";
        assert_eq!(pod_uid_from_cgroup(content).as_deref(), Some(UID));
    }

    #[test]
    fn no_pod_token_is_none() {
        // A node-agent's own cgroup (or any non-pod process): no pod UID.
        let content = "\
0::/system.slice/docker.service
11:memory:/system.slice/kubelet.service
";
        assert_eq!(pod_uid_from_cgroup(content), None);
    }

    #[test]
    fn malformed_uuid_is_none() {
        // "pod" followed by something that isn't a 8-4-4-4-12 UUID.
        let content = "0::/kubepods/besteffort/podnot-a-uuid/container\n";
        assert_eq!(pod_uid_from_cgroup(content), None);
    }

    #[test]
    fn attest_decision_attested() {
        assert_eq!(
            attest_decision(UID, Some(UID)),
            Attestation::Attested(UID.to_string())
        );
    }

    #[test]
    fn attest_decision_mismatch() {
        let other = "00000000-0000-0000-0000-000000000000";
        assert_eq!(
            attest_decision(UID, Some(other)),
            Attestation::Mismatch {
                expected: UID.to_string(),
                got: other.to_string(),
            }
        );
    }

    #[test]
    fn attest_decision_unresolved() {
        assert_eq!(attest_decision(UID, None), Attestation::Unresolved);
    }

    #[test]
    fn attest_mode_parsing() {
        assert_eq!(AttestMode::parse("warn"), AttestMode::Warn);
        assert_eq!(AttestMode::parse("WARN"), AttestMode::Warn);
        assert_eq!(AttestMode::parse(" Off "), AttestMode::Off);
        assert_eq!(AttestMode::parse("enforce"), AttestMode::Enforce);
        // Unknown / empty → fail-safe to Enforce.
        assert_eq!(AttestMode::parse("bogus"), AttestMode::Enforce);
        assert_eq!(AttestMode::parse(""), AttestMode::Enforce);
    }
}
