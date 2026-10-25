//! Port of `src/aimap/accounts.py`: where the worker gets its list of IMAP
//! accounts.
//!
//! Two sources, chosen with ACCOUNTS_SOURCE:
//!
//! bucket (default when IMAP_USER is not set)
//!     A JSON file in the bucket, `<S3_PREFIX>config/accounts.json` by
//!     default. Passwords are encrypted with AIMAP_SECRET_KEY; everything
//!     else is plain so the file can be inspected. The worker re-checks the
//!     file's ETag every poll, so account changes take effect without a
//!     restart.
//!
//! env (default when IMAP_USER is set)
//!     IMAP_USER / IMAP_PASSWORD, plus IMAP_USER2 / IMAP_PASSWORD2 and so on.
//!     Fixed at startup. Meant for a quick single-mailbox setup.
//!
//! File format:
//!
//! ```json
//! {"version": 1, "accounts": [{
//!     "user": "me@example.com", "host": "imap.gmail.com", "port": 993,
//!     "mailboxes": ["INBOX"], "enabled": true,
//!     "credentials": "<Fernet token>",
//!     "created_at": "...", "updated_at": "..."}]}
//! ```

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::crypto::{Cipher, CryptoError};
use crate::logs::{LogValue, Logger};
use crate::storage::{S3Client, S3Error, Store, StoreError};

pub const FILE_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsError(pub String);

impl std::fmt::Display for AccountsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for AccountsError {}

/// A mailbox the worker can log in to. The password is decrypted and kept in
/// memory only (kept out of the Debug output, like `field(repr=False)`).
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: i32,
    pub mailboxes: Vec<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("user", &self.user)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("mailboxes", &self.mailboxes)
            .finish()
    }
}

/// What the ingester needs from an account list: the current enabled accounts.
pub trait AccountSource {
    fn load(&mut self) -> Vec<Account>;
}

// --------------------------------------------------------------------------------------
// env source
// --------------------------------------------------------------------------------------

static USER_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^IMAP_USER(\d*)$").unwrap());

pub fn parse_env_accounts(env: &std::collections::HashMap<String, String>) -> Result<Vec<Account>, AccountsError> {
    let mut suffixes: Vec<&str> = env
        .iter()
        .filter_map(|(k, v)| {
            USER_KEY.captures(k).and_then(|c| c.get(1)).map(|m| m.as_str()).filter(|_| !v.trim().is_empty())
        })
        .collect();
    // sorted(key=lambda s: int(s or 1)): "" sorts as 1
    suffixes.sort_by_key(|s| if s.is_empty() { 1 } else { s.parse::<i64>().unwrap_or(i64::MAX) });

    let mut accounts = Vec::new();
    for sfx in &suffixes {
        let get = |name: &str, default: Option<&str>, fallback: bool| -> Option<String> {
            let mut value = env.get(&format!("{name}{sfx}")).map(|s| s.trim()).unwrap_or("");
            if value.is_empty() && fallback {
                value = env.get(name).map(|s| s.trim()).unwrap_or("");
            }
            if value.is_empty() {
                default.map(|d| d.to_string())
            } else {
                Some(value.to_string())
            }
        };

        let Some(password) = get("IMAP_PASSWORD", None, false) else {
            return Err(AccountsError(format!(
                "IMAP_PASSWORD{sfx} is required when IMAP_USER{sfx} is set"
            )));
        };
        let Some(host) = get("IMAP_HOST", None, true) else {
            return Err(AccountsError(format!("IMAP_HOST{sfx} (or IMAP_HOST) is required")));
        };
        let Some(port) = get("IMAP_PORT", Some("993"), true) else {
            unreachable!("default is never None");
        };
        if !port.chars().all(|c| c.is_ascii_digit()) {
            return Err(AccountsError(format!("IMAP_PORT{sfx} must be an integer, got {port:?}")));
        }
        let mailboxes = parse_mailboxes_str(&get("IMAP_MAILBOX", Some("INBOX"), true).unwrap());
        accounts.push(Account {
            user: get("IMAP_USER", None, false).unwrap_or_default(),
            password: clean_password(&password),
            host,
            port: port.parse().unwrap(),
            mailboxes,
        });
    }
    Ok(accounts)
}

/// ACCOUNTS_SOURCE=env, fixed at startup.
#[derive(Debug, Clone)]
pub struct EnvAccounts {
    accounts: Vec<Account>,
}

impl EnvAccounts {
    pub fn new(env: &std::collections::HashMap<String, String>) -> Result<EnvAccounts, AccountsError> {
        let accounts = parse_env_accounts(env)?;
        if accounts.is_empty() {
            return Err(AccountsError(
                "ACCOUNTS_SOURCE=env but IMAP_USER / IMAP_PASSWORD are not set".to_string(),
            ));
        }
        Ok(EnvAccounts { accounts })
    }

    pub fn load(&mut self) -> Vec<Account> {
        self.accounts.clone()
    }
}

impl AccountSource for EnvAccounts {
    fn load(&mut self) -> Vec<Account> {
        self.accounts.clone()
    }
}

/// App passwords are often pasted with spaces.
pub fn clean_password(password: &str) -> String {
    password.replace(' ', "")
}

/// `parse_mailboxes` for the string form: comma-separated, trimmed, empties
/// dropped; falls back to INBOX when nothing is left.
pub fn parse_mailboxes_str(value: &str) -> Vec<String> {
    let items: Vec<String> = value.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if items.is_empty() {
        vec!["INBOX".to_string()]
    } else {
        items
    }
}

/// `parse_mailboxes` for the JSON form (string, list of strings, or null).
pub fn parse_mailboxes_value(value: Option<&Value>) -> Vec<String> {
    match value {
        None | Some(Value::Null) => vec!["INBOX".to_string()],
        Some(Value::String(s)) => parse_mailboxes_str(s),
        Some(Value::Array(items)) => {
            let items: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            if items.is_empty() {
                vec!["INBOX".to_string()]
            } else {
                items
            }
        }
        Some(other) => {
            let _ = other;
            vec!["INBOX".to_string()]
        }
    }
}

// --------------------------------------------------------------------------------------
// bucket source
// --------------------------------------------------------------------------------------

/// `datetime.now(UTC).isoformat(timespec="seconds")`.
pub fn now_iso() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// An account as stored in the file. `credentials` is ciphertext.
#[derive(Clone, PartialEq)]
pub struct AccountRecord {
    pub user: String,
    pub host: String,
    pub credentials: String,
    pub port: i32,
    pub mailboxes: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Default for AccountRecord {
    fn default() -> Self {
        AccountRecord {
            user: String::new(),
            host: String::new(),
            credentials: String::new(),
            port: 993,
            mailboxes: vec!["INBOX".to_string()],
            enabled: true,
            created_at: now_iso(),
            updated_at: now_iso(),
        }
    }
}

impl std::fmt::Debug for AccountRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRecord")
            .field("user", &self.user)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("mailboxes", &self.mailboxes)
            .field("enabled", &self.enabled)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Python `str()` of a JSON value for the fields the file may hold loosely
/// typed (str() of True is "True", of 1 is "1").
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        other => other.to_string(),
    }
}

/// Python `bool()` of a JSON value.
fn py_bool(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

impl AccountRecord {
    pub fn from_dict(d: &Map<String, Value>) -> Result<AccountRecord, AccountsError> {
        let make_err = |d: &Map<String, Value>, e: String| -> AccountsError {
            let user = d.get("user").map(py_str).unwrap_or_else(|| "?".to_string());
            AccountsError(format!("invalid account record {user:?}: {e}"))
        };
        let user = d.get("user").ok_or_else(|| make_err(d, "'user'".to_string()))?;
        let host = d.get("host").ok_or_else(|| make_err(d, "'host'".to_string()))?;
        let credentials = d.get("credentials").ok_or_else(|| make_err(d, "'credentials'".to_string()))?;
        let port = match d.get("port") {
            None | Some(Value::Null) => 993,
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| make_err(d, "invalid literal for int()".to_string()))?,
            Some(Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| make_err(d, format!("invalid literal for int() with base 10: {s:?}")))?,
            Some(_) => return Err(make_err(d, "int() argument must be a number or string".to_string())),
        };
        Ok(AccountRecord {
            user: py_str(user),
            host: py_str(host),
            credentials: py_str(credentials),
            port: port as i32,
            mailboxes: parse_mailboxes_value(d.get("mailboxes")),
            enabled: match d.get("enabled") {
                None => true,
                Some(v) => py_bool(Some(v)),
            },
            created_at: d.get("created_at").map(py_str).unwrap_or_default(),
            updated_at: d.get("updated_at").map(py_str).unwrap_or_default(),
        })
    }

    /// Refresh `updated_at` after direct field changes (the tail of Python's
    /// `update(**changes)`; callers assign the fields themselves).
    pub fn touch(&mut self) {
        self.updated_at = now_iso();
    }

    pub fn to_account(&self, cipher: &Cipher) -> Result<Account, CryptoError> {
        Ok(Account {
            password: cipher.decrypt(&self.user, &self.credentials)?,
            user: self.user.clone(),
            host: self.host.clone(),
            port: self.port,
            mailboxes: self.mailboxes.clone(),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "user": self.user,
            "host": self.host,
            "credentials": self.credentials,
            "port": self.port,
            "mailboxes": self.mailboxes,
            "enabled": self.enabled,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

/// The parsed accounts file. `etag` of None means the file does not exist yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountsDoc {
    pub records: Vec<AccountRecord>,
    pub etag: Option<String>,
}

impl AccountsDoc {
    pub fn parse(body: &[u8], etag: Option<String>) -> Result<AccountsDoc, AccountsError> {
        let data: Value = serde_json::from_slice(body)
            .map_err(|e| AccountsError(format!("accounts file is not valid JSON: {e}")))?;
        let Value::Object(data) = data else {
            return Err(AccountsError(
                "accounts file must be {\"version\": 1, \"accounts\": [...]}".to_string(),
            ));
        };
        let Some(Value::Array(items)) = data.get("accounts") else {
            return Err(AccountsError(
                "accounts file must be {\"version\": 1, \"accounts\": [...]}".to_string(),
            ));
        };
        let version = data.get("version").map(py_str).unwrap_or_else(|| FILE_VERSION.to_string());
        if version != FILE_VERSION.to_string() {
            return Err(AccountsError(format!("unsupported accounts file version {version}")));
        }
        let mut records = Vec::new();
        for item in items {
            let Value::Object(d) = item else {
                return Err(AccountsError(format!(
                    "invalid account record {}: 'dict' object is required",
                    Value::Null
                )));
            };
            records.push(AccountRecord::from_dict(d)?);
        }
        let mut seen = std::collections::HashSet::new();
        for r in &records {
            if !seen.insert(r.user.clone()) {
                return Err(AccountsError(format!("duplicate account {:?} in accounts file", r.user)));
            }
        }
        Ok(AccountsDoc { records, etag })
    }

    pub fn dump(&self) -> Vec<u8> {
        let accounts: Vec<Value> = self.records.iter().map(|r| r.to_json()).collect();
        let mut out = serde_json::to_string_pretty(&json!({
            "version": FILE_VERSION,
            "accounts": accounts,
        }))
        .unwrap()
        .into_bytes();
        out.push(b'\n');
        out
    }

    pub fn get(&self, user: &str) -> Option<&AccountRecord> {
        self.records.iter().find(|r| r.user == user)
    }

    pub fn require(&self, user: &str) -> Result<&AccountRecord, AccountsError> {
        self.get(user)
            .ok_or_else(|| AccountsError(format!("no account {user:?}")))
    }

    pub fn require_mut(&mut self, user: &str) -> Result<&mut AccountRecord, AccountsError> {
        self.records
            .iter_mut()
            .find(|r| r.user == user)
            .ok_or_else(|| AccountsError(format!("no account {user:?}")))
    }
}

/// Read/change errors: S3/conditional-write failures plus accounts-file
/// validation.
#[derive(Debug, Clone, PartialEq)]
pub enum RepoError {
    Store(StoreError),
    Accounts(AccountsError),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::Store(e) => write!(f, "{e}"),
            RepoError::Accounts(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for RepoError {}

impl From<S3Error> for RepoError {
    fn from(e: S3Error) -> Self {
        RepoError::Store(StoreError::S3(e))
    }
}
impl From<AccountsError> for RepoError {
    fn from(e: AccountsError) -> Self {
        RepoError::Accounts(e)
    }
}

/// Read and change the accounts file. Writes are conditional on the ETag, so
/// two concurrent runs cannot silently overwrite each other.
pub struct AccountsRepo<'a, C: S3Client> {
    pub store: &'a Store<C>,
    pub key: String,
}

impl<'a, C: S3Client> AccountsRepo<'a, C> {
    pub fn new(store: &'a Store<C>, path: &str) -> AccountsRepo<'a, C> {
        AccountsRepo { store, key: store.key(path) }
    }

    pub fn etag(&self) -> Result<Option<String>, S3Error> {
        self.store.head_etag(&self.key)
    }

    pub fn read(&self) -> Result<AccountsDoc, RepoError> {
        match self.store.get_bytes(&self.key)? {
            None => Ok(AccountsDoc::default()),
            Some((body, etag)) => Ok(AccountsDoc::parse(&body, Some(etag))?),
        }
    }

    /// Apply change() to a fresh copy and write it back, retrying if someone
    /// else wrote first. change runs again on the fresh copy after a lost
    /// race, exactly like the Python loop.
    pub fn modify(
        &self,
        change: impl Fn(&mut AccountsDoc),
        attempts: u32,
        log: &Logger,
    ) -> Result<AccountsDoc, RepoError> {
        for i in 0..attempts {
            let mut doc = self.read()?;
            change(&mut doc);
            match self.store.put_bytes(&self.key, &doc.dump(), "application/json", doc.etag.as_deref()) {
                Ok(etag) => {
                    doc.etag = Some(etag);
                    return Ok(doc);
                }
                // someone else wrote first: retry with a fresh read
                Err(StoreError::Conflict(_)) if i + 1 < attempts => {
                    log.info("accounts file changed during update, retrying", &[]);
                    continue;
                }
                Err(e) => return Err(RepoError::Store(e)),
            }
        }
        unreachable!("loop returns on the last attempt")
    }
}

/// Which copy of the file the source has seen: never (sentinel in Python),
/// or the etag of the last read (None = file does not exist yet).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SeenEtag {
    Never,
    Loaded(Option<String>),
}

/// Account source backed by the accounts file. Re-reads it only when its ETag
/// changes. If the file becomes unreadable, keeps serving the last good list.
pub struct BucketAccounts<'a, C: S3Client> {
    pub repo: AccountsRepo<'a, C>,
    pub cipher: Cipher,
    etag: SeenEtag,
    accounts: Vec<Account>,
    /// named `aimap.accounts`, like `logging.getLogger(__name__)`
    pub log: Logger,
}

impl<'a, C: S3Client> BucketAccounts<'a, C> {
    pub fn new(repo: AccountsRepo<'a, C>, cipher: Cipher, log: Logger) -> BucketAccounts<'a, C> {
        BucketAccounts { repo, cipher, etag: SeenEtag::Never, accounts: Vec::new(), log }
    }

    /// The last good account list (never fails; unreadable files keep the
    /// previous list, like the Python error handling).
    pub fn load(&mut self) -> Vec<Account> {
        let etag = match self.repo.etag() {
            Ok(etag) => etag,
            Err(e) => return self.last_good_copy(RepoError::from(e)),
        };
        if self.etag == SeenEtag::Loaded(etag.clone()) {
            return self.accounts.clone();
        }
        let doc = match self.repo.read() {
            Ok(doc) => doc,
            Err(e) => return self.last_good_copy(e),
        };
        let mut accounts = Vec::new();
        for rec in &doc.records {
            if !rec.enabled {
                continue;
            }
            match rec.to_account(&self.cipher) {
                Ok(a) => accounts.push(a),
                Err(e) => {
                    self.log.error("skipping account", &[
                        ("account", LogValue::str(rec.user.clone())),
                        ("error", LogValue::str(e.to_string())),
                    ]);
                }
            }
        }
        self.etag = SeenEtag::Loaded(doc.etag);
        self.accounts = accounts;
        self.log.info("accounts loaded", &[
            ("key", LogValue::str(self.repo.key.clone())),
            ("enabled", LogValue::List(self.accounts.iter().map(|a| a.user.clone()).collect())),
            (
                "disabled",
                LogValue::List(doc.records.iter().filter(|r| !r.enabled).map(|r| r.user.clone()).collect()),
            ),
        ]);
        self.accounts.clone()
    }

    /// `cannot read accounts file, using last good copy`: log the failure and
    /// keep serving the previous list.
    fn last_good_copy(&mut self, e: RepoError) -> Vec<Account> {
        self.log.error("cannot read accounts file, using last good copy", &[
            ("key", LogValue::str(self.repo.key.clone())),
            ("error", LogValue::str(e.to_string())),
            ("accounts", LogValue::Int(self.accounts.len() as i64)),
        ]);
        self.accounts.clone()
    }
}

impl<C: S3Client> AccountSource for BucketAccounts<'_, C> {
    fn load(&mut self) -> Vec<Account> {
        BucketAccounts::load(self)
    }
}
