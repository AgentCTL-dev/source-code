// SPDX-License-Identifier: BUSL-1.1
//! A minimal, dependency-light S3 client: exactly the three operations the
//! artifacts façade needs (PUT / GET object, paginated LIST), signed with AWS
//! Signature V4 over `reqwest` + `sha2`/`hmac`. Hand-rolled on purpose — every
//! off-the-shelf S3 crate drags in an OpenSSL, MPL, or vulnerable-XML tail this
//! rustls-only, deny-clean workspace won't carry.
//!
//! Two simplifications keep signing exact and mismatch-free:
//! - **keys are safe-charset** (`[A-Za-z0-9/_.-]`, enforced by the caller), so
//!   the request path needs no percent-encoding and the signed canonical URI is
//!   the path verbatim — no encoder round-trip to disagree with `reqwest`;
//! - **pagination is `start-after`** (one of our own safe keys), never the
//!   opaque `continuation-token`, so the LIST query stays safe-charset too.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub struct S3 {
    pub http: reqwest::Client,
    /// Scheme + authority, no trailing slash, e.g. `http://minio:9000`.
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access: String,
    pub secret: String,
}

pub struct Obj {
    pub key: String,
    pub size: u64,
    pub last_modified: String,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, datestamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

impl S3 {
    /// The `host[:port]` the `Host` header (and the signed `host`) carries.
    fn host(&self) -> &str {
        self.endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.endpoint)
    }

    /// The SigV4 headers for one request. `canonical_query` is the already-sorted,
    /// already-encoded query string (empty for object ops).
    fn auth(
        &self,
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
        payload_hash: &str,
    ) -> Vec<(String, String)> {
        let now = jiff::Timestamp::now();
        let amzdate = now.strftime("%Y%m%dT%H%M%SZ").to_string();
        let datestamp = now.strftime("%Y%m%d").to_string();
        let host = self.host();
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amzdate}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{datestamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amzdate}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signature = hex::encode(hmac(
            &signing_key(&self.secret, &datestamp, &self.region, "s3"),
            string_to_sign.as_bytes(),
        ));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access
        );
        vec![
            ("authorization".into(), authorization),
            ("x-amz-date".into(), amzdate),
            ("x-amz-content-sha256".into(), payload_hash.to_string()),
        ]
    }

    /// Create the bucket if it does not exist (idempotent — an existing bucket
    /// we own returns 409, which is success here). MinIO does not
    /// auto-create, and we would rather not ship a separate `mc` init Job.
    pub async fn ensure_bucket(&self) -> Result<(), String> {
        let canonical_uri = format!("/{}", self.bucket);
        let url = format!("{}{canonical_uri}", self.endpoint);
        let mut req = self.http.put(&url);
        for (k, v) in self.auth("PUT", &canonical_uri, "", EMPTY_SHA256) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("ensure_bucket status {status}: {}", body.trim()))
    }

    pub async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), String> {
        let canonical_uri = format!("/{}/{}", self.bucket, key);
        let payload_hash = sha256_hex(&body);
        let url = format!("{}{canonical_uri}", self.endpoint);
        let mut req = self.http.put(&url);
        for (k, v) in self.auth("PUT", &canonical_uri, "", &payload_hash) {
            req = req.header(k, v);
        }
        // content-type is not in SignedHeaders — sent, not signed, so it is
        // stored without disturbing the signature.
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        let resp = req.body(body).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("status {status}: {}", body.trim()));
        }
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let canonical_uri = format!("/{}/{}", self.bucket, key);
        let url = format!("{}{canonical_uri}", self.endpoint);
        let mut req = self.http.get(&url);
        for (k, v) in self.auth("GET", &canonical_uri, "", EMPTY_SHA256) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("status {status}: {}", body.trim()));
        }
        Ok(Some(
            resp.bytes().await.map_err(|e| e.to_string())?.to_vec(),
        ))
    }

    /// Every object under `prefix`, following `start-after` pagination. `prefix`
    /// is safe-charset, so the only query value needing encoding is `/` → `%2F`.
    pub async fn list_all(&self, prefix: &str) -> Result<Vec<Obj>, String> {
        let mut out = Vec::new();
        let mut start_after: Option<String> = None;
        loop {
            let (mut page, truncated, last) =
                self.list_page(prefix, start_after.as_deref()).await?;
            out.append(&mut page);
            if truncated {
                match last {
                    Some(k) => start_after = Some(k),
                    None => break,
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> Result<(Vec<Obj>, bool, Option<String>), String> {
        // ListObjectsV2 on the bucket. Canonical query = sorted, encoded params.
        let mut params: Vec<(String, String)> = vec![
            ("list-type".into(), "2".into()),
            ("max-keys".into(), "1000".into()),
            ("prefix".into(), q_encode(prefix)),
        ];
        if let Some(sa) = start_after {
            params.push(("start-after".into(), q_encode(sa)));
        }
        params.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let canonical_uri = format!("/{}", self.bucket);
        let url = format!("{}{canonical_uri}?{canonical_query}", self.endpoint);
        let mut req = self.http.get(&url);
        for (k, v) in self.auth("GET", &canonical_uri, &canonical_query, EMPTY_SHA256) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("status {status}: {}", body.trim()));
        }
        let xml = resp.text().await.map_err(|e| e.to_string())?;
        Ok(parse_list(&xml))
    }
}

/// Percent-encode a query value: everything but the RFC3986 unreserved set
/// (`A-Za-z0-9-_.~`) — for our safe-charset inputs that means only `/` → `%2F`.
fn q_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Extract `<Contents>` rows and the truncation flag from a ListObjectsV2 body.
/// Our keys are safe-charset, so no XML-entity unescaping is needed.
fn parse_list(xml: &str) -> (Vec<Obj>, bool, Option<String>) {
    let truncated = between(xml, "<IsTruncated>", "</IsTruncated>")
        .map(|v| v.trim() == "true")
        .unwrap_or(false);
    let mut objs = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let block_start = start + "<Contents>".len();
        let Some(end_rel) = rest[block_start..].find("</Contents>") else {
            break;
        };
        let block = &rest[block_start..block_start + end_rel];
        if let Some(key) = between(block, "<Key>", "</Key>") {
            let size = between(block, "<Size>", "</Size>")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let last_modified = between(block, "<LastModified>", "</LastModified>")
                .unwrap_or("")
                .to_string();
            objs.push(Obj {
                key: key.to_string(),
                size,
                last_modified,
            });
        }
        rest = &rest[block_start + end_rel + "</Contents>".len()..];
    }
    let last = objs.last().map(|o| o.key.clone());
    (objs, truncated, last)
}

fn between<'a>(hay: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let s = hay.find(open)? + open.len();
    let e = hay[s..].find(close)? + s;
    Some(&hay[s..e])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_matches_the_aws_worked_example() {
        // AWS SigV4 documentation's canonical example vectors.
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex::encode(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn empty_sha256_constant_is_correct() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }

    #[test]
    fn q_encode_only_touches_the_slash_for_safe_input() {
        assert_eq!(q_encode("orgs/org-acme/"), "orgs%2Forg-acme%2F");
        assert_eq!(q_encode("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn parse_list_reads_contents_and_truncation() {
        let xml = r#"<?xml version="1.0"?><ListBucketResult>
          <IsTruncated>false</IsTruncated>
          <Contents><Key>orgs/o/a.txt</Key><Size>12</Size><LastModified>2026-08-31T00:00:00.000Z</LastModified></Contents>
          <Contents><Key>orgs/o/b.txt</Key><Size>34</Size><LastModified>2026-08-31T00:01:00.000Z</LastModified></Contents>
        </ListBucketResult>"#;
        let (objs, truncated, last) = parse_list(xml);
        assert!(!truncated);
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].key, "orgs/o/a.txt");
        assert_eq!(objs[0].size, 12);
        assert_eq!(objs[1].size, 34);
        assert_eq!(last.as_deref(), Some("orgs/o/b.txt"));
    }
}
