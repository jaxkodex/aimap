//! Port of `src/aimap/crypto.py`: encryption for stored credentials.
//!
//! Uses Fernet (AES-128-CBC + HMAC-SHA256) via the pure-Rust `fernet` crate,
//! which is wire-compatible with Python's `cryptography` — tokens produced by
//! either implementation decrypt with the other (verified by golden vectors
//! generated from the real `cryptography` library in `tests/fixtures`).
//!
//! AIMAP_SECRET_KEY holds one or more comma-separated keys. The first encrypts;
//! all of them can decrypt, so a key can be rotated without downtime.

use fernet::{DecryptionError, Fernet, MultiFernet};

/// `CryptoError` in crypto.py.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoError(pub String);

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CryptoError {}

/// `Fernet.generate_key()`: 32 random bytes, url-safe base64.
pub fn generate_key() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("system entropy unavailable");
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// `Cipher` in crypto.py. `Debug`/`Display` never reveal the keys.
#[derive(Clone)]
pub struct Cipher {
    f: MultiFernet,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(<redacted>)")
    }
}

impl Cipher {
    /// `Cipher(keys)`: one comma-separated string of Fernet keys.
    pub fn new(keys: &str) -> Result<Cipher, CryptoError> {
        Self::from_key_list(keys.split(','))
    }

    pub fn from_key_list<'a, I: IntoIterator<Item = &'a str>>(keys: I) -> Result<Cipher, CryptoError> {
        let mut fernets = Vec::new();
        for k in keys {
            let k = k.trim();
            if k.is_empty() {
                continue;
            }
            let f = Fernet::new(k)
                .ok_or_else(|| CryptoError("AIMAP_SECRET_KEY must be Fernet keys (run `aimap keygen`)".to_string()))?;
            fernets.push(f);
        }
        if fernets.is_empty() {
            return Err(CryptoError("AIMAP_SECRET_KEY is empty".to_string()));
        }
        Ok(Cipher { f: MultiFernet::new(fernets) })
    }

    /// Encrypts `{"user", "password"}` so a token can be checked against the account.
    pub fn encrypt(&self, user: &str, password: &str) -> String {
        let blob = serde_json::json!({"user": user, "password": password}).to_string();
        self.f.encrypt(blob.as_bytes())
    }

    /// Decrypts and checks the embedded user, so a ciphertext copied onto
    /// another account's record is rejected.
    pub fn decrypt(&self, user: &str, token: &str) -> Result<String, CryptoError> {
        let data = self
            .f
            .decrypt(token)
            .map_err(|_: DecryptionError| {
                CryptoError(format!(
                    "cannot decrypt credentials for {user}: wrong key or corrupted token"
                ))
            })
            .and_then(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map_err(|_| {
                        CryptoError(format!(
                            "cannot decrypt credentials for {user}: wrong key or corrupted token"
                        ))
                    })
            })?;
        match data.get("user").and_then(|u| u.as_str()) {
            Some(u) if u == user => Ok(data
                .get("password")
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string()),
            _ => Err(CryptoError(format!(
                "credentials for {user} were encrypted for a different account"
            ))),
        }
    }

    /// Re-encrypts with the primary key (MultiFernet.rotate semantics).
    pub fn rotate(&self, token: &str) -> Result<String, CryptoError> {
        let data = self.f.decrypt(token).map_err(|_: DecryptionError| {
            CryptoError("cannot rotate token: no configured key decrypts it".to_string())
        })?;
        Ok(self.f.encrypt(&data))
    }
}
