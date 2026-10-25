//! aimap: the command line, port of `cli.py`. Parsing is hand-rolled to keep
//! the surface identical (subcommands, repeatable --mailbox, --password-stdin,
//! '-' files). All I/O goes through `Hooks`, so tests inject an in-memory S3,
//! a recording `verify_login` and in-memory stdio.

use crate::accounts::{
    clean_password, Account, AccountRecord, AccountsDoc, AccountsError, AccountsRepo, AccountSource,
    BucketAccounts, EnvAccounts, RepoError,
};
use crate::backfill;
use crate::catalog::PgCatalog;
use crate::config::{self, Settings, S3Config};
use crate::crypto::Cipher;
use crate::db::{self, Pool};
use crate::draft;
use crate::imap::ImapSource;
use crate::logs::{LogValue, Logger};
use crate::profiles;
use crate::queue;
use crate::storage::{S3Client, Store, StoreError};
use crate::supervise::{self, Supervisor};
use crate::worker::Worker;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

const DB_COMMANDS: [&str; 7] = ["migrate", "backfill", "classify", "jobs", "profiles", "patterns", "api"];

const USAGE: &str = "usage: aimap [--env-file FILE] COMMAND ...
  run | once | check | keygen | migrate | backfill | api | all
  classify [--once]
  jobs status | jobs retry [--done]
  profiles list | profiles show NAME | profiles set NAME --file FILE
  patterns list PROFILE | patterns import PROFILE FILE [--format csv|json]
  accounts list | add | update | set-password | enable | disable | remove | rotate-key | set-profile";

// --------------------------------------------------------------------------------------
// parsed command line
// --------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Version,
    Run,
    Once,
    Check,
    Keygen,
    Migrate,
    Backfill,
    Api,
    All,
    Classify { once: bool },
    JobsStatus,
    JobsRetry { done: bool },
    ProfilesList,
    ProfilesShow { name: String },
    ProfilesSet { name: String, file: String },
    PatternsList { profile: String },
    PatternsImport { profile: String, file: String, format: Option<String> },
    AccountsSetProfile { user: String, profile: String },
    AccountsList,
    AccountsAdd {
        user: String,
        host: String,
        port: i32,
        mailboxes: Vec<String>,
        password_stdin: bool,
        no_verify: bool,
        disabled: bool,
    },
    AccountsUpdate {
        user: String,
        host: Option<String>,
        port: Option<i32>,
        mailboxes: Vec<String>,
        no_verify: bool,
    },
    AccountsSetPassword { user: String, password_stdin: bool, no_verify: bool },
    AccountsEnable { user: String },
    AccountsDisable { user: String },
    AccountsRemove { user: String },
    AccountsRotateKey,
}

impl Command {
    /// the argparse subcommand name (used for DB_COMMANDS and log routing)
    pub fn name(&self) -> &'static str {
        match self {
            Command::Version | Command::Keygen => "keygen",
            Command::Run => "run",
            Command::Once => "once",
            Command::Check => "check",
            Command::Migrate => "migrate",
            Command::Backfill => "backfill",
            Command::Api => "api",
            Command::All => "all",
            Command::Classify { .. } => "classify",
            Command::JobsStatus | Command::JobsRetry { .. } => "jobs",
            Command::ProfilesList | Command::ProfilesShow { .. } | Command::ProfilesSet { .. } => "profiles",
            Command::PatternsList { .. } | Command::PatternsImport { .. } => "patterns",
            _ => "accounts",
        }
    }

    fn is_accounts_file_action(&self) -> bool {
        matches!(
            self,
            Command::AccountsList
                | Command::AccountsAdd { .. }
                | Command::AccountsUpdate { .. }
                | Command::AccountsSetPassword { .. }
                | Command::AccountsEnable { .. }
                | Command::AccountsDisable { .. }
                | Command::AccountsRemove { .. }
                | Command::AccountsRotateKey
        )
    }
}

/// Parse the argument list (already without the program name). Errors carry
/// the message argparse would print; they exit 2.
pub fn parse(argv: &[String]) -> Result<(String, Command), String> {
    let mut env_file = ".env".to_string();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" => return Ok((env_file, Command::Version)),
            "--env-file" => {
                i += 1;
                env_file = argv.get(i).ok_or("--env-file requires a value")?.clone();
            }
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    let mut it = rest.into_iter();
    let Some(command) = it.next() else {
        return Ok((env_file, Command::Run)); // args.command or "run"
    };
    let mut flags = Flags::new(it);
    let parsed = match command.as_str() {
        "run" => Command::Run,
        "once" => Command::Once,
        "check" => Command::Check,
        "keygen" => Command::Keygen,
        "migrate" => Command::Migrate,
        "backfill" => Command::Backfill,
        "api" => Command::Api,
        "all" => Command::All,
        "classify" => Command::Classify { once: flags.has("--once") },
        "jobs" => match flags.next_positional("jobs action")?.as_str() {
            "status" => Command::JobsStatus,
            "retry" => Command::JobsRetry { done: flags.has("--done") },
            other => return Err(format!("jobs: unknown action {other:?} (status, retry)")),
        },
        "profiles" => match flags.next_positional("profiles action")?.as_str() {
            "list" => Command::ProfilesList,
            "show" => Command::ProfilesShow { name: flags.next_positional("name")? },
            "set" => Command::ProfilesSet {
                name: flags.next_positional("name")?,
                file: flags.value("--file")?,
            },
            other => return Err(format!("profiles: unknown action {other:?} (list, show, set)")),
        },
        "patterns" => match flags.next_positional("patterns action")?.as_str() {
            "list" => Command::PatternsList { profile: flags.next_positional("profile")? },
            "import" => {
                let profile = flags.next_positional("profile")?;
                let file = flags.next_positional("file")?;
                let format = match flags.opt_value("--format")? {
                    Some(f) if f == "csv" || f == "json" => Some(f),
                    Some(f) => return Err(format!("--format: invalid choice: {f:?} (choose from 'csv', 'json')")),
                    None => None,
                };
                Command::PatternsImport { profile, file, format }
            }
            other => return Err(format!("patterns: unknown action {other:?} (list, import)")),
        },
        "accounts" => {
            let action = flags.next_positional("accounts action")?;
            match action.as_str() {
                "list" => Command::AccountsList,
                "add" => Command::AccountsAdd {
                    user: flags.next_positional("user")?,
                    host: flags.value("--host")?,
                    port: flags.opt_value("--port")?.map(|v| parse_port(&v)).transpose()?.unwrap_or(993),
                    mailboxes: flags.all("--mailbox"),
                    password_stdin: flags.has("--password-stdin"),
                    no_verify: flags.has("--no-verify"),
                    disabled: flags.has("--disabled"),
                },
                "update" => Command::AccountsUpdate {
                    user: flags.next_positional("user")?,
                    host: flags.opt_value("--host")?,
                    port: flags.opt_value("--port")?.map(|v| parse_port(&v)).transpose()?,
                    mailboxes: flags.all("--mailbox"),
                    no_verify: flags.has("--no-verify"),
                },
                "set-password" => Command::AccountsSetPassword {
                    user: flags.next_positional("user")?,
                    password_stdin: flags.has("--password-stdin"),
                    no_verify: flags.has("--no-verify"),
                },
                "enable" => Command::AccountsEnable { user: flags.next_positional("user")? },
                "disable" => Command::AccountsDisable { user: flags.next_positional("user")? },
                "remove" => Command::AccountsRemove { user: flags.next_positional("user")? },
                "rotate-key" => Command::AccountsRotateKey,
                "set-profile" => Command::AccountsSetProfile {
                    user: flags.next_positional("user")?,
                    profile: flags.next_positional("profile")?,
                },
                other => return Err(format!("accounts: unknown action {other:?}")),
            }
        }
        other => return Err(format!("unknown command {other:?}")),
    };
    let leftover = flags.unused();
    if !leftover.is_empty() {
        return Err(format!("unrecognized arguments: {}", leftover.join(" ")));
    }
    Ok((env_file, parsed))
}

fn parse_port(value: &str) -> Result<i32, String> {
    value.parse::<i32>().map_err(|_| format!("--port: invalid int value: {value:?}"))
}

/// Leftover flag tracking for the subcommand arguments.
struct Flags {
    items: Vec<String>,
}

impl Flags {
    fn new(items: impl Iterator<Item = String>) -> Flags {
        Flags { items: items.collect() }
    }

    fn has(&mut self, name: &str) -> bool {
        if let Some(at) = self.items.iter().position(|s| s == name) {
            self.items.remove(at);
            return true;
        }
        false
    }

    fn value(&mut self, name: &str) -> Result<String, String> {
        self.opt_value(name)?.ok_or_else(|| format!("{name} is required"))
    }

    fn opt_value(&mut self, name: &str) -> Result<Option<String>, String> {
        if let Some(at) = self.items.iter().position(|s| s == name) {
            self.items.remove(at);
            let value = self.items.get(at).ok_or_else(|| "expected one argument".to_string())?.clone();
            self.items.remove(at);
            return Ok(Some(value));
        }
        Ok(None)
    }

    /// All values for a repeatable flag, in order.
    fn all(&mut self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(v) = self.opt_value(name).ok().flatten() {
            out.push(v);
        }
        out
    }

    fn next_positional(&mut self, what: &str) -> Result<String, String> {
        if self.items.is_empty() {
            return Err(format!("the following arguments are required: {what}"));
        }
        Ok(self.items.remove(0))
    }

    fn unused(&self) -> &[String] {
        &self.items
    }
}

// --------------------------------------------------------------------------------------
// entry point
// --------------------------------------------------------------------------------------

/// Everything `main` needs from the process: real S3, real IMAP, real stdio
/// in production; fakes in tests.
pub struct Hooks<'a, C> {
    pub verify_login: &'a dyn Fn(&Account, f64) -> Result<(), String>,
    pub s3: &'a dyn Fn(&S3Config) -> C,
    pub stdin: &'a mut dyn Read,
    pub stdin_tty: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

fn get(env: &HashMap<String, String>, name: &str) -> String {
    env.get(name).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The `main(argv)` of Python. Returns the process exit code.
pub fn main_with<C: S3Client + Send + Sync + Clone + 'static>(env: &HashMap<String, String>, argv: &[String], hooks: &mut Hooks<C>) -> i32 {
    let mut args: Vec<String> = argv.to_vec();
    // the image's ENTRYPOINT is aimap; accept "aimap migrate" as a command too
    if args.first().map(String::as_str) == Some("aimap") {
        args.remove(0);
    }
    let (env_file, command) = match parse(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(hooks.err, "{USAGE}");
            let _ = writeln!(hooks.err, "aimap: error: {message}");
            return 2;
        }
    };

    if command == Command::Version {
        let _ = writeln!(hooks.out, "aimap {VERSION}");
        return 0;
    }
    if command == Command::Keygen {
        let _ = writeln!(hooks.out, "{}", crate::crypto::generate_key());
        return 0;
    }

    // load_dotenv: variables from the file, existing environment wins
    let mut env = env.clone();
    for (key, value) in config::read_dotenv(Path::new(&env_file)) {
        env.entry(key).or_insert(value);
    }

    if command == Command::All {
        let level = { let v = get(&env, "LOG_LEVEL"); if v.is_empty() { "INFO".into() } else { v.to_uppercase() } };
        let fmt = { let v = get(&env, "LOG_FORMAT"); if v.is_empty() { "json".into() } else { v.to_lowercase() } };
        let log = Logger::stdout_logger_for(&level, &fmt, "aimap.supervise");
        let mut supervisor =
            Supervisor::new(supervise::aimap_commands(&env_file), 30.0, 0.5, log);
        supervisor.install_signal_handlers();
        return supervisor.run();
    }

    let set_profile = matches!(command, Command::AccountsSetProfile { .. });
    let need_accounts = !DB_COMMANDS.contains(&command.name()) && !set_profile;
    let settings = match config::load_settings(&env, need_accounts) {
        Ok(settings) => settings,
        Err(e) => {
            let log = Logger::stdout_logger_for("INFO", "json", "aimap");
            log.error("invalid configuration", &[("error", LogValue::str(e.0))]);
            return 2;
        }
    };

    if command.is_accounts_file_action() {
        let log = Logger::stdout_logger_for(&settings.log_level, "text", "aimap");
        return accounts_command(&command, &settings, &env, hooks, &log);
    }

    let interactive = matches!(
        command,
        Command::JobsStatus
            | Command::JobsRetry { .. }
            | Command::ProfilesList
            | Command::ProfilesShow { .. }
            | Command::ProfilesSet { .. }
            | Command::PatternsList { .. }
            | Command::PatternsImport { .. }
            | Command::AccountsSetProfile { .. }
            | Command::Migrate
    );
    let fmt = if interactive { "text" } else { settings.log_format.as_str() };
    let log = Logger::stdout_logger_for(&settings.log_level, fmt, "aimap");

    let Some(database_url) = settings.database_url.clone() else {
        log.error("DATABASE_URL is required", &[]);
        return 2;
    };

    if command == Command::Migrate {
        return match db::migrate(&database_url, &log) {
            Ok(applied) => {
                let suffix =
                    if applied.is_empty() { String::new() } else { format!(": {}", applied.join(", ")) };
                let _ = writeln!(hooks.out, "applied {} migration(s){suffix}", applied.len());
                0
            }
            Err(e) => {
                log.error("migration failed", &[("error", LogValue::str(e.to_string()))]);
                4
            }
        };
    }

    let pool_size =
        if command == Command::Api { settings.api.pool_size } else { settings.classifier.concurrency + 2 };
    let pool = match Pool::open(&database_url, pool_size) {
        Ok(pool) => pool,
        Err(e) => {
            log.error("cannot reach the database", &[("error", LogValue::str(e.to_string()))]);
            return 4;
        }
    };
    let schema = pool.get();
    match schema {
        Err(e) => {
            log.error("cannot reach the database", &[("error", LogValue::str(e.to_string()))]);
            return 4;
        }
        Ok(mut conn) => {
            if let Err(crate::db::SchemaError(message)) = db::require_current(&mut *conn) {
                log.error(&message, &[]);
                return 4;
            }
        }
    }

    
    match command {
        Command::JobsStatus
        | Command::JobsRetry { .. }
        | Command::ProfilesList
        | Command::ProfilesShow { .. }
        | Command::ProfilesSet { .. }
        | Command::PatternsList { .. }
        | Command::PatternsImport { .. }
        | Command::AccountsSetProfile { .. } => admin_command(&command, &pool, hooks, &log),
        _ => service_command(&command, &settings, &env, &pool, hooks, &log),
    }
}

// --------------------------------------------------------------------------------------
// services (run, once, check, classify, backfill, api)
// --------------------------------------------------------------------------------------

fn service_command<C: S3Client + Send + Sync + Clone + 'static>(
    command: &Command,
    settings: &Settings,
    env: &HashMap<String, String>,
    pool: &Pool,
    hooks: &mut Hooks<C>,
    log: &Logger,
) -> i32 {
    let store = Store::new(settings.s3.clone(), (hooks.s3)(&settings.s3));
    if let Err(e) = store.check() {
        log.error("cannot reach S3 bucket", &[
            ("bucket", LogValue::str(settings.s3.bucket.clone())),
            ("error", LogValue::str(e.code)),
        ]);
        return 3;
    }
    let mut catalog = PgCatalog::new();
    let mut conn = match pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            log.error("cannot reach the database", &[("error", LogValue::str(e.to_string()))]);
            return 4;
        }
    };

    match command {
        Command::Backfill => {
            let result = match backfill::backfill(&store, &mut catalog, &mut conn, 500, log) {
                Ok(result) => result,
                Err(e) => {
                    log.error("backfill failed", &[("error", LogValue::str(e.to_string()))]);
                    return 1;
                }
            };
            log.info("backfill complete", &[
                ("scanned", LogValue::Int(result.scanned as i64)),
                ("recorded", LogValue::Int(result.recorded as i64)),
                ("new_messages", LogValue::Int(result.new_messages as i64)),
                ("skipped", LogValue::Int(result.skipped as i64)),
            ]);
            0
        }
        Command::Api => serve_api(settings, pool, &store, log),
        Command::Classify { once } => {
            let Some(api_key) = settings.classifier.api_key.clone() else {
                log.error("JEV_API_KEY is required for classify", &[]);
                return 2;
            };
            let cs = settings.classifier.clone();
            let base_url = env
                .get("JEV_BASE_URL")
                .cloned()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| crate::typesafe::DEFAULT_BASE_URL.to_string());
            let classifier = crate::classifier::Classifier {
                settings: cs.clone(),
                pool,
                store: &store,
                asker_factory: {
                    let base_url = base_url.clone();
                    move || {
                        Box::new(crate::typesafe::TypeSafeAsker::new(
                            api_key.clone(),
                            cs.model.clone(),
                            base_url.clone(),
                            Duration::from_secs_f64(cs.request_timeout),
                        )) as Box<dyn crate::classifier::Asker>
                    }
                },
                stop: crate::classifier::StopEvent::new(),
                idle_wait: 5.0,
                reclaim_every: 60.0,
                log: Logger { name: "aimap.classifier".into(), ..log.clone() },
            };
            classifier.install_signal_handlers();
            if *once {
                let n = classifier.drain();
                log.info("classify pass complete", &[("processed", LogValue::Int(n as i64))]);
            } else {
                classifier.run_forever();
            }
            0
        }
        Command::Check => {
            let mut source = match account_source(settings, &store, env, log) {
                Ok(source) => source,
                Err(e) => {
                    log.error("invalid accounts configuration", &[("error", LogValue::str(e))]);
                    return 2;
                }
            };
            log.info("database ok", &[]);
            check(settings, &mut *source, hooks.verify_login, log)
        }
        Command::Once | Command::Run => {
            let mut source = match account_source(settings, &store, env, log) {
                Ok(source) => source,
                Err(e) => {
                    log.error("invalid accounts configuration", &[("error", LogValue::str(e))]);
                    return 2;
                }
            };
            let worker_log = Logger { name: "aimap.worker".into(), ..log.clone() };
            let mut worker =
                Worker::new(settings.clone(), &store, &mut catalog, &mut *source, worker_log);
            worker.install_signal_handlers();
            if *command == Command::Once {
                let stored = worker.run_once(&mut conn);
                log.info("pass complete", &[
                    ("stored", LogValue::Int(stored as i64)),
                    ("failed_accounts", LogValue::List(worker.failed_accounts.clone())),
                ]);
                if worker.failed_accounts.is_empty() {
                    0
                } else {
                    1
                }
            } else {
                worker.run_forever(&mut conn);
                0
            }
        }
        _ => unreachable!("service_command got a non-service command"),
    }
}

/// The HTTP API: Firebase token verification, the draft config from the
/// environment, and the threaded server until the process is killed.
fn serve_api<C: S3Client + Send + Sync + Clone + 'static>(
    settings: &Settings,
    pool: &Pool,
    store: &Store<C>,
    log: &Logger,
) -> i32 {
    let a = &settings.api;
    let Some(project_id) = a.firebase_project_id.clone() else {
        log.error("FIREBASE_PROJECT_ID is required for api", &[]);
        return 2;
    };
    if a.allowed_emails.is_empty() {
        log.error("AIMAP_ALLOWED_EMAILS is required for api: nobody could sign in", &[]);
        return 2;
    }
    let verifier = crate::auth::FirebaseVerifier::new(&project_id, a.allowed_emails.clone());
    let d = &a.draft;
    let draft_config = draft::DraftConfig {
        base_url: d.base_url.clone(),
        api_key: d.api_key.clone(),
        model: d.model.clone(),
        timeout_secs: d.timeout.max(1.0) as u64,
        max_tokens: d.max_tokens.max(0) as u32,
        reasoning_effort: d.reasoning_effort.clone(),
    };
    let state = Arc::new(crate::api::AppState {
        pool: pool.clone(),
        store: store.clone(),
        verifier: Arc::new(verifier),
        draft_config: Some(draft_config),
        draft_transport: Arc::new(draft::HttpChatTransport),
        log: Logger { name: "aimap.api".into(), ..log.clone() },
    });
    log.info("api starting", &[
        ("port", LogValue::Int(a.port as i64)),
        ("allowed_emails", LogValue::Int(a.allowed_emails.len() as i64)),
    ]);
    match crate::api::serve(state, &a.host, a.port as u16) {
        Ok(()) => 0,
        Err(e) => {
            log.error("cannot bind the API server", &[("error", LogValue::str(e.to_string()))]);
            1
        }
    }
}

fn account_source<'a, C: S3Client>(
    settings: &Settings,
    store: &'a Store<C>,
    env: &HashMap<String, String>,
    log: &Logger,
) -> Result<Box<dyn AccountSource + 'a>, String> {
    if settings.accounts_source == "env" {
        let source = EnvAccounts::new(env).map_err(|e| e.0)?;
        Ok(Box::new(source))
    } else {
        let secret = settings.secret_key.clone().unwrap_or_default();
        let cipher = Cipher::new(&secret).map_err(|e| e.0)?;
        let accounts_log = Logger { name: "aimap.accounts".into(), ..log.clone() };
        Ok(Box::new(BucketAccounts::new(
            AccountsRepo::new(store, &settings.accounts_path),
            cipher,
            accounts_log,
        )))
    }
}

fn check(
    settings: &Settings,
    source: &mut dyn AccountSource,
    verify_login: &dyn Fn(&Account, f64) -> Result<(), String>,
    log: &Logger,
) -> i32 {
    log.info("s3 ok", &[("bucket", LogValue::str(settings.s3.bucket.clone()))]);
    let accounts = source.load();
    if accounts.is_empty() {
        log.warning("no enabled accounts", &[]);
    }
    let mut ok = true;
    for acct in accounts {
        match verify_login(&acct, settings.imap_timeout) {
            Ok(()) => log.info("imap ok", &[
                ("account", LogValue::str(acct.user.clone())),
                ("mailboxes", LogValue::List(acct.mailboxes.clone())),
            ]),
            Err(e) => {
                ok = false;
                log.error("imap failed", &[
                    ("account", LogValue::str(acct.user.clone())),
                    ("error", LogValue::str(e)),
                ])
            }
        }
    }
    if ok {
        0
    } else {
        1
    }
}

/// The real login check: connect to IMAP and select every mailbox.
pub fn real_verify_login(acct: &Account, timeout: f64) -> Result<(), String> {
    let mut source = ImapSource::connect(acct, Duration::from_secs_f64(timeout)).map_err(|e| e.0)?;
    for mailbox in &acct.mailboxes {
        source.select(mailbox).map_err(|e| e.0)?;
    }
    source.close();
    Ok(())
}

// --------------------------------------------------------------------------------------
// admin commands (jobs, profiles, patterns, accounts set-profile)
// --------------------------------------------------------------------------------------

fn admin_command<C: S3Client>(command: &Command, pool: &Pool, hooks: &mut Hooks<C>, log: &Logger) -> i32 {
    let mut conn = match pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            let _ = writeln!(hooks.err, "error: {e}");
            return 1;
        }
    };
    match admin(command, &mut conn, hooks, log) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(hooks.err, "error: {message}");
            1
        }
    }
}

fn admin<C: S3Client>(
    command: &Command,
    conn: &mut postgres::Client,
    hooks: &mut Hooks<C>,
    _log: &Logger,
) -> Result<i32, String> {
    match command {
        Command::JobsStatus => {
            let rows = queue::counts(conn).map_err(db_error)?;
            if rows.is_empty() {
                let _ = writeln!(hooks.out, "no jobs");
            }
            for (stage, status, n) in rows {
                let _ = writeln!(hooks.out, "{stage:<10} {status:<8} {n}");
            }
            Ok(0)
        }
        Command::JobsRetry { done } => {
            let statuses: Vec<&str> = if *done { vec!["failed", "done"] } else { vec!["failed"] };
            let n = queue::requeue(conn, "classify", &statuses).map_err(db_error)?;
            let _ = writeln!(hooks.out, "requeued {n} job(s)");
            Ok(0)
        }
        Command::ProfilesList => {
            let _ = writeln!(hooks.out, "{:<20} {:>8} {:>8}", "PROFILE", "ACCOUNTS", "PATTERNS");
            for (name, accounts, patterns) in profiles::list_all(conn).map_err(db_error)? {
                let _ = writeln!(hooks.out, "{name:<20} {accounts:>8} {patterns:>8}");
            }
            Ok(0)
        }
        Command::ProfilesShow { name } => {
            let profile = profiles::get(conn, name).map_err(|e| e.to_string())?;
            let json = serde_json::to_string_pretty(&profile.profile).map_err(|e| e.to_string())?;
            let _ = writeln!(hooks.out, "{json}");
            Ok(0)
        }
        Command::ProfilesSet { name, file } => {
            let text = read_input(file, hooks)?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            profiles::put(conn, name, &value).map_err(|e| e.to_string())?;
            let _ = writeln!(hooks.out, "saved profile {name}");
            Ok(0)
        }
        Command::PatternsList { profile } => {
            let profile = profiles::get(conn, profile).map_err(|e| e.to_string())?;
            for p in profiles::patterns(conn, profile.id).map_err(db_error)? {
                let _ = writeln!(
                    hooks.out,
                    "{:<6} {:<12} {}  [{}]",
                    p.importance,
                    p.action_bucket,
                    p.insight,
                    p.tags.join("; ")
                );
            }
            Ok(0)
        }
        Command::PatternsImport { profile, file, format } => {
            let profile_row = profiles::get(conn, profile).map_err(|e| e.to_string())?;
            let fmt = format
                .clone()
                .unwrap_or_else(|| if file.ends_with(".json") { "json".to_string() } else { "csv".to_string() });
            let text = read_input(file, hooks)?;
            let items = profiles::parse_patterns(&text, &fmt).map_err(|e| e.to_string())?;
            let n = items.len();
            let mut tx = conn.transaction().map_err(db_error)?;
            profiles::replace_patterns(&mut tx, profile_row.id, &items).map_err(|e| e.to_string())?;
            tx.commit().map_err(db_error)?;
            let _ = writeln!(hooks.out, "imported {n} pattern(s) into {profile}");
            Ok(0)
        }
        Command::AccountsSetProfile { user, profile } => {
            profiles::assign(conn, user, profile).map_err(|e| e.to_string())?;
            let _ = writeln!(hooks.out, "{user} now uses profile {profile}");
            Ok(0)
        }
        _ => unreachable!("admin got a non-admin command"),
    }
}

fn db_error(e: postgres::Error) -> String {
    e.to_string()
}

fn read_input<C: S3Client>(path: &str, hooks: &mut Hooks<C>) -> Result<String, String> {
    if path == "-" {
        let mut text = String::new();
        hooks.stdin.read_to_string(&mut text).map_err(|e| e.to_string())?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

// --------------------------------------------------------------------------------------
// accounts subcommands
// --------------------------------------------------------------------------------------

enum CliError {
    Message(String),
    Conflict,
}

impl From<AccountsError> for CliError {
    fn from(e: AccountsError) -> Self {
        CliError::Message(e.0)
    }
}
impl From<crate::crypto::CryptoError> for CliError {
    fn from(e: crate::crypto::CryptoError) -> Self {
        CliError::Message(e.0)
    }
}
impl From<RepoError> for CliError {
    fn from(e: RepoError) -> Self {
        match e {
            RepoError::Store(StoreError::Conflict(_)) => CliError::Conflict,
            RepoError::Store(other) => CliError::Message(other.to_string()),
            RepoError::Accounts(e) => CliError::Message(e.0),
        }
    }
}
impl From<CliError> for String {
    fn from(e: CliError) -> Self {
        match e {
            CliError::Message(m) => m,
            CliError::Conflict => "the accounts file kept changing under us; try again".to_string(),
        }
    }
}

fn accounts_command<C: S3Client>(
    command: &Command,
    settings: &Settings,
    env: &HashMap<String, String>,
    hooks: &mut Hooks<C>,
    log: &Logger,
) -> i32 {
    let _ = env;
    let Some(secret) = settings.secret_key.clone() else {
        log.error("AIMAP_SECRET_KEY is required to manage the accounts file (generate: aimap keygen)", &[]);
        return 2;
    };
    let Ok(cipher) = Cipher::new(&secret) else {
        let _ = writeln!(hooks.err, "error: invalid AIMAP_SECRET_KEY");
        return 1;
    };
    let store = Store::new(settings.s3.clone(), (hooks.s3)(&settings.s3));
    let repo = AccountsRepo::new(&store, &settings.accounts_path);
    match accounts_action(command, settings, &cipher, &repo, hooks, log) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(hooks.err, "error: {}", String::from(e));
            1
        }
    }
}

fn accounts_action<C: S3Client>(
    command: &Command,
    settings: &Settings,
    cipher: &Cipher,
    repo: &AccountsRepo<C>,
    hooks: &mut Hooks<C>,
    log: &Logger,
) -> Result<i32, CliError> {
    match command {
        Command::AccountsList => {
            let doc = repo.read()?;
            if doc.records.is_empty() {
                let _ = writeln!(hooks.out, "no accounts in s3://{}/{}", settings.s3.bucket, repo.key);
                return Ok(0);
            }
            let mut rows: Vec<Vec<String>> = vec![vec![
                "USER".into(),
                "HOST".into(),
                "PORT".into(),
                "MAILBOXES".into(),
                "ENABLED".into(),
                "KEY".into(),
                "UPDATED".into(),
            ]];
            for r in &doc.records {
                let key_ok = match cipher.decrypt(&r.user, &r.credentials) {
                    Ok(_) => "ok",
                    Err(_) => "cannot decrypt",
                };
                rows.push(vec![
                    r.user.clone(),
                    r.host.clone(),
                    r.port.to_string(),
                    r.mailboxes.join(","),
                    if r.enabled { "yes" } else { "no" }.to_string(),
                    key_ok.to_string(),
                    r.updated_at.clone(),
                ]);
            }
            let widths: Vec<usize> =
                (0..rows[0].len()).map(|i| rows.iter().map(|row| row[i].len()).max().unwrap_or(0)).collect();
            for row in rows {
                let cells: Vec<String> = row
                    .iter()
                    .zip(&widths)
                    .map(|(cell, w)| format!("{cell:<w$}"))
                    .collect();
                let _ = writeln!(hooks.out, "{}", cells.join("  ").trim_end());
            }
            Ok(0)
        }
        Command::AccountsAdd { user, host, port, mailboxes, password_stdin, no_verify, disabled } => {
            if repo.read()?.get(user).is_some() {
                return Err(AccountsError(format!("account '{user}' already exists (use update or set-password)")).into());
            }
            let password = read_password(*password_stdin, *no_verify, hooks)?;
            let acct = Account {
                user: user.clone(),
                password,
                host: host.clone(),
                port: *port,
                mailboxes: mailboxes_from_flags(mailboxes),
            };
            if !no_verify {
                verify(&acct, settings, hooks)?;
            }
            let record = AccountRecord {
                user: user.clone(),
                host: host.clone(),
                port: *port,
                mailboxes: acct.mailboxes.clone(),
                enabled: !disabled,
                credentials: cipher.encrypt(user, &acct.password),
                ..Default::default()
            };
            let raced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = raced.clone();
            let user2 = user.clone();
            repo.modify(
                move |doc: &mut AccountsDoc| {
                    if doc.get(&user2).is_some() {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    doc.records.push(record.clone());
                },
                3,
                log,
            )?;
            if raced.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AccountsError(format!("account '{user}' was added by someone else meanwhile")).into());
            }
            let _ = writeln!(hooks.out, "added {user}{}", if *disabled { " (disabled)" } else { "" });
            Ok(0)
        }
        Command::AccountsUpdate { user, host, port, mailboxes, no_verify } => {
            let doc = repo.read()?;
            let current = doc.require(user)?.clone();
            let mut names: Vec<&str> = Vec::new();
            if host.is_some() {
                names.push("host");
            }
            if port.is_some() {
                names.push("port");
            }
            if !mailboxes.is_empty() {
                names.push("mailboxes");
            }
            if names.is_empty() {
                return Err(AccountsError("nothing to change: pass --host, --port or --mailbox".into()).into());
            }
            if !no_verify {
                let acct = current.to_account(cipher)?;
                let merged = Account {
                    user: acct.user.clone(),
                    password: acct.password,
                    host: host.clone().unwrap_or(acct.host.clone()),
                    port: port.unwrap_or(acct.port),
                    mailboxes: if mailboxes.is_empty() { acct.mailboxes.clone() } else { mailboxes_from_flags(mailboxes) },
                };
                verify(&merged, settings, hooks)?;
            }
            let user2 = user.clone();
            let host2 = host.clone();
            let port2 = *port;
            let mailboxes2 = if mailboxes.is_empty() { None } else { Some(mailboxes_from_flags(mailboxes)) };
            let missing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = missing.clone();
            repo.modify(
                move |doc: &mut AccountsDoc| {
                    let Ok(rec) = doc.require_mut(&user2) else {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    };
                    if let Some(h) = &host2 {
                        rec.host = h.clone();
                    }
                    if let Some(p) = port2 {
                        rec.port = p;
                    }
                    if let Some(m) = &mailboxes2 {
                        rec.mailboxes = m.clone();
                    }
                    rec.updated_at = crate::accounts::now_iso();
                },
                3,
                log,
            )?;
            if missing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AccountsError(format!("no account '{user}'")).into());
            }
            let names: Vec<&str> = names;
            let _ = writeln!(hooks.out, "updated {user}: {}", names.join(", "));
            Ok(0)
        }
        Command::AccountsSetPassword { user, password_stdin, no_verify } => {
            let doc = repo.read()?;
            let current = doc.require(user)?.clone();
            let password = read_password(*password_stdin, *no_verify, hooks)?;
            if !no_verify {
                verify(
                    &Account {
                        user: current.user.clone(),
                        password: password.clone(),
                        host: current.host.clone(),
                        port: current.port,
                        mailboxes: current.mailboxes.clone(),
                    },
                    settings,
                    hooks,
                )?;
            }
            let token = cipher.encrypt(user, &password);
            let user2 = user.clone();
            let missing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = missing.clone();
            repo.modify(
                move |doc: &mut AccountsDoc| {
                    let Ok(rec) = doc.require_mut(&user2) else {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    };
                    rec.credentials = token.clone();
                    rec.updated_at = crate::accounts::now_iso();
                },
                3,
                log,
            )?;
            if missing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AccountsError(format!("no account '{user}'")).into());
            }
            let _ = writeln!(hooks.out, "password updated for {user}");
            Ok(0)
        }
        Command::AccountsEnable { user } => set_enabled(repo, user, true, hooks, log),
        Command::AccountsDisable { user } => set_enabled(repo, user, false, hooks, log),
        Command::AccountsRemove { user } => {
            let user2 = user.clone();
            let missing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = missing.clone();
            repo.modify(
                move |doc: &mut AccountsDoc| {
                    let before = doc.records.len();
                    doc.records.retain(|r| r.user != user2);
                    if doc.records.len() == before {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                },
                3,
                log,
            )?;
            if missing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AccountsError(format!("no account '{user}'")).into());
            }
            let _ = writeln!(
                hooks.out,
                "removed {user} (its stored mail stays in the bucket, its rows in Postgres)"
            );
            Ok(0)
        }
        Command::AccountsRotateKey => {
            let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = broken.clone();
            let doc = repo.modify(
                move |doc: &mut AccountsDoc| {
                    for rec in &mut doc.records {
                        match cipher.rotate(&rec.credentials) {
                            Ok(token) => rec.credentials = token,
                            Err(_) => flag.store(true, std::sync::atomic::Ordering::SeqCst),
                        }
                    }
                },
                3,
                log,
            )?;
            if broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::crypto::CryptoError("could not rotate every password".into()).into());
            }
            for r in &doc.records {
                cipher.decrypt(&r.user, &r.credentials)?; // still bound to the right user
            }
            let _ = writeln!(hooks.out, "re-encrypted {} account(s) with the primary key", doc.records.len());
            Ok(0)
        }
        _ => unreachable!("accounts_action got a non-accounts command"),
    }
}

fn set_enabled<C: S3Client>(
    repo: &AccountsRepo<C>,
    user: &str,
    enabled: bool,
    hooks: &mut Hooks<C>,
    log: &Logger,
) -> Result<i32, CliError> {
    let user2 = user.to_string();
    let missing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = missing.clone();
    repo.modify(
        move |doc: &mut AccountsDoc| {
            let Ok(rec) = doc.require_mut(&user2) else {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            };
            rec.enabled = enabled;
            rec.updated_at = crate::accounts::now_iso();
        },
        3,
        log,
    )?;
    if missing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(AccountsError(format!("no account '{user}'")).into());
    }
    let _ = writeln!(hooks.out, "{}d {user}", if enabled { "enable" } else { "disable" });
    Ok(0)
}

fn mailboxes_from_flags(flags: &[String]) -> Vec<String> {
    let items: Vec<String> = flags.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if items.is_empty() {
        vec!["INBOX".to_string()]
    } else {
        items
    }
}

/// Read one line from a `dyn Read` (`read_line` lives on BufRead).
fn read_line_from(input: &mut dyn Read) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte)? {
            0 => break,
            _ => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// `read_password`: stdin with --password-stdin, else a (double) prompt on a
/// terminal, else the Python error.
fn read_password<C: S3Client>(from_stdin: bool, confirm: bool, hooks: &mut Hooks<C>) -> Result<String, CliError> {
    let raw = if from_stdin {
        let line = read_line_from(hooks.stdin).map_err(|e| AccountsError(e.to_string()))?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else if hooks.stdin_tty {
        let _ = writeln!(hooks.err, "IMAP password: ");
        let _ = hooks.err.flush();
        let line = read_line_from(hooks.stdin).map_err(|e| AccountsError(e.to_string()))?;
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if confirm {
            let _ = writeln!(hooks.err, "Repeat password: ");
            let _ = hooks.err.flush();
            let repeat = read_line_from(hooks.stdin).map_err(|e| AccountsError(e.to_string()))?;
            let repeat = repeat.trim_end_matches(['\r', '\n']).to_string();
            if repeat != line {
                return Err(AccountsError("passwords do not match".into()).into());
            }
        }
        line
    } else {
        return Err(AccountsError("no terminal to prompt for the password; pipe it with --password-stdin".into()).into());
    };
    let pw = clean_password(&raw);
    if pw.is_empty() {
        Err(AccountsError("empty password".into()).into())
    } else {
        Ok(pw)
    }
}

fn verify<C: S3Client>(acct: &Account, settings: &Settings, hooks: &mut Hooks<C>) -> Result<(), CliError> {
    let _ = writeln!(hooks.err, "logging in to {}:{} as {} ...", acct.host, acct.port, acct.user);
    let _ = hooks.err.flush();
    match (hooks.verify_login)(acct, settings.imap_timeout) {
        Ok(()) => Ok(()),
        Err(e) => Err(AccountsError(format!("login check failed ({e}); fix it or pass --no-verify")).into()),
    }
}
