//! Port of `tests/test_auth.py`: the Firebase ID token verifier. Tokens are
//! minted with the `rsa` crate against a hand-built X.509 certificate (the
//! minimal DER the verifier parses), and Google's certificate endpoint is a
//! fake fetcher that counts its calls — the `FakeCerts` role.

mod support;

use aimap::auth::{authorize, AuthError, CachingRequest, FirebaseVerifier, Forbidden, User, VerifyError, CERTS_URL};
use rsa::pkcs1v15::SigningKey;
use rsa::rand_core::OsRng;
use rsa::sha2::Sha256;
use rsa::signature::SignatureEncoding;
use rsa::signature::Signer;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const PROJECT: &str = "aimap-test";

fn allowed() -> BTreeSet<String> {
    BTreeSet::from(["me@example.com".to_string()])
}

// --------------------------------------------------------------------------------------
// RSA keys, a minimal X.509 certificate, and RS256 tokens
// --------------------------------------------------------------------------------------

fn keypair() -> (RsaPrivateKey, String) {
    let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA key");
    let cert = build_cert_pem(&key);
    (key, cert)
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64url(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

const OID_RSA_ENCRYPTION: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_SHA256_RSA: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];

/// DER encoding: tag, length (short or long form), content.
fn der(tag: u8, content: impl Into<Vec<u8>>) -> Vec<u8> {
    let content = content.into();
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
    out.extend_from_slice(&content);
    out
}

/// A positive INTEGER with a leading zero when the high bit would make it
/// look negative (DER INTEGERs are signed).
fn integer(bytes: &[u8]) -> Vec<u8> {
    if !bytes.is_empty() && bytes[0] & 0x80 != 0 {
        der(0x02, [[0u8].as_slice(), bytes].concat())
    } else {
        der(0x02, bytes.to_vec())
    }
}

/// A self-signed certificate for `key` with the shape the verifier's DER walk
/// expects: SEQUENCE { SEQUENCE { [0] version, serial, alg, issuer, validity,
/// subject, SPKI }, alg, signature }. Signed with SHA-256/RSA, like the
/// `cryptography` certificates the Python test mints.
fn build_cert_pem(key: &RsaPrivateKey) -> String {
    let public = RsaPublicKey::from(key);
    let n_int = integer(&public.n().to_bytes_be());
    let e_int = integer(&public.e().to_bytes_be());
    let rsa_key = der(0x30, [n_int, e_int].concat());
    let spki = der(
        0x30,
        [
            der(0x30, der(0x06, OID_RSA_ENCRYPTION.to_vec())),          // algorithm
            der(0x03, [[0u8].as_slice(), rsa_key.as_slice()].concat()), // BIT STRING
        ]
        .concat(),
    );

    // Name ::= SEQUENCE OF RelativeDistinguishedName; one CN=test RDN (a SET),
    // like the Python test's single-attribute name.
    let cn = der(
        0x30,
        [der(
            0x31,
            der(
                0x30,
                [der(0x06, [0x55u8, 0x04, 0x03]), der(0x0c, b"test".to_vec())].concat(),
            ),
        )]
        .concat(),
    );
    let validity = der(
        0x30,
        [der(0x17, b"250101000000Z".to_vec()), der(0x17, b"350101000000Z".to_vec())].concat(),
    );
    let alg = der(0x30, der(0x06, OID_SHA256_RSA.to_vec())); // sha256WithRSAEncryption
    let tbs = der(
        0x30,
        [
            der(0xA0, der(0x02, vec![2u8])), // [0] EXPLICIT version v3
            der(0x02, vec![1u8]),            // serial 1
            alg.clone(),
            cn.clone(), // issuer
            validity,
            cn, // subject
            spki,
        ]
        .concat(),
    );
    let signer = SigningKey::<Sha256>::new(key.clone());
    let signature = signer.sign(&tbs);
    let sig_bits = der(0x03, [[0u8].as_slice(), signature.to_bytes().as_ref()].concat());
    let cert = der(0x30, [tbs, alg, sig_bits].concat());

    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    let b64body = b64(&cert);
    for chunk in b64body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

fn token(key: &RsaPrivateKey, kid: &str, claims: &Value) -> String {
    let header = json!({"alg": "RS256", "kid": kid});
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let signer = SigningKey::<Sha256>::new(key.clone());
    let signature = signer.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", b64url(signature.to_bytes().as_ref()))
}

fn default_claims(now: i64) -> Value {
    json!({
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "aud": PROJECT,
        "sub": "uid-1",
        "email": "Me@Example.com",
        "email_verified": true,
        "iat": now,
        "exp": now + 3600,
        "firebase": {"sign_in_provider": "google.com"},
    })
}

// --------------------------------------------------------------------------------------
// the fake certificate endpoint
// --------------------------------------------------------------------------------------

/// google-auth transport serving one certificate, counting fetches.
struct FakeCerts {
    certs: HashMap<String, String>,
    max_age: i64,
    calls: AtomicU64,
}

impl FakeCerts {
    #[allow(dead_code)] // the empty-map case is the "no certs published" fake
    fn new() -> Arc<FakeCerts> {
        Arc::new(FakeCerts { max_age: 3600, calls: AtomicU64::new(0), certs: HashMap::new() })
    }
    fn with_cert(certs: HashMap<String, String>, max_age: i64) -> Arc<FakeCerts> {
        Arc::new(FakeCerts { certs, max_age, calls: AtomicU64::new(0) })
    }
}

impl aimap::auth::Fetcher for FakeCerts {
    fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        assert_eq!(url, CERTS_URL);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::to_vec(&self.certs).unwrap();
        Ok((body, format!("public, max-age={}", self.max_age)))
    }
}

/// The verifier under test plus the key/certs it was minted against.
struct Fixture {
    key: RsaPrivateKey,
    verifier: FirebaseVerifier<Arc<CachingRequest<Arc<FakeCerts>>>>,
    certs: Arc<FakeCerts>,
    clock: FakeMono,
    now: i64,
}

impl Fixture {
    fn new() -> Fixture {
        let (key, cert) = keypair();
        let mut certs = HashMap::new();
        certs.insert("k1".to_string(), cert);
        let certs = FakeCerts::with_cert(certs, 3600);
        let clock = FakeMono::new();
        let inner = CachingRequest::new(certs.clone(), clock.mono(), 300.0);
        let verifier = FirebaseVerifier::with_transport(PROJECT, allowed(), Arc::new(inner), 10);
        Fixture { key, verifier, certs, clock, now: now_unix() }
    }

    fn token(&self) -> String {
        self.token_with(&json!({}))
    }
    fn token_with(&self, overrides: &Value) -> String {
        let mut claims = default_claims(self.now);
        if let (Some(obj), Some(over)) = (claims.as_object_mut(), overrides.as_object()) {
            for (k, v) in over {
                if v.is_null() {
                    obj.remove(k);
                } else {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        token(&self.key, "k1", &claims)
    }
    fn verify(&self, token: &str) -> Result<User, VerifyError> {
        self.verifier.verify(token)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The `clock=lambda: clock[0]` fake from the Python test.
struct FakeMono(Arc<std::sync::Mutex<f64>>);

impl FakeMono {
    fn new() -> FakeMono {
        FakeMono(Arc::new(std::sync::Mutex::new(0.0)))
    }
    fn advance_mono(&self, seconds: f64) {
        *self.0.lock().unwrap() += seconds;
    }
    fn mono(&self) -> Arc<dyn Fn() -> f64 + Send + Sync> {
        let cell = Arc::clone(&self.0);
        Arc::new(move || *cell.lock().unwrap())
    }
}

// --------------------------------------------------------------------------------------
// the Python suite
// --------------------------------------------------------------------------------------

#[test]
fn valid_token_returns_user_with_lowercased_email() {
    let f = Fixture::new();
    let user = f.verify(&f.token()).expect("valid token verifies");
    assert_eq!((user.uid.as_str(), user.email.as_str()), ("uid-1", "me@example.com"));
}

#[test]
fn rejects_token_for_another_project() {
    let f = Fixture::new();
    let bad = f.token_with(&json!({"aud": "other-project"}));
    let err = f.verify(&bad).unwrap_err();
    assert!(matches!(err, VerifyError::Auth(AuthError(_))), "{err:?}");
}

#[test]
fn rejects_token_for_another_issuer() {
    let f = Fixture::new();
    let bad = f.token_with(&json!({"iss": "https://securetoken.google.com/other-project"}));
    let err = f.verify(&bad).unwrap_err();
    assert!(matches!(err, VerifyError::Auth(AuthError(_))), "{err:?}");
}

#[test]
fn rejects_expired_token() {
    let f = Fixture::new();
    let bad = f.token_with(&json!({"exp": f.now - 3600, "iat": f.now - 7200}));
    let err = f.verify(&bad).unwrap_err();
    assert!(matches!(err, VerifyError::Auth(AuthError(_))), "{err:?}");
}

#[test]
fn rejects_token_without_subject() {
    let f = Fixture::new();
    let bad = f.token_with(&json!({"sub": ""}));
    let err = f.verify(&bad).unwrap_err();
    assert!(matches!(err, VerifyError::Auth(AuthError(_))), "{err:?}");
}

#[test]
fn rejects_token_signed_with_another_key() {
    let f = Fixture::new();
    let (other, _) = keypair();
    let mut claims = default_claims(f.now);
    claims["email"] = json!("me@example.com");
    let bad = token(&other, "k1", &claims);
    assert!(f.verify(&bad).is_err());
}

#[test]
fn rejects_garbage() {
    let f = Fixture::new();
    assert!(f.verify("not-a-jwt").is_err());
}

#[test]
fn rejects_token_signed_for_a_missing_key_id() {
    let f = Fixture::new();
    let (other, _) = keypair();
    let bad = token(&other, "k9", &default_claims(f.now));
    assert!(f.verify(&bad).is_err());
}

#[test]
fn rejects_non_rs256_algorithms() {
    let f = Fixture::new();
    let header = json!({"alg": "HS256", "kid": "k1"});
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(default_claims(f.now).to_string().as_bytes())
    );
    let bad = format!("{signing_input}.{}", b64url(b"not-a-signature"));
    assert!(f.verify(&bad).is_err());
}

#[test]
fn valid_token_but_not_allowed_is_forbidden() {
    for overrides in [json!({"email": "someone@example.com"}), json!({"email_verified": false}), json!({"email": null})] {
        let f = Fixture::new();
        let err = f.verify(&f.token_with(&overrides)).unwrap_err();
        assert!(matches!(err, VerifyError::Forbidden(Forbidden(_))), "{overrides}: {err:?}");
    }
}

#[test]
fn authorize_is_fail_closed_with_empty_allowlist() {
    let claims = json!({
        "iss": format!("https://securetoken.google.com/{PROJECT}"),
        "sub": "u",
        "email": "me@example.com",
        "email_verified": true,
    })
    .as_object()
    .unwrap()
    .clone();
    let err = authorize(&claims, PROJECT, &BTreeSet::new()).unwrap_err();
    assert!(matches!(err, VerifyError::Forbidden(Forbidden(_))), "{err:?}");
}

// --------------------------------------------------------------------------------------
// the certificate cache
// --------------------------------------------------------------------------------------

#[test]
fn certificates_are_cached_for_their_max_age() {
    let (key, cert) = keypair();
    let mut map = HashMap::new();
    map.insert("k1".to_string(), cert);
    let certs = FakeCerts::with_cert(map, 100);
    let clock = FakeMono::new();
    let inner = CachingRequest::new(certs.clone(), clock.mono(), 300.0);
    let verifier = FirebaseVerifier::with_transport(PROJECT, allowed(), Arc::new(inner), 10);

    let f = Fixture { key, verifier, certs, clock, now: now_unix() };
    let tok = f.token();
    f.verify(&tok).expect("first verify");
    f.verify(&tok).expect("second verify (cached)");
    assert_eq!(f.certs.calls.load(Ordering::SeqCst), 1);
    f.clock.advance_mono(101.0);
    f.verify(&tok).expect("third verify (cache expired)");
    assert_eq!(f.certs.calls.load(Ordering::SeqCst), 2);
}
