//! Real S3 transport: an [`S3Client`] over HTTP with AWS Signature V4.
//!
//! Signing is implemented here (it is ~80 lines and golden-tested against a
//! Python-computed signature in `tests/sigv4_golden.json`, which pins it to
//! the AWS-published algorithm botocore uses). The HTTP client is `ureq`
//! without TLS: production endpoints (`https://s3.<region>.amazonaws.com`)
//! come online with the pure-Rust rustls provider in the API batch; today the
//! transport speaks plain HTTP to S3-compatible servers (moto, SeaweedFS,
//! MinIO) — the integration test runs against moto, the same engine the
//! Python test suite uses.

use jiff::Timestamp;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::storage::{PutCondition, S3Client, S3Error, S3Object};

/// Endpoint, region and credentials — the part of S3Config the boto3 client
/// consumes in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Settings {
    /// e.g. `http://127.0.0.1:5000` (moto/SeaweedFS); None = AWS proper.
    pub endpoint_url: Option<String>,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// false = virtual-host addressing on AWS (`bucket.s3.region.amazonaws.com`),
    /// true / custom endpoint = path style (`host/bucket`).
    pub force_path_style: bool,
    /// "https" (default) or "http" (plain HTTP: local servers, and AWS S3
    /// accepts it in us-east-1 — used to validate signatures without TLS).
    pub scheme: String,
}

pub struct HttpS3Client {
    settings: S3Settings,
    bucket: String,
    agent: ureq::Agent,
    /// deterministic clock for tests
    fixed_time: Option<Timestamp>,
}

impl HttpS3Client {
    pub fn new(bucket: impl Into<String>, settings: S3Settings) -> Self {
        HttpS3Client {
            settings,
            bucket: bucket.into(),
            agent: ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(60)).build(),
            fixed_time: None,
        }
    }

    /// Pin the signing clock (tests).
    pub fn with_fixed_time(mut self, at: Timestamp) -> Self {
        self.fixed_time = Some(at);
        self
    }

    /// Test hook: sign a request on fixed inputs (golden tests).
    #[allow(clippy::too_many_arguments)]
    pub fn sign_public(
        &self,
        method: &str,
        key_path: &str,
        query: &[(String, String)],
        content_type: Option<&str>,
        extra_amz: &[(String, String)],
        payload: &[u8],
    ) -> SignedRequest {
        self.sign(method, key_path, query, content_type, extra_amz, payload)
    }

    /// Test hook: replace settings (golden tests).
    pub fn set_settings_public(&mut self, settings: S3Settings) {
        self.settings = settings;
    }

    fn now(&self) -> Timestamp {
        self.fixed_time.unwrap_or_else(Timestamp::now)
    }

    fn base_url(&self) -> String {
        let scheme = if self.settings.scheme.is_empty() { "https" } else { &self.settings.scheme };
        match (&self.settings.endpoint_url, self.settings.force_path_style) {
            (None, false) => format!("{}://{}.s3.{}.amazonaws.com", scheme, self.bucket, self.settings.region),
            (None, true) => format!("{}://s3.{}.amazonaws.com/{}", scheme, self.settings.region, self.bucket),
            (Some(endpoint), _) => format!("{}/{}", endpoint.trim_end_matches('/'), self.bucket),
        }
    }

    /// Build the URL and SigV4 headers for a request to `key_path` (already
    /// one-encoded key path under the bucket) with `query` params.
    fn sign(
        &self,
        method: &str,
        key_path: &str,
        query: &[(String, String)],
        content_type: Option<&str>,
        extra_amz: &[(String, String)],
        payload: &[u8],
    ) -> SignedRequest {
        let now = self.now();
        let amz_date = now.strftime("%Y%m%dT%H%M%SZ").to_string();
        let datestamp = now.strftime("%Y%m%d").to_string();
        let payload_hash = sha256_hex(payload);

        // canonical URI: the encoded path (key_path is already URI-encoded per segment)
        let canonical_uri = format!("/{}", key_path.trim_start_matches('/'));

        let mut amz_headers: Vec<(String, String)> = vec![
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        amz_headers.extend(extra_amz.iter().cloned());
        amz_headers.sort_by(|a, b| a.0.cmp(&b.0));

        // canonical headers: host + every x-amz-* (+ content-type when set)
        let mut header_pairs: Vec<(String, String)> = vec![("host".to_string(), host_of(&self.base_url()))];
        if let Some(ct) = content_type {
            header_pairs.push(("content-type".to_string(), ct.to_string()));
        }
        header_pairs.extend(amz_headers.iter().cloned());
        header_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers: String = header_pairs
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k.to_lowercase(), v.trim()))
            .collect();
        let signed_headers: Vec<String> = header_pairs.iter().map(|(k, _)| k.to_lowercase()).collect();
        let signed_headers = signed_headers.join(";");

        // canonical query: sorted by key, then value; RFC 3986 encoded
        let mut query_sorted: Vec<(String, String)> = query.to_vec();
        query_sorted.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let canonical_query = query_sorted
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let canonical_request = [
            method,
            canonical_uri.as_str(),
            canonical_query.as_str(),
            canonical_headers.as_str(),
            signed_headers.as_str(),
            payload_hash.as_str(),
        ]
        .join("\n");

        let scope = format!("{}/{}/s3/aws4_request", datestamp, self.settings.region);
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            amz_date.as_str(),
            scope.as_str(),
            sha256_hex(canonical_request.as_bytes()).as_str(),
        ]
        .join("\n");

        let signing_key = {
            let k = hmac_sha256(format!("AWS4{}", self.settings.secret_access_key).as_bytes(), datestamp.as_bytes());
            let k = hmac_sha256(&k, self.settings.region.as_bytes());
            let k = hmac_sha256(&k, b"s3");
            hmac_sha256(&k, b"aws4_request")
        };
        let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        let url = match query_sorted.is_empty() {
            true => format!("{}{}", self.base_url(), canonical_uri),
            false => format!("{}{}?{}", self.base_url(), canonical_uri, canonical_query),
        };
        let mut headers = vec![
            ("Authorization".to_string(),
             format!("AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
                     self.settings.access_key_id, scope, signed_headers, signature)),
            ("x-amz-date".to_string(), amz_date),
            ("x-amz-content-sha256".to_string(), payload_hash),
        ];
        if let Some(ct) = content_type {
            headers.push(("Content-Type".to_string(), ct.to_string()));
        }
        headers.extend(extra_amz.iter().cloned());
        SignedRequest { method: method.to_string(), url, headers }
    }

    fn send(&self, req: &SignedRequest, body: Option<&[u8]>) -> Result<ureq::Response, S3Error> {
        let mut r = self.agent.request(&req.method, &req.url);
        for (k, v) in &req.headers {
            r = r.set(k, v);
        }
        let result = match body {
            Some(b) => r.send_bytes(b),
            None => r.call(),
        };
        match result {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(_, resp)) => Ok(resp), // non-2xx handled by callers
            Err(ureq::Error::Transport(t)) => Err(S3Error { code: format!("NetworkError: {}", t) }),
        }
    }

    fn error_code(resp: ureq::Response) -> S3Error {
        // S3 error XML: <Error><Code>NoSuchKey</Code>...; fall back to status
        let status = resp.status();
        if let Ok(body) = resp.into_string() {
            if let Some(code) = xml_text(&body, "Code") {
                return S3Error { code };
            }
        }
        S3Error { code: status.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl S3Client for HttpS3Client {
    fn head_object(&self, bucket: &str, key: &str) -> Result<Option<String>, S3Error> {
        debug_assert_eq!(bucket, self.bucket, "one client per bucket");
        let req = self.sign("HEAD", &uri_encode_path(key), &[], None, &[], b"");
        let resp = self.send(&req, None)?;
        match resp.status() {
            200 => Ok(resp.header("ETag").map(str::to_string)),
            404 | 410 => Ok(None),
            _ => Err(Self::error_code(resp)),
        }
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Option<S3Object>, S3Error> {
        debug_assert_eq!(bucket, self.bucket);
        let req = self.sign("GET", &uri_encode_path(key), &[], None, &[], b"");
        let resp = self.send(&req, None)?;
        match resp.status() {
            200 => {
                let etag = resp.header("ETag").unwrap_or_default().to_string();
                let mut metadata = Vec::new();
                for name in resp.headers_names() {
                    if let Some(meta) = name.strip_prefix("x-amz-meta-") {
                        if let Some(v) = resp.header(&name) {
                            metadata.push((meta.to_string(), v.to_string()));
                        }
                    }
                }
                // bodies are raw RFC 822 bytes: read binary, never as text
                use std::io::Read;
                let mut body = Vec::new();
                resp.into_reader().read_to_end(&mut body).map_err(|e| S3Error { code: format!("NetworkError: {e}") })?;
                Ok(Some(S3Object { body, etag, metadata }))
            }
            404 | 410 => {
                // moto gives a proper XML code; plain 404 also maps to missing
                Err(S3Error { code: "NoSuchKey".to_string() })
            }
            _ => Err(Self::error_code(resp)),
        }
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: &[u8],
        content_type: &str,
        metadata: &[(String, String)],
        condition: &PutCondition,
    ) -> Result<String, S3Error> {
        debug_assert_eq!(bucket, self.bucket);
        let mut extra_amz: Vec<(String, String)> = metadata
            .iter()
            .map(|(k, v)| (format!("x-amz-meta-{}", k.to_lowercase()), v.clone()))
            .collect();
        match condition {
            PutCondition::None => {}
            PutCondition::IfMatch(etag) => extra_amz.push(("if-match".to_string(), etag.clone())),
            PutCondition::IfNoneMatchAny => extra_amz.push(("if-none-match".to_string(), "*".to_string())),
        }
        let req = self.sign("PUT", &uri_encode_path(key), &[], Some(content_type), &extra_amz, body);
        let resp = self.send(&req, Some(body))?;
        match resp.status() {
            200 => Ok(resp.header("ETag").unwrap_or_default().to_string()),
            412 => Err(S3Error { code: "PreconditionFailed".to_string() }),
            _ => Err(Self::error_code(resp)),
        }
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, S3Error> {
        debug_assert_eq!(bucket, self.bucket);
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.to_string()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".to_string(), t.clone()));
            }
            let req = self.sign("GET", "", &query, None, &[], b"");
            let resp = self.send(&req, None)?;
            if resp.status() != 200 {
                return Err(Self::error_code(resp));
            }
            let body = resp
                .into_string()
                .map_err(|e| S3Error { code: format!("NetworkError: {e}") })?;
            for m in KEY_RE.captures_iter(&body) {
                keys.push(html_escape::decode_html_entities(&m[1]).into_owned());
            }
            let truncated = xml_text(&body, "IsTruncated").is_some_and(|v| v == "true");
            if !truncated {
                return Ok(keys);
            }
            token = xml_text(&body, "NextContinuationToken");
            if token.is_none() {
                return Ok(keys);
            }
        }
    }

    fn head_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        debug_assert_eq!(bucket, self.bucket);
        let req = self.sign("HEAD", "", &[], None, &[], b"");
        let resp = self.send(&req, None)?;
        match resp.status() {
            200 => Ok(()),
            404 => Err(S3Error { code: "404".to_string() }),
            _ => Err(Self::error_code(resp)),
        }
    }
}

// ---- SigV4 primitives (public for the golden tests) -------------------------

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// AWS URI encoding: RFC 3986 unreserved characters stay literal, everything
/// else becomes %XX uppercase. Slash is *not* special here (query values).
pub fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Same rules but slash and percent are preserved: S3 object keys written by
/// `storage::seg` already contain single-encoded `%xx` sequences, and per the
/// SigV4 S3 rules they pass through verbatim (S3 does not double-encode).
/// Literal spaces/brackets (keys as returned by listings) still get encoded.
pub fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'/' | b'%' => out.push(b as char),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- helpers ----------------------------------------------------------------

static KEY_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?s)<Key>(.*?)</Key>").unwrap());

fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r"(?s)<{}>(.*?)</{}>", regex::escape(tag), regex::escape(tag))).ok()?;
    let caps = re.captures(xml)?;
    Some(html_escape::decode_html_entities(&caps[1]).into_owned())
}

fn host_of(url: &str) -> String {
    let rest = &url[url.find("://").map_or(0, |i| i + 3)..];
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Split an object key into URL path segments, each AWS-encoded once.
/// Keys built by `storage::seg` already contain literal `%` characters, which
/// must themselves be encoded (`%25`) on the wire.
pub fn encode_key(key: &str) -> String {
    uri_encode_path(key)
}
