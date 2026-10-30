//! Storing a blob on Walrus through the **internal publisher** (ADR-0007): a `BlobStore` port, and the implementation that
//! calls Mysten's `walrus publisher` (`PUT /v1/blobs?epochs=N&permanent=true`).
//!
//! The publisher is reachable only from the hub's namespace. When it runs with `--jwt-decode-secret`, every request must
//! carry an HS256 JWT; the hub mints one per request with an `exp` 60 s ahead, a fresh `jti`, the exact `epochs` and the
//! exact `size` of the blob (claims as in upstream `walrus-service/src/client/daemon/auth.rs`, checked 2026-10-29), so a
//! token cannot be replayed or reused for a different upload. The reply is either `newlyCreated` (with the `Blob` object
//! and its storage end epoch) or `alreadyCertified` (the blob was already stored for long enough).

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use serde_json::{Value, json};

use crate::net::{Failure, Http, Request};

/// A blob that is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlob {
    pub blob_id: String,
    /// The Sui `Blob` object (absent when the blob was already certified).
    pub object_id: Option<String>,
    /// The epoch the storage ends at, as the publisher reports it.
    pub end_epoch: Option<u64>,
    pub already_certified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    /// The publisher answered with a refusal.
    #[error("the publisher refused the blob (HTTP {status}): {reason}")]
    Rejected { status: u16, reason: String },
    /// No answer, or the connection was never made.
    #[error("the publisher could not be reached: {0}")]
    Unreachable(String),
    /// An answer that is not what the publisher documents.
    #[error("unexpected publisher reply: {0}")]
    Unexpected(String),
}

/// Where snapshots (and later site files) are stored.
pub trait BlobStore: Send + Sync {
    /// Store `bytes` for `epochs` epochs.
    fn put(&self, bytes: &[u8], epochs: u32) -> impl Future<Output = Result<StoredBlob, BlobError>> + Send;
}

/// A Walrus blob id: 32 bytes, base64url without padding (43 characters).
pub fn is_blob_id(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_object_id(s: &str) -> bool {
    s.len() == 66 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// The publisher's reply to a store.
pub fn parse_response(body: &str) -> Result<StoredBlob, BlobError> {
    let v: Value = serde_json::from_str(body).map_err(|_| BlobError::Unexpected("not JSON".into()))?;
    let blob_id = |x: &Value| x.as_str().filter(|s| is_blob_id(s)).map(str::to_owned);
    if let Some(n) = v.get("newlyCreated") {
        let o = n.get("blobObject").ok_or_else(|| BlobError::Unexpected("newlyCreated without blobObject".into()))?;
        let id = blob_id(o.get("blobId").unwrap_or(&Value::Null))
            .ok_or_else(|| BlobError::Unexpected("no valid blobId".into()))?;
        let object_id = o.get("id").and_then(Value::as_str).filter(|s| is_object_id(s)).map(str::to_owned);
        let end_epoch = o.pointer("/storage/endEpoch").and_then(Value::as_u64);
        return Ok(StoredBlob { blob_id: id, object_id, end_epoch, already_certified: false });
    }
    if let Some(a) = v.get("alreadyCertified") {
        let id = blob_id(a.get("blobId").unwrap_or(&Value::Null))
            .ok_or_else(|| BlobError::Unexpected("no valid blobId".into()))?;
        return Ok(StoredBlob {
            blob_id: id,
            object_id: None,
            end_epoch: a.get("endEpoch").and_then(Value::as_u64),
            already_certified: true,
        });
    }
    Err(BlobError::Unexpected("neither newlyCreated nor alreadyCertified".into()))
}

/// The HMAC key from the configured secret: `0x…` is hex, anything else is the UTF-8 bytes (as the publisher reads it).
pub fn jwt_secret_bytes(text: &str) -> Result<Vec<u8>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("the publisher JWT secret is empty".into());
    }
    match t.strip_prefix("0x") {
        None => Ok(t.as_bytes().to_vec()),
        Some(hex) => {
            if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("the publisher JWT secret starts with 0x but is not hex".into());
            }
            Ok((0..hex.len()).step_by(2).filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect())
        }
    }
}

/// A compact JWT signed with HS256.
pub fn jwt_hs256(secret: &[u8], claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let input = format!("{header}.{payload}");
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
}

/// The claims for one store: expires in 60 s, unique, bound to the exact epochs and size.
pub fn claims(now_unix: i64, jti: &str, epochs: u32, size: usize) -> Value {
    json!({ "iat": now_unix, "exp": now_unix + 60, "jti": jti, "epochs": epochs, "size": size })
}

/// The internal Walrus publisher.
pub struct Publisher {
    http: Http,
    base: String,
    secret: Option<Vec<u8>>,
}

impl Publisher {
    /// `base` is the publisher's URL, for example `http://walrus-storage-publisher.blockchain.svc:31415`; the client may
    /// call only that host. `jwt_secret` is `WALRUS_PUBLISHER_JWT_SECRET` (absent: no token, for a publisher without
    /// `--jwt-decode-secret`).
    pub fn new(base: &str, jwt_secret: Option<&str>) -> Result<Publisher, String> {
        let url = url::Url::parse(base).map_err(|_| "the publisher URL is not a URL".to_owned())?;
        if !matches!(url.scheme(), "http" | "https") || url.path() != "/" || url.query().is_some() {
            return Err("the publisher URL is http(s)://host[:port] with nothing after it".into());
        }
        let host = url.host_str().ok_or("the publisher URL has no host")?.to_owned();
        let secret = jwt_secret.map(jwt_secret_bytes).transpose()?;
        Ok(Publisher { http: Http::internal(&host)?, base: base.trim_end_matches('/').to_owned(), secret })
    }

    #[doc(hidden)]
    pub fn with_client(http: Http, base: &str, jwt_secret: Option<&str>) -> Result<Publisher, String> {
        Ok(Publisher {
            http,
            base: base.trim_end_matches('/').to_owned(),
            secret: jwt_secret.map(jwt_secret_bytes).transpose()?,
        })
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn jti() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        return format!("{}-{}", unix_now(), std::process::id());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl BlobStore for Publisher {
    async fn put(&self, bytes: &[u8], epochs: u32) -> Result<StoredBlob, BlobError> {
        // `permanent=true` so the result does not depend on the publisher's default (its documentation contradicts itself).
        let url = format!("{}/v1/blobs?epochs={epochs}&permanent=true", self.base);
        let mut req = Request::put_bytes(url, bytes.to_vec());
        if let Some(secret) = &self.secret {
            let token = jwt_hs256(secret, &claims(unix_now(), &jti(), epochs, bytes.len()));
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let reply = self.http.send(req).await.map_err(|f| match f {
            Failure::NotSent(r) | Failure::Unknown(r) | Failure::Refused(r) => BlobError::Unreachable(r),
        })?;
        if !reply.is_success() {
            let reason = content_hub_core::delivery::clip(&reply.body, 300);
            return Err(BlobError::Rejected { status: reply.status, reason });
        }
        parse_response(&reply.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = "Qc9aP0V6x3mYk2w1TnqL8sRZb4dEoFhJiUyXvCgA7MM";

    #[test]
    fn the_hs256_signature_matches_rfc_7515() {
        // RFC 7515 appendix A.1: the HMAC over this signing input with this key.
        let key = URL_SAFE_NO_PAD
            .decode("AyM1SysPpbyDfgZld3umj1qzKObwVMkoqQ-EstJQLr_T-1qS0gZH75aKtMN3Yj0iPS4hcgUuTwjAzZr1Z9CAow")
            .expect("key");
        let input = "eyJ0eXAiOiJKV1QiLA0KICJhbGciOiJIUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ";
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), input.as_bytes());
        assert_eq!(URL_SAFE_NO_PAD.encode(tag.as_ref()), "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    }

    #[test]
    fn tokens_are_compact_hs256_with_the_exact_claims() {
        let c = claims(1_000, "abc", 30, 715);
        assert_eq!(c, json!({"iat": 1000, "exp": 1060, "jti": "abc", "epochs": 30, "size": 715}));
        let t = jwt_hs256(b"secret", &c);
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[0]).expect("h"), br#"{"alg":"HS256","typ":"JWT"}"#);
        let body: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("p")).expect("json");
        assert_eq!(body, c);
        // The signature is over `header.payload` with the secret.
        let tag =
            hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, b"secret"), format!("{}.{}", parts[0], parts[1]).as_bytes());
        assert_eq!(URL_SAFE_NO_PAD.encode(tag.as_ref()), parts[2]);
        assert_ne!(jti(), jti());
    }

    #[test]
    fn secrets_are_hex_when_prefixed() {
        assert_eq!(jwt_secret_bytes("0x0aff").expect("hex"), vec![0x0a, 0xff]);
        assert_eq!(jwt_secret_bytes(" plain-secret ").expect("raw"), b"plain-secret");
        for bad in ["", "0x", "0xabc", "0xzz"] {
            assert!(jwt_secret_bytes(bad).is_err(), "{bad}");
        }
    }

    /// Fixtures follow the documented shapes (docs.wal.app → Web API: newlyCreated / alreadyCertified).
    #[test]
    fn replies() {
        let created = format!(
            r#"{{"newlyCreated":{{"blobObject":{{"id":"0x{}","registeredEpoch":5,"blobId":"{BLOB}","size":715,"encodingType":"RS2","certifiedEpoch":5,"storage":{{"id":"0x{}","startEpoch":5,"endEpoch":35,"storageSize":1000}},"deletable":false}},"resourceOperation":{{"registerFromScratch":{{"encodedLength":1000,"epochsAhead":30}}}},"cost":100}}}}"#,
            "ab".repeat(32),
            "cd".repeat(32)
        );
        assert_eq!(
            parse_response(&created),
            Ok(StoredBlob {
                blob_id: BLOB.into(),
                object_id: Some(format!("0x{}", "ab".repeat(32))),
                end_epoch: Some(35),
                already_certified: false
            })
        );
        let already = format!(
            r#"{{"alreadyCertified":{{"blobId":"{BLOB}","event":{{"txDigest":"x","eventSeq":"0"}},"endEpoch":40}}}}"#
        );
        assert_eq!(
            parse_response(&already),
            Ok(StoredBlob { blob_id: BLOB.into(), object_id: None, end_epoch: Some(40), already_certified: true })
        );
        for bad in ["", "{}", r#"{"newlyCreated":{}}"#, r#"{"alreadyCertified":{"blobId":"short"}}"#, "[1]"] {
            assert!(matches!(parse_response(bad), Err(BlobError::Unexpected(_))), "{bad}");
        }
        assert!(is_blob_id(BLOB) && !is_blob_id("nope"));
    }

    #[test]
    fn the_publisher_url_must_be_a_bare_origin() {
        assert!(Publisher::new("http://walrus-storage-publisher.blockchain.svc:31415", Some("0x0a")).is_ok());
        for bad in ["ftp://x", "http://x/v1", "http://x/?a=1", "nonsense"] {
            assert!(Publisher::new(bad, None).is_err(), "{bad}");
        }
        assert!(Publisher::new("http://x", Some("0xzz")).is_err());
    }
}
