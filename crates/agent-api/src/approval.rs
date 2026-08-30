// SPDX-License-Identifier: Apache-2.0
//! Owner-approval markers for destructive control verbs (RFC 0027 §6, P4-5).
//!
//! The CONTROL server records a pending destructive request as an annotation
//! on the target object; the GATEWAY — the only component that verifies the
//! owner's own bearer — flips it to approved. The supervisor that asked never
//! holds the owner's token, so it cannot approve its own request; the split
//! across two annotations (pending vs approved) makes "asked" and "the human
//! said yes" separately auditable.
//!
//! Marker grammar (both annotations): `<nonce>|<subject>|<expires unix>`.

/// Pending destructive request, written by the control server:
/// `<nonce>|<requesting user>|<expires>`.
pub const PENDING_DELETE_ANNOTATION: &str = "agentctl.dev/pending-delete";
/// Owner approval, written by the gateway after verifying the owner's
/// bearer: `<nonce>|<approving user>|<expires>`.
pub const APPROVED_DELETE_ANNOTATION: &str = "agentctl.dev/approved-delete";
/// How long a pending request (and an approval) stays valid.
pub const APPROVAL_TTL_SECS: i64 = 600;

/// Parse a marker into `(nonce, subject, expires_unix)`.
pub fn parse_approval(v: &str) -> Option<(String, String, i64)> {
    let mut parts = v.splitn(3, '|');
    let nonce = parts.next()?.to_string();
    let user = parts.next()?.to_string();
    let exp: i64 = parts.next()?.parse().ok()?;
    Some((nonce, user, exp))
}

/// Render a marker.
pub fn approval_marker(nonce: &str, subject: &str, expires_unix: i64) -> String {
    format!("{nonce}|{subject}|{expires_unix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip() {
        let m = approval_marker("a1b2", "mock:carol", 1_900_000_000);
        assert_eq!(
            parse_approval(&m).unwrap(),
            ("a1b2".to_string(), "mock:carol".to_string(), 1_900_000_000)
        );
        assert!(parse_approval("no-fields").is_none());
        assert!(parse_approval("a|b|not-a-number").is_none());
    }
}
