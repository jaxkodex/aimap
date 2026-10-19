//! Port of `src/aimap/storage.py`: S3 layout and checkpoint storage.
//!
//! Keys (all under S3_PREFIX):
//!
//! ```text
//! raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml    original RFC 822 bytes
//! state/<account>/<mailbox>.json                     legacy checkpoints
//! ```
//!
//! IMAP UIDs are only unique within one UIDVALIDITY, so it is part of the key.
//! Uploads use the same key for the same message, so re-running a batch after
//! a crash overwrites instead of duplicating.
//!
//! The S3 protocol calls live behind [`S3Client`] (Python injects a boto3
//! client the same way; tests use an in-memory fake like moto).

use serde_json::Value;

/// The full S3Config lives in [`crate::config`] (as in Python); re-exported
/// here because everything storage touches goes through it.
pub use crate::config::S3Config;

/// Make an account or mailbox name safe as one key segment
/// (`"[Gmail]/All Mail"` has a slash). Mirrors
/// `urllib.parse.quote(value, safe="@.-_+")`.
pub fn seg(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'@' | b'+' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `urllib.parse.unquote` (UTF-8, invalid sequences replaced).
pub fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(hex) = bytes.get(i + 1..i + 3) {
                if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16) {
                    decoded.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `ConflictError`: a conditional write lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictError(pub String);

impl std::fmt::Display for ConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ConflictError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub uidvalidity: i64,
    pub last_uid: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawKey {
    pub account: String,
    pub mailbox: String,
    pub uidvalidity: i64,
    pub uid: i64,
}

/// Models a botocore `ClientError` error code ("NoSuchKey", "PreconditionFailed", …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Error {
    pub code: String,
}

impl S3Error {
    pub fn new(code: &str) -> S3Error {
        S3Error { code: code.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    pub body: Vec<u8>,
    pub etag: String,
    pub metadata: Vec<(String, String)>,
}

/// Conditional-write precondition on `put_object`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutCondition {
    None,
    IfMatch(String),
    IfNoneMatchAny,
}

/// The subset of the S3 API the store uses.
pub trait S3Client {
    fn head_object(&self, bucket: &str, key: &str) -> Result<Option<String>, S3Error>;
    fn get_object(&self, bucket: &str, key: &str) -> Result<Option<S3Object>, S3Error>;
    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: &[u8],
        content_type: &str,
        metadata: &[(String, String)],
        condition: &PutCondition,
    ) -> Result<String, S3Error>;
    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, S3Error>;
    fn head_bucket(&self, bucket: &str) -> Result<(), S3Error>;
}

/// Failures the Store reports: lost conditional writes, S3 errors, malformed
/// checkpoint payloads.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreError {
    Conflict(ConflictError),
    S3(S3Error),
    Payload(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(e) => write!(f, "{e}"),
            StoreError::S3(e) => write!(f, "s3 error: {}", e.code),
            StoreError::Payload(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for StoreError {}

impl From<S3Error> for StoreError {
    fn from(e: S3Error) -> Self {
        StoreError::S3(e)
    }
}

fn missing(code: &str) -> bool {
    matches!(code, "NoSuchKey" | "404" | "NotFound")
}

/// S3 user metadata must be ASCII (`_ascii` in storage.py:
/// `encode("ascii", "backslashreplace")[:1024]`).
pub fn ascii_sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let cp = c as u32;
            if cp < 0x100 {
                out.push_str(&format!("\\x{cp:02x}"));
            } else if cp < 0x10000 {
                out.push_str(&format!("\\u{cp:04x}"));
            } else {
                out.push_str(&format!("\\U{cp:08x}"));
            }
        }
    }
    out.chars().take(1024).collect()
}

pub struct Store<C: S3Client> {
    pub cfg: S3Config,
    pub client: C,
}

/// `(body, user metadata)` of a stored message.
pub type RawWithMetadata = (Vec<u8>, Vec<(String, String)>);

impl<C: S3Client> Store<C> {
    pub fn new(cfg: S3Config, client: C) -> Self {
        Store { cfg, client }
    }

    pub fn key(&self, path: &str) -> String {
        format!("{}{}", self.cfg.prefix, path.trim_start_matches('/'))
    }

    pub fn head_etag(&self, key: &str) -> Result<Option<String>, S3Error> {
        match self.client.head_object(&self.cfg.bucket, key) {
            Ok(etag) => Ok(etag),
            Err(e) if missing(&e.code) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `(body, etag)`, or None if the object does not exist.
    pub fn get_bytes(&self, key: &str) -> Result<Option<(Vec<u8>, String)>, S3Error> {
        match self.client.get_object(&self.cfg.bucket, key) {
            Ok(Some(obj)) => Ok(Some((obj.body, obj.etag))),
            Ok(None) => Ok(None),
            Err(e) if missing(&e.code) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write only if the object still has expect_etag (None: only if it does
    /// not exist yet). Returns the new etag.
    pub fn put_bytes(
        &self,
        key: &str,
        body: &[u8],
        content_type: &str,
        expect_etag: Option<&str>,
    ) -> Result<String, StoreError> {
        let condition = match expect_etag {
            Some(etag) => PutCondition::IfMatch(etag.to_string()),
            None => PutCondition::IfNoneMatchAny,
        };
        match self.client.put_object(&self.cfg.bucket, key, body, content_type, &[], &condition) {
            Ok(etag) => Ok(etag),
            Err(e) if matches!(e.code.as_str(), "PreconditionFailed" | "ConditionalRequestConflict" | "412") => {
                Err(StoreError::Conflict(ConflictError(key.to_string())))
            }
            Err(e) => Err(StoreError::S3(e)),
        }
    }

    pub fn raw_key(&self, account: &str, mailbox: &str, uidvalidity: i64, uid: i64) -> String {
        format!(
            "{}raw/{}/{}/{}/{}.eml",
            self.cfg.prefix,
            seg(account),
            seg(mailbox),
            uidvalidity,
            uid
        )
    }

    pub fn state_key(&self, account: &str, mailbox: &str) -> String {
        format!("{}state/{}/{}.json", self.cfg.prefix, seg(account), seg(mailbox))
    }

    /// Fail fast at startup if the bucket is unreachable or credentials are wrong.
    pub fn check(&self) -> Result<(), S3Error> {
        self.client.head_bucket(&self.cfg.bucket)
    }

    pub fn put_raw(
        &self,
        account: &str,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        raw: &[u8],
        metadata: Option<&[(String, String)]>,
    ) -> Result<String, S3Error> {
        let key = self.raw_key(account, mailbox, uidvalidity, uid);
        let md: Vec<(String, String)> = metadata
            .unwrap_or(&[])
            .iter()
            .map(|(k, v)| (k.clone(), ascii_sanitize(v)))
            .collect();
        self.client
            .put_object(&self.cfg.bucket, &key, raw, "message/rfc822", &md, &PutCondition::None)?;
        Ok(key)
    }

    /// `(body, user metadata)` of a stored message, or None if it is gone.
    /// `(body, user metadata)` of a stored message, or None if it is gone.
    pub fn get_raw(&self, key: &str) -> Result<Option<RawWithMetadata>, S3Error> {
        match self.client.get_object(&self.cfg.bucket, key) {
            Ok(Some(obj)) => Ok(Some((obj.body, obj.metadata))),
            Ok(None) => Ok(None),
            Err(e) if missing(&e.code) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every key under raw/, in S3 listing order.
    pub fn list_raw(&self) -> Result<Vec<String>, S3Error> {
        self.client.list_objects(&self.cfg.bucket, &format!("{}raw/", self.cfg.prefix))
    }

    pub fn parse_raw_key(&self, key: &str) -> Option<RawKey> {
        let prefix = format!("{}raw/", self.cfg.prefix);
        let rest = key.strip_prefix(&prefix)?;
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 4 || !parts[3].ends_with(".eml") {
            return None;
        }
        let uid = parts[3].strip_suffix(".eml")?;
        Some(RawKey {
            account: unquote(parts[0]),
            mailbox: unquote(parts[1]),
            uidvalidity: parts[2].parse().ok()?,
            uid: uid.parse().ok()?,
        })
    }

    /// Checkpoint written to S3 by versions before Postgres. Only read, to
    /// import it once.
    pub fn legacy_checkpoint(&self, account: &str, mailbox: &str) -> Result<Option<Checkpoint>, StoreError> {
        let Some((body, _)) = self
            .get_bytes(&self.state_key(account, mailbox))
            .map_err(StoreError::S3)?
        else {
            return Ok(None);
        };
        let data: Value = serde_json::from_slice(&body)
            .map_err(|e| StoreError::Payload(format!("bad checkpoint json: {e}")))?;
        let int_of = |v: &Value| -> Option<i64> {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        };
        Ok(Some(Checkpoint {
            uidvalidity: int_of(data.get("uidvalidity").ok_or_else(|| {
                StoreError::Payload("checkpoint missing uidvalidity".to_string())
            })?)
            .ok_or_else(|| StoreError::Payload("bad uidvalidity".to_string()))?,
            last_uid: int_of(data.get("last_uid").ok_or_else(|| {
                StoreError::Payload("checkpoint missing last_uid".to_string())
            })?)
            .ok_or_else(|| StoreError::Payload("bad last_uid".to_string()))?,
        }))
    }
}
