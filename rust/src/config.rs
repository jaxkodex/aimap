//! Port of `src/aimap/config.py`: service settings from environment variables.
//! Accounts are configured separately (see [`crate::accounts`]).
//!
//! Secret fields are left out of the [`Debug`] output, like `field(repr=False)`
//! in the Python dataclasses.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::jev::Thresholds;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ConfigError {}

/// The full S3Config; the Store and the S3 transport consume their parts.
#[derive(Clone, Default, PartialEq)]
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,
    pub endpoint_url: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub force_path_style: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("endpoint_url", &self.endpoint_url)
            .field("region", &self.region)
            .field("force_path_style", &self.force_path_style)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct ClassifierSettings {
    pub api_key: Option<String>,
    /// pinned so tuned thresholds stay valid
    pub model: String,
    pub thresholds: Thresholds,
    pub body_chars: usize,
    pub concurrency: usize,
    pub max_attempts: i32,
    /// first retry; doubles each attempt, capped at one hour
    pub retry_delay: f64,
    /// a job running longer than this is put back in the queue
    pub job_timeout: f64,
    pub request_timeout: f64,
}

impl Default for ClassifierSettings {
    fn default() -> Self {
        ClassifierSettings {
            api_key: None,
            model: "jev-1.13.0".to_string(),
            thresholds: Thresholds::default(),
            body_chars: 1500,
            concurrency: 4,
            max_attempts: 5,
            retry_delay: 30.0,
            job_timeout: 600.0,
            request_timeout: 120.0,
        }
    }
}

impl std::fmt::Debug for ClassifierSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassifierSettings")
            .field("model", &self.model)
            .field("thresholds", &self.thresholds)
            .field("body_chars", &self.body_chars)
            .field("concurrency", &self.concurrency)
            .field("max_attempts", &self.max_attempts)
            .field("retry_delay", &self.retry_delay)
            .field("job_timeout", &self.job_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct DraftSettings {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout: f64,
    pub max_tokens: i32,
    pub reasoning_effort: String,
}

impl Default for DraftSettings {
    fn default() -> Self {
        DraftSettings {
            base_url: "https://api.deepseek.com".to_string(),
            api_key: None,
            model: "deepseek-flash".to_string(),
            timeout: 45.0,
            max_tokens: 2000,
            reasoning_effort: "low".to_string(),
        }
    }
}

impl std::fmt::Debug for DraftSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraftSettings")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("max_tokens", &self.max_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct ApiSettings {
    pub firebase_project_id: Option<String>,
    /// Firebase lets any Google account sign in; this is the gate
    pub allowed_emails: BTreeSet<String>,
    pub host: String,
    pub port: i32,
    pub pool_size: usize,
    pub draft: DraftSettings,
}

impl Default for ApiSettings {
    fn default() -> Self {
        ApiSettings {
            firebase_project_id: None,
            allowed_emails: BTreeSet::new(),
            host: "0.0.0.0".to_string(),
            port: 8080,
            pool_size: 10,
            draft: DraftSettings::default(),
        }
    }
}

impl std::fmt::Debug for ApiSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiSettings")
            .field("firebase_project_id", &self.firebase_project_id)
            .field("allowed_emails", &self.allowed_emails)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("pool_size", &self.pool_size)
            .field("draft", &self.draft)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub s3: S3Config,
    pub classifier: ClassifierSettings,
    pub api: ApiSettings,
    /// "bucket" or "env"
    pub accounts_source: String,
    pub accounts_path: String,
    pub poll_interval: f64,
    pub initial_fetch_count: i64,
    pub batch_size: usize,
    pub max_backoff: f64,
    pub log_level: String,
    pub log_format: String,
    pub imap_timeout: f64,
    pub database_url: Option<String>,
    pub secret_key: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            s3: S3Config::default(),
            classifier: ClassifierSettings::default(),
            api: ApiSettings::default(),
            accounts_source: "bucket".to_string(),
            accounts_path: "config/accounts.json".to_string(),
            poll_interval: 60.0,
            initial_fetch_count: 100,
            batch_size: 25,
            max_backoff: 1800.0,
            log_level: "INFO".to_string(),
            log_format: "json".to_string(),
            imap_timeout: 60.0,
            database_url: None,
            secret_key: None,
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("s3", &self.s3)
            .field("classifier", &self.classifier)
            .field("api", &self.api)
            .field("accounts_source", &self.accounts_source)
            .field("accounts_path", &self.accounts_path)
            .field("poll_interval", &self.poll_interval)
            .field("initial_fetch_count", &self.initial_fetch_count)
            .field("batch_size", &self.batch_size)
            .field("max_backoff", &self.max_backoff)
            .field("log_level", &self.log_level)
            .field("log_format", &self.log_format)
            .field("imap_timeout", &self.imap_timeout)
            .finish()
    }
}

fn get(env: &HashMap<String, String>, name: &str) -> String {
    env.get(name).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn get_or(env: &HashMap<String, String>, names: &[&str]) -> String {
    for n in names {
        let v = get(env, n);
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

fn int(env: &HashMap<String, String>, name: &str, default: i64) -> Result<i64, ConfigError> {
    let raw = get(env, name);
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<i64>()
        .map_err(|_| ConfigError(format!("{name} must be an integer, got {raw:?}")))
}

fn float(env: &HashMap<String, String>, name: &str, default: f64) -> Result<f64, ConfigError> {
    let raw = get(env, name);
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<f64>()
        .map_err(|_| ConfigError(format!("{name} must be a number, got {raw:?}")))
}

fn bool(env: &HashMap<String, String>, name: &str, default: bool) -> bool {
    let raw = get(env, name).to_lowercase();
    if raw.is_empty() {
        return default;
    }
    matches!(raw.as_str(), "1" | "true" | "yes" | "on")
}

fn opt(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// `need_accounts=false` skips the AIMAP_SECRET_KEY check, for commands that
/// never read the accounts file.
pub fn load_settings(
    env: &HashMap<String, String>,
    need_accounts: bool,
) -> Result<Settings, ConfigError> {
    let bucket = get(env, "S3_BUCKET");
    if bucket.is_empty() {
        return Err(ConfigError("S3_BUCKET is required".to_string()));
    }
    let prefix = get(env, "S3_PREFIX").trim_matches('/').to_string();
    let s3 = S3Config {
        bucket,
        prefix: if prefix.is_empty() { prefix } else { format!("{prefix}/") },
        endpoint_url: opt(get(env, "S3_ENDPOINT_URL")),
        region: opt(get_or(env, &["S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"])),
        access_key_id: opt(get(env, "S3_ACCESS_KEY_ID")),
        secret_access_key: opt(get(env, "S3_SECRET_ACCESS_KEY")),
        force_path_style: bool(env, "S3_FORCE_PATH_STYLE", false),
    };

    let mut source = get(env, "ACCOUNTS_SOURCE").to_lowercase();
    if source.is_empty() {
        source = if !get(env, "IMAP_USER").is_empty() { "env" } else { "bucket" }.to_string();
    }
    if source != "bucket" && source != "env" {
        return Err(ConfigError(format!(
            "ACCOUNTS_SOURCE must be 'bucket' or 'env', got {source:?}"
        )));
    }
    let secret_key = opt(get(env, "AIMAP_SECRET_KEY"));
    if source == "bucket" && secret_key.is_none() && need_accounts {
        return Err(ConfigError(
            "AIMAP_SECRET_KEY is required for the bucket accounts file (generate: aimap keygen)".to_string(),
        ));
    }

    let allowed_emails: BTreeSet<String> = get(env, "AIMAP_ALLOWED_EMAILS")
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let settings = Settings {
        s3,
        accounts_source: source,
        accounts_path: {
            let p = get(env, "ACCOUNTS_PATH").trim_matches('/').to_string();
            if p.is_empty() { "config/accounts.json".to_string() } else { p }
        },
        secret_key,
        poll_interval: float(env, "POLL_INTERVAL_SECONDS", 60.0)?,
        initial_fetch_count: int(env, "INITIAL_FETCH_COUNT", 100)?,
        batch_size: int(env, "BATCH_SIZE", 25)? as usize,
        imap_timeout: float(env, "IMAP_TIMEOUT_SECONDS", 60.0)?,
        max_backoff: float(env, "MAX_BACKOFF_SECONDS", 1800.0)?,
        log_level: {
            let l = get(env, "LOG_LEVEL").to_uppercase();
            if l.is_empty() { "INFO".to_string() } else { l }
        },
        log_format: {
            let f = get(env, "LOG_FORMAT").to_lowercase();
            if f.is_empty() { "json".to_string() } else { f }
        },
        database_url: opt(get(env, "DATABASE_URL")),
        classifier: ClassifierSettings {
            api_key: opt(get_or(env, &["JEV_API_KEY", "TYPESAFE_API_KEY"])),
            model: {
                let m = get(env, "JEV_MODEL");
                if m.is_empty() { "jev-1.13.0".to_string() } else { m }
            },
            thresholds: Thresholds {
                pattern_confidence: float(env, "JEV_PATTERN_CONFIDENCE", 0.5)?,
                tag_threshold: float(env, "JEV_TAG_THRESHOLD", 0.8)?,
                review_confidence: float(env, "JEV_REVIEW_CONFIDENCE", 0.4)?,
            },
            body_chars: int(env, "JEV_BODY_CHARS", 1500)? as usize,
            concurrency: int(env, "CLASSIFY_CONCURRENCY", 4)? as usize,
            max_attempts: int(env, "CLASSIFY_MAX_ATTEMPTS", 5)? as i32,
            retry_delay: float(env, "CLASSIFY_RETRY_SECONDS", 30.0)?,
            job_timeout: float(env, "CLASSIFY_JOB_TIMEOUT_SECONDS", 600.0)?,
            request_timeout: float(env, "JEV_TIMEOUT_SECONDS", 120.0)?,
        },
        api: ApiSettings {
            firebase_project_id: opt(get(env, "FIREBASE_PROJECT_ID")),
            allowed_emails,
            host: {
                let h = get(env, "API_HOST");
                if h.is_empty() { "0.0.0.0".to_string() } else { h }
            },
            port: int(env, "PORT", 8080)? as i32,
            pool_size: int(env, "API_POOL_SIZE", 10)? as usize,
            draft: DraftSettings {
                base_url: {
                    let b = get(env, "AIMAP_DRAFT_BASE_URL");
                    if b.is_empty() { "https://api.deepseek.com".to_string() } else { b }
                },
                api_key: opt(get(env, "AIMAP_DRAFT_API_KEY")),
                model: {
                    let m = get(env, "AIMAP_DRAFT_MODEL");
                    if m.is_empty() { "deepseek-flash".to_string() } else { m }
                },
                timeout: float(env, "AIMAP_DRAFT_TIMEOUT", 45.0)?,
                max_tokens: int(env, "AIMAP_DRAFT_MAX_TOKENS", 2000)? as i32,
                reasoning_effort: {
                    let e = get(env, "AIMAP_DRAFT_EFFORT");
                    if e.is_empty() { "low".to_string() } else { e }
                },
            },
        },
    };
    if settings.poll_interval <= 0.0 {
        return Err(ConfigError("POLL_INTERVAL_SECONDS must be > 0".to_string()));
    }
    if settings.batch_size == 0 {
        return Err(ConfigError("BATCH_SIZE must be > 0".to_string()));
    }
    if settings.initial_fetch_count < 0 {
        return Err(ConfigError("INITIAL_FETCH_COUNT must be >= 0".to_string()));
    }
    if settings.classifier.concurrency == 0 {
        return Err(ConfigError("CLASSIFY_CONCURRENCY must be > 0".to_string()));
    }
    if settings.api.pool_size == 0 {
        return Err(ConfigError("API_POOL_SIZE must be > 0".to_string()));
    }
    if settings.classifier.max_attempts <= 0 {
        return Err(ConfigError("CLASSIFY_MAX_ATTEMPTS must be > 0".to_string()));
    }
    Ok(settings)
}

/// `load_settings` against the process environment.
pub fn load_settings_from_env(need_accounts: bool) -> Result<Settings, ConfigError> {
    load_settings(&std::env::vars().collect(), need_accounts)
}

/// Minimal .env loader for local runs. Existing environment variables win.
pub fn load_dotenv(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().strip_prefix("export ").unwrap_or(key.trim()).trim();
        let mut value = value.trim().to_string();
        if value.len() >= 2 {
            let bytes = value.as_bytes();
            let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
            if first == last && (first == b'"' || first == b'\'') {
                value = value[1..value.len() - 1].to_string();
            }
        }
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
        }
    }
}
