//! Who may call the API: a Firebase Authentication ID token for an allowed
//! email. Port of `src/aimap/auth.py`.
//!
//! The app signs in with the Firebase SDK (Google, Apple, ...) and sends the
//! ID token as `Authorization: Bearer <token>`. Tokens last an hour and the
//! SDK renews them, so the API keeps no sessions. It checks the signature
//! against Google's published certificates, the audience and issuer for
//! FIREBASE_PROJECT_ID, and then that the email is verified and listed in
//! AIMAP_ALLOWED_EMAILS. Any Google account can sign in to a Firebase project,
//! so the allowlist is what keeps strangers out.
//!
//! The wire part verifies RS256 against the certificate map Google publishes
//! for `securetoken@system.gserviceaccount.com` (the endpoint Firebase's own
//! "verify with a third-party library" docs point at); certificates are
//! unwrapped with a hand-rolled minimal DER walk, keeping the tree free of
//! C-backed X.509 crates. The claim checks live in [`authorize`], which is
//! pure and tested without tokens, like the Python code.

use rsa::pkcs1v15::VerifyingKey;
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Google's certificate map for the Firebase token signer: a JSON object of
/// key id → PEM certificate.
pub const CERTS_URL: &str =
    "https://www.googleapis.com/robot/v1/metadata/x509/securetoken@system.gserviceaccount.com";

/// The token is missing, invalid or expired: 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError(pub String);

/// The token is valid but its email is not allowed: 403.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forbidden(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    Auth(AuthError),
    Forbidden(Forbidden),
}

impl VerifyError {
    /// The message the API maps to 401 vs 403.
    pub fn message(&self) -> &str {
        match self {
            VerifyError::Auth(e) => &e.0,
            VerifyError::Forbidden(e) => &e.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub uid: String,
    pub email: String,
}

/// Checks google-auth leaves to the caller. Pure, so it is tested without
/// tokens.
pub fn authorize(
    claims: &Map<String, Value>,
    project_id: &str,
    allowed_emails: &BTreeSet<String>,
) -> Result<User, VerifyError> {
    if claims.get("iss").and_then(Value::as_str) != Some(&format!("https://securetoken.google.com/{project_id}")) {
        return Err(VerifyError::Auth(AuthError("token issued for another project".into())));
    }
    let uid = match claims.get("sub").and_then(Value::as_str) {
        Some(sub) if !sub.is_empty() => sub.to_string(),
        _ => return Err(VerifyError::Auth(AuthError("token has no subject".into()))),
    };
    let email = claims.get("email").and_then(Value::as_str).unwrap_or("").to_lowercase();
    if email.is_empty() || claims.get("email_verified") != Some(&Value::Bool(true)) {
        return Err(VerifyError::Forbidden(Forbidden("email not verified".into())));
    }
    if !allowed_emails.contains(&email) {
        return Err(VerifyError::Forbidden(Forbidden("email not allowed".into())));
    }
    Ok(User { uid, email })
}

/// The transport Google certificates come through: a GET that returns the
/// response body plus its `Cache-Control` header. `CachingRequest` wraps one
/// of these and stands in for google-auth's request wrapper.
pub trait Fetcher: Send + Sync {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String>;
}

/// A GET through [`crate::httpc`]: the transport `FirebaseVerifier` uses in
/// production.
#[derive(Clone, Copy, Default)]
pub struct HttpFetch;

impl Fetcher for HttpFetch {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        let req = crate::httpc::Request {
            method: "GET",
            url,
            headers: &[],
            body: None,
            timeout: Duration::from_secs(30),
        };
        let resp = crate::httpc::request(&req).map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(format!("GET {url} returned {}", resp.status));
        }
        let cache_control = resp.header("cache-control").unwrap_or("").to_string();
        Ok((resp.body, cache_control))
    }
}

impl<F: Fetcher + ?Sized> Fetcher for Arc<F> {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        (**self).get(url)
    }
}

fn max_age(cache_control: &str) -> Option<f64> {
    // r"max-age=(\d+)", like the Python module
    let lower = cache_control.to_ascii_lowercase();
    let start = lower.find("max-age=")? + "max-age=".len();
    let digits: String = lower[start..].chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// One cached GET: response bytes and its Cache-Control header.
type CachedResponse = (Vec<u8>, String);

/// Keeps GET responses for their Cache-Control max-age. Firebase's signing
/// certificates change every few hours and say so in their headers; without
/// this every API call would fetch them again.
pub struct CachingRequest<F: Fetcher> {
    inner: F,
    clock: ClockFn,
    default_ttl: f64,
    cache: Mutex<HashMap<String, (f64, CachedResponse)>>,
}

impl<F: Fetcher> CachingRequest<F> {
    /// `clock` is time.monotonic (seconds); tests inject a fake.
    pub fn new(inner: F, clock: ClockFn, default_ttl: f64) -> CachingRequest<F> {
        CachingRequest { inner, clock, default_ttl, cache: Mutex::new(HashMap::new()) }
    }
}

impl<F: Fetcher> Fetcher for CachingRequest<F> {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        let now = (self.clock)();
        if let Some((until, hit)) = self.cache.lock().unwrap().get(url) {
            if *until > now {
                return Ok(hit.clone());
            }
        }
        let response = self.inner.get(url)?;
        let ttl = max_age(&response.1).unwrap_or(self.default_ttl);
        self.cache.lock().unwrap().insert(url.to_string(), (now + ttl, response.clone()));
        Ok(response)
    }
}

/// Verifies Firebase ID tokens for one project and allowlist.
pub struct FirebaseVerifier<F: Fetcher> {
    pub project_id: String,
    pub allowed_emails: BTreeSet<String>,
    /// Clock skew accepted around exp/iat, like google-auth's 10 seconds.
    pub clock_skew: i64,
    transport: F,
}

impl FirebaseVerifier<CacheWrap> {
    pub fn new(project_id: &str, allowed_emails: BTreeSet<String>) -> FirebaseVerifier<CacheWrap> {
        let clock: ClockFn = Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
        let transport = CacheWrap(Arc::new(CachingRequest::new(HttpFetch, clock, 300.0)));
        FirebaseVerifier { project_id: project_id.into(), allowed_emails, clock_skew: 10, transport }
    }
}

/// What `FirebaseVerifier::new` builds: a shared, caching transport (the
/// Python constructor wraps `Request()` in `CachingRequest`).
pub struct CacheWrap(Arc<dyn Fetcher>);

impl Fetcher for CacheWrap {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        self.0.get(url)
    }
}

impl<F: Fetcher> FirebaseVerifier<F> {
    pub fn with_transport(
        project_id: &str,
        allowed_emails: BTreeSet<String>,
        transport: F,
        clock_skew: i64,
    ) -> FirebaseVerifier<F> {
        FirebaseVerifier { project_id: project_id.into(), allowed_emails, clock_skew, transport }
    }

    pub fn verify(&self, token: &str) -> Result<User, VerifyError> {
        let claims = self.decode_and_verify(token).map_err(|message| {
            VerifyError::Auth(AuthError(format!("invalid token: {message}")))
        })?;
        authorize(&claims, &self.project_id, &self.allowed_emails)
    }

    /// Signature, timestamps and audience: everything google-auth checks
    /// before handing the claims to `authorize`.
    fn decode_and_verify(&self, token: &str) -> Result<Map<String, Value>, String> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err("not a JWT".into());
        }
        let header = decode_segment(parts[0])?;
        let header = header
            .as_object()
            .ok_or_else(|| "JWT header is not an object".to_string())?;
        if header.get("alg").and_then(Value::as_str) != Some("RS256") {
            return Err("unsupported algorithm".into());
        }
        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .ok_or_else(|| "JWT header has no key id".to_string())?;

        let (certs_body, _) = self.transport.get(CERTS_URL)?;
        let certs: Value = serde_json::from_slice(&certs_body).map_err(|e| e.to_string())?;
        let pem = certs
            .get(kid)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no certificate for key id {kid:?}"))?;
        let public = rsa_public_key_from_cert_pem(pem)?;

        let signature: Vec<u8> = b64url_decode(parts[2])?;
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let key = VerifyingKey::<Sha256>::new(public);
        let signature = rsa::pkcs1v15::Signature::try_from(signature.as_slice())
            .map_err(|_| "malformed signature".to_string())?;
        key.verify(signing_input.as_bytes(), &signature)
            .map_err(|_| "signature mismatch".to_string())?;

        let claims = decode_segment(parts[1])?;
        let claims = claims
            .as_object()
            .ok_or_else(|| "JWT payload is not an object".to_string())?
            .clone();

        let now = unix_now();
        let skew = self.clock_skew;
        let expired = |field: &str| -> Result<i64, String> {
            claims
                .get(field)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("JWT has no {field}"))
        };
        let exp = expired("exp")?;
        if now > exp + skew {
            return Err("token expired".into());
        }
        if let Some(iat) = claims.get("iat").and_then(Value::as_i64) {
            if now < iat - skew {
                return Err("token used too early".into());
            }
        }
        if claims.get("aud").and_then(Value::as_str) != Some(self.project_id.as_str()) {
            return Err("token issued for another project".into());
        }
        Ok(claims)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn decode_segment(part: &str) -> Result<Value, String> {
    let bytes = b64url_decode(part)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JWT JSON: {e}"))
}

fn b64url_decode(part: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|e| format!("invalid base64url: {e}"))
}

// --------------------------------------------------------------------------------------
// minimal DER: pull the RSA public key out of an X.509 certificate
// --------------------------------------------------------------------------------------

/// One DER TLV: (tag, content bytes, rest of the buffer).
fn der_tlv(buf: &[u8]) -> Result<(u8, &[u8], &[u8]), String> {
    if buf.len() < 2 {
        return Err("DER: truncated".into());
    }
    let tag = buf[0];
    let first = buf[1];
    let (len, rest_off) = match first {
        n if n < 0x80 => (n as usize, 2usize),
        0x81 => {
            if buf.len() < 3 {
                return Err("DER: truncated length".into());
            }
            (buf[2] as usize, 3usize)
        }
        0x82 => {
            if buf.len() < 4 {
                return Err("DER: truncated length".into());
            }
            (((buf[2] as usize) << 8) | buf[3] as usize, 4usize)
        }
        _ => return Err("DER: length too large".into()),
    };
    let content_end = rest_off.checked_add(len).ok_or("DER: length overflows")?;
    if buf.len() < content_end {
        return Err("DER: value truncated".into());
    }
    Ok((tag, &buf[rest_off..content_end], &buf[content_end..]))
}

fn expect_tlv<'a>(buf: &'a [u8], tag: u8, what: &str) -> Result<(&'a [u8], &'a [u8]), String> {
    let (t, content, rest) = der_tlv(buf)?;
    if t != tag {
        return Err(format!("DER: expected tag {tag:#x} for {what}, got {t:#x}"));
    }
    Ok((content, rest))
}

/// Certificate → SubjectPublicKeyInfo → RSAPublicKey (modulus, exponent).
///
/// Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
/// tbsCertificate ::= SEQUENCE { [0] version?, serial, algId, issuer, validity,
///                               subject, subjectPublicKeyInfo, ... }
pub fn rsa_public_key_from_cert_der(der: &[u8]) -> Result<RsaPublicKey, String> {
    let (cert, _) = expect_tlv(der, 0x30, "certificate")?;
    let (tbs, _) = expect_tlv(cert, 0x30, "tbsCertificate")?;
    // optional [0] EXPLICIT version
    let mut rest = tbs;
    if rest.first() == Some(&0xA0) {
        let (_, r) = expect_tlv(rest, 0xA0, "version")?;
        rest = r;
    }
    let (_, rest) = expect_tlv(rest, 0x02, "serialNumber")?; // INTEGER serial
    let (_, rest) = expect_tlv(rest, 0x30, "signature algorithm")?;
    let (_, rest) = expect_tlv(rest, 0x30, "issuer")?;
    let (_, rest) = expect_tlv(rest, 0x30, "validity")?;
    let (_, rest) = expect_tlv(rest, 0x30, "subject")?;
    let (spki, _) = expect_tlv(rest, 0x30, "subjectPublicKeyInfo")?;
    rsa_public_key_from_spki(spki)
}

/// SubjectPublicKeyInfo ::= SEQUENCE { algorithm, subjectPublicKey BIT STRING }
fn rsa_public_key_from_spki(spki: &[u8]) -> Result<RsaPublicKey, String> {
    let (alg, rest) = expect_tlv(spki, 0x30, "algorithm")?;
    // rsaEncryption is 1.2.840.113549.1.1.1; content starts with the OID TLV.
    let (oid, _) = expect_tlv(alg, 0x06, "algorithm OID")?;
    if oid != [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01] {
        return Err("certificate key is not RSA".into());
    }
    let (bits, _) = expect_tlv(rest, 0x03, "subjectPublicKey")?;
    if bits.is_empty() {
        return Err("DER: empty bit string".into());
    }
    let (unused, key_der) = (bits[0], &bits[1..]);
    if unused != 0 {
        return Err("DER: unexpected unused bits in subjectPublicKey".into());
    }
    let (key, _) = expect_tlv(key_der, 0x30, "RSAPublicKey")?;
    let (n_bytes, rest) = expect_tlv(key, 0x02, "modulus")?;
    let (e_bytes, _) = expect_tlv(rest, 0x02, "exponent")?;
    // INTEGERs are signed; keys are positive, so a leading zero is padding.
    let n = BigUint::from_bytes_be(n_bytes.strip_prefix(&[0]).unwrap_or(n_bytes));
    let e = BigUint::from_bytes_be(e_bytes.strip_prefix(&[0]).unwrap_or(e_bytes));
    RsaPublicKey::new(n, e).map_err(|e| format!("invalid RSA public key: {e}"))
}

pub fn rsa_public_key_from_cert_pem(pem: &str) -> Result<RsaPublicKey, String> {
    let der = pem_to_der(pem)?;
    rsa_public_key_from_cert_der(&der)
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("invalid PEM: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_age_parsing() {
        assert_eq!(max_age("public, max-age=3600"), Some(3600.0));
        assert_eq!(max_age("max-age=100"), Some(100.0));
        assert_eq!(max_age("no-cache"), None);
        assert_eq!(max_age("max-age=abc"), None);
    }
}
