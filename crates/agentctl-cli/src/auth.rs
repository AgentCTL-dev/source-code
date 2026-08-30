// SPDX-License-Identifier: Apache-2.0
//! `agentctl login` / `agentctl whoami` — the human side of the identity plane
//! (RFC 0028 §3). `login` runs the RFC 8628 device flow against the
//! agentctl-identity service: start → print the user code + verification URI →
//! poll until the human approves at the IdP → persist the session under
//! `~/.config/agentctl/credentials.json` (0600). `whoami` reads it back.
//!
//! The CLI speaks only the identity service's HTTP wire (`/v1/providers`,
//! `/v1/device/*`) — it never touches IdP endpoints or crate internals, so the
//! service can evolve custody without a CLI release.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Args)]
pub struct LoginArgs {
    /// Identity provider name (as configured on the identity service). Omitted
    /// with exactly one provider configured, that one is used.
    #[arg(long)]
    pub provider: Option<String>,
    /// Base URL of the agentctl-identity service (or AGENTCTL_IDENTITY_URL).
    /// In-cluster: http://agentctl-identity.<namespace>. From a workstation:
    /// `kubectl -n agentctl-system port-forward svc/agentctl-identity 8087:80`
    /// then http://127.0.0.1:8087.
    #[arg(long)]
    pub identity_url: Option<String>,
    /// Give up after this many seconds of polling.
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
}

#[derive(Args)]
pub struct WhoamiArgs {}

/// The persisted session (never the refresh token — custody stays with the
/// identity service; this file holds only the short-lived access token).
#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub identity_url: String,
    pub provider: String,
    pub access_token: String,
    /// Unix seconds after which `access_token` is dead.
    pub expires_unix: i64,
    /// The identity the service resolved at login (subject/email/groups).
    #[serde(default)]
    pub identity: Value,
}

pub async fn run_login(args: LoginArgs) -> Result<()> {
    let url = identity_url(args.identity_url.as_deref())?;
    let http = http_client()?;

    let provider = match args.provider {
        Some(p) => p,
        None => pick_sole_provider(&fetch_provider_names(&http, &url).await?)?,
    };

    let start: Value = post_json(
        &http,
        &url,
        "/v1/device/start",
        json!({ "provider": provider }),
    )
    .await
    .context("start device login")?;
    let handle = start["handle"]
        .as_str()
        .context("identity service returned no login handle")?
        .to_string();
    let user_code = start["user_code"].as_str().unwrap_or("?");
    let verification_uri = start["verification_uri"].as_str().unwrap_or("?");
    let interval = start["interval"].as_u64().unwrap_or(5).max(1);

    println!("To sign in, open:\n\n    {verification_uri}\n\nand enter the code: {user_code}\n");
    print!("Waiting for approval");
    std::io::stdout().flush().ok();

    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut wait = interval;
    loop {
        if std::time::Instant::now() >= deadline {
            println!();
            bail!(
                "login timed out after {}s; run `agentctl login` again",
                args.timeout
            );
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let poll: Value = post_json(&http, &url, "/v1/device/poll", json!({ "handle": handle }))
            .await
            .context("poll device login")?;
        match poll["status"].as_str() {
            Some("ok") => {
                println!();
                let creds = credentials_from_poll(&url, &provider, &poll, now_unix())?;
                let path = save_credentials(&creds)?;
                match creds.identity.get("subject").and_then(Value::as_str) {
                    Some(subject) => {
                        println!("Signed in as {subject} (saved to {}).", path.display())
                    }
                    None => println!("Signed in (saved to {}).", path.display()),
                }
                return Ok(());
            }
            Some("pending") => {
                print!(".");
                std::io::stdout().flush().ok();
            }
            Some("slow_down") => wait += 5,
            other => {
                println!();
                bail!("unexpected login status {other:?} from the identity service");
            }
        }
    }
}

pub async fn run_whoami(_args: WhoamiArgs) -> Result<()> {
    let path = credentials_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no saved session at {} — run `agentctl login`",
            path.display()
        )
    })?;
    let creds: Credentials = serde_json::from_str(&raw).with_context(|| {
        format!(
            "unreadable session file {} — run `agentctl login`",
            path.display()
        )
    })?;
    print!("{}", describe_credentials(&creds, now_unix()));
    Ok(())
}

/// Load the saved session for API calls (`agentctl chat`), refusing loudly
/// when absent or expired — a dead token would just bounce off the gateway.
pub fn load_session() -> Result<Credentials> {
    let path = credentials_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no saved session at {} — run `agentctl login`",
            path.display()
        )
    })?;
    let creds: Credentials = serde_json::from_str(&raw).with_context(|| {
        format!(
            "unreadable session file {} — run `agentctl login`",
            path.display()
        )
    })?;
    if creds.expires_unix <= now_unix() {
        bail!("your session has expired — run `agentctl login`");
    }
    Ok(creds)
}

/// The HTTP client other commands reuse (same TLS posture as login).
pub fn api_client() -> Result<reqwest::Client> {
    http_client()
}

// ===========================================================================
// Pure helpers (unit-tested below).
// ===========================================================================

/// Resolve the identity service URL: flag > AGENTCTL_IDENTITY_URL. No silent
/// default — a wrong guess would send a login somewhere surprising.
fn identity_url(flag: Option<&str>) -> Result<String> {
    let raw = match flag {
        Some(u) => u.to_string(),
        None => match std::env::var("AGENTCTL_IDENTITY_URL") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => bail!(
                "no identity service URL: pass --identity-url or set AGENTCTL_IDENTITY_URL \
                 (from a workstation: `kubectl -n agentctl-system port-forward \
                 svc/agentctl-identity 8087:80` then http://127.0.0.1:8087)"
            ),
        },
    };
    Ok(raw.trim_end_matches('/').to_string())
}

/// With exactly one provider configured, pick it; otherwise the human decides.
fn pick_sole_provider(names: &[String]) -> Result<String> {
    match names {
        [] => bail!("the identity service has no providers configured"),
        [only] => Ok(only.clone()),
        many => bail!(
            "multiple providers configured ({}); pick one with --provider",
            many.join(", ")
        ),
    }
}

/// Build the persisted session from a successful poll response.
fn credentials_from_poll(
    url: &str,
    provider: &str,
    poll: &Value,
    now_unix: i64,
) -> Result<Credentials> {
    let access_token = poll["access_token"]
        .as_str()
        .context("identity service returned no access token")?
        .to_string();
    let expires_in = poll["expires_in"].as_i64().unwrap_or(300);
    Ok(Credentials {
        identity_url: url.to_string(),
        provider: provider.to_string(),
        access_token,
        expires_unix: now_unix + expires_in,
        identity: poll.get("identity").cloned().unwrap_or(Value::Null),
    })
}

/// Human-readable whoami output.
fn describe_credentials(creds: &Credentials, now_unix: i64) -> String {
    let mut out = String::new();
    let subject = creds
        .identity
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("-");
    out.push_str(&format!("Subject:    {subject}\n"));
    if let Some(email) = creds.identity.get("email").and_then(Value::as_str) {
        out.push_str(&format!("Email:      {email}\n"));
    }
    if let Some(groups) = creds.identity.get("groups").and_then(Value::as_array) {
        let names: Vec<&str> = groups.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            out.push_str(&format!("Groups:     {}\n", names.join(", ")));
        }
    }
    out.push_str(&format!("Provider:   {}\n", creds.provider));
    out.push_str(&format!("Identity:   {}\n", creds.identity_url));
    let left = creds.expires_unix - now_unix;
    if left > 0 {
        out.push_str(&format!(
            "Session:    valid for {}\n",
            format_duration(left)
        ));
    } else {
        out.push_str("Session:    EXPIRED — run `agentctl login`\n");
    }
    out
}

/// Coarse single-unit duration, mirroring the table AGE style.
fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// `$AGENTCTL_CONFIG_DIR` (tests/overrides) or `~/.config/agentctl`.
fn credentials_path() -> Result<PathBuf> {
    let dir = match std::env::var("AGENTCTL_CONFIG_DIR") {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => {
            let home = std::env::var("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".config").join("agentctl")
        }
    };
    Ok(dir.join("credentials.json"))
}

/// Persist the session at 0600 (it holds a bearer). Returns the path written.
fn save_credentials(creds: &Credentials) -> Result<PathBuf> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(creds).expect("serializable");
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(path)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ===========================================================================
// Wire (reqwest over rustls with the EXPLICIT ring provider + webpki roots —
// the control-plane client pattern; plain-http URLs bypass TLS entirely).
// ===========================================================================

fn http_client() -> Result<reqwest::Client> {
    let provider = rustls::crypto::ring::default_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .timeout(Duration::from_secs(20))
        .build()
        .context("build http client")
}

async fn fetch_provider_names(http: &reqwest::Client, url: &str) -> Result<Vec<String>> {
    let v: Value = http
        .get(format!("{url}/v1/providers"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("reach the identity service")?
        .json()
        .await
        .context("read provider list")?;
    Ok(v["providers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

async fn post_json(http: &reqwest::Client, url: &str, path: &str, body: Value) -> Result<Value> {
    let resp = http
        .post(format!("{url}{path}"))
        .json(&body)
        .send()
        .await
        .context("reach the identity service")?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = v["error"].as_str().unwrap_or("unexpected response");
        bail!("identity service refused ({status}): {msg}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_url_requires_a_source_and_trims_slash() {
        // Explicit flag wins and the trailing slash is normalized away.
        assert_eq!(
            identity_url(Some("http://id.example/")).unwrap(),
            "http://id.example"
        );
    }

    #[test]
    fn sole_provider_is_picked_and_ambiguity_is_refused() {
        assert_eq!(pick_sole_provider(&["okta".into()]).unwrap(), "okta");
        assert!(pick_sole_provider(&[]).is_err());
        let err = pick_sole_provider(&["okta".into(), "auth0".into()]).unwrap_err();
        assert!(format!("{err}").contains("okta, auth0"));
    }

    #[test]
    fn credentials_capture_the_poll_response() {
        let poll = json!({
            "status": "ok",
            "access_token": "at-1",
            "expires_in": 300,
            "identity": { "subject": "okta:alice", "email": "alice@acme.test", "groups": ["eng"] },
        });
        let creds = credentials_from_poll("http://id", "okta", &poll, 1_000).unwrap();
        assert_eq!(creds.access_token, "at-1");
        assert_eq!(creds.expires_unix, 1_300);
        assert_eq!(creds.identity["subject"], "okta:alice");
        // A refresh token in the response would be a custody leak — the shape
        // never carries one, and the poll response never includes one either.
        assert!(!serde_json::to_string(&creds).unwrap().contains("refresh"));
    }

    #[test]
    fn missing_access_token_is_an_error() {
        assert!(credentials_from_poll("u", "p", &json!({ "status": "ok" }), 0).is_err());
    }

    #[test]
    fn whoami_reports_the_session() {
        let creds = Credentials {
            identity_url: "http://id".into(),
            provider: "okta".into(),
            access_token: "sekrit-bearer-x".into(),
            expires_unix: 1_000 + 240,
            identity: json!({ "subject": "okta:alice", "email": "alice@acme.test", "groups": ["eng", "sre"] }),
        };
        let text = describe_credentials(&creds, 1_000);
        assert!(text.contains("Subject:    okta:alice"));
        assert!(text.contains("Groups:     eng, sre"));
        assert!(text.contains("valid for 4m"));
        // The bearer itself never appears in whoami output.
        assert!(
            !text.contains("sekrit-bearer-x"),
            "token leaked into output: {text}"
        );

        let expired = describe_credentials(
            &Credentials {
                expires_unix: 999,
                ..creds
            },
            1_000,
        );
        assert!(expired.contains("EXPIRED"));
    }

    #[test]
    fn duration_formatting_is_single_unit() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(240), "4m");
        assert_eq!(format_duration(7_200), "2h");
    }
}
