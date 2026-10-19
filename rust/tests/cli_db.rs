//! Ports of `tests/test_cli.py` and `tests/test_cli_db.py`: the accounts
//! commands against the in-memory S3 fake, the DB admin commands against a
//! real Postgres, and the service commands' early exits — all through
//! `cli::main_with` with injected stdio, S3 and IMAP login check.

mod support;

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;

use aimap::accounts::Account;
use aimap::cli::{self, Hooks};
use aimap::config::S3Config;
use aimap::crypto::{Cipher, generate_key};
use aimap::storage::Store;
use support::{MemoryS3, TestDb};

const BUCKET: &str = "aimap-test";

/// The per-test world: env vars, a shared S3 fake, the recorded logins and
/// the captured stdout/stderr.
struct World {
    env: HashMap<String, String>,
    mem: MemoryS3,
    logins: Mutex<Vec<(String, String)>>,
    /// fail the IMAP login check with this message
    login_error: Mutex<Option<String>>,
    db: Option<TestDb>,
}

impl World {
    fn new() -> World {
        let mut env = HashMap::new();
        env.insert("S3_BUCKET".to_string(), BUCKET.to_string());
        env.insert("AWS_DEFAULT_REGION".to_string(), "us-east-1".to_string());
        env.insert("AIMAP_SECRET_KEY".to_string(), generate_key());
        World { env, mem: MemoryS3::new(), logins: Mutex::new(Vec::new()), login_error: Mutex::new(None), db: None }
    }

    fn with_db(mut self) -> World {
        let db = TestDb::new();
        self.env.insert("DATABASE_URL".to_string(), db.url());
        self.db = Some(db);
        self
    }

    fn remove(&mut self, key: &str) {
        self.env.remove(key);
    }

    fn set(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    /// Run `aimap <argv>`; returns (exit code, stdout, stderr).
    fn run(&self, argv: &[&str], stdin: &str) -> (i32, String, String) {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mem = self.mem.clone();
        let s3_factory = move |_cfg: &S3Config| mem.clone();
        let logins = &self.logins;
        let login_error = &self.login_error;
        let verify_login = move |acct: &Account, _timeout: f64| {
            if let Some(e) = login_error.lock().unwrap().clone() {
                return Err(e);
            }
            logins.lock().unwrap().push((acct.user.clone(), acct.password.clone()));
            Ok(())
        };
        let mut stdin_bytes = Cursor::new(stdin.to_string());
        let mut hooks = Hooks {
            verify_login: &verify_login,
            s3: &s3_factory,
            stdin: &mut stdin_bytes,
            stdin_tty: false,
            out: &mut out,
            err: &mut err,
        };
        let code = cli::main_with(&self.env, &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &mut hooks);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// The accounts file as stored in the fake S3 (parsed).
    fn stored(&self) -> serde_json::Value {
        let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, self.mem.clone());
        let (body, _) = store.get_bytes("config/accounts.json").unwrap().unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        doc["accounts"].clone()
    }

    fn conn(&self) -> postgres::Client {
        self.db.as_ref().expect("world needs a database").connect()
    }
}

fn account_args(user: &str, password: &str) -> Vec<String> {
    vec![
        "accounts".to_string(),
        "add".to_string(),
        user.to_string(),
        "--host".to_string(),
        "imap.x.com".to_string(),
        "--mailbox".to_string(),
        "INBOX".to_string(),
        "--mailbox".to_string(),
        "Sent".to_string(),
        "--password-stdin".to_string(),
        format!("\n{password}"),
    ]
}

// --------------------------------------------------------------------------------------
// accounts (test_cli.py)
// --------------------------------------------------------------------------------------

#[test]
fn add_list_update_disable_remove() {
    let w = World::new();

    let argv: Vec<String> = vec![
        "accounts".into(), "add".into(), "me@x.com".into(), "--host".into(), "imap.x.com".into(),
        "--mailbox".into(), "INBOX".into(), "--mailbox".into(), "Sent".into(),
        "--password-stdin".into(),
    ];
    let (code, out, err) = w.run(&argv.iter().map(String::as_str).collect::<Vec<_>>(), "abcd efgh\n");
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(out, "added me@x.com\n");
    // verified before saving, spaces stripped
    let logins = w.logins.lock().unwrap().clone();
    assert_eq!(logins, vec![("me@x.com".to_string(), "abcdefgh".to_string())]);
    let stored = w.stored();
    let records = stored.as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["mailboxes"], serde_json::json!(["INBOX", "Sent"]));
    assert!(!serde_json::to_string(&records).unwrap().contains("abcdefgh"));

    // adding it again fails
    let argv: Vec<String> = vec![
        "accounts".into(), "add".into(), "me@x.com".into(), "--host".into(), "h".into(),
        "--password-stdin".into(),
    ];
    let (code, out, err) = w.run(&argv.iter().map(String::as_str).collect::<Vec<_>>(), "p\n");
    assert_eq!(code, 1);
    assert!(err.contains("already exists"), "{err}");
    assert!(out.is_empty());

    let (code, _, _) = w.run(&["accounts", "disable", "me@x.com"], "");
    assert_eq!(code, 0);
    assert_eq!(w.stored()[0]["enabled"], serde_json::json!(false));

    let (code, _, _) = w.run(&["accounts", "update", "me@x.com", "--port", "1993"], "");
    assert_eq!(code, 0);
    assert_eq!(w.stored()[0]["port"], serde_json::json!(1993));

    let (code, out, _) = w.run(&["accounts", "list"], "");
    assert_eq!(code, 0);
    assert!(out.contains("me@x.com") && !out.contains("abcdefgh") && out.contains(" ok "), "{out}");

    let (code, _, _) = w.run(&["accounts", "remove", "me@x.com"], "");
    assert_eq!(code, 0);
    assert_eq!(w.stored().as_array().unwrap().len(), 0);
}

#[test]
fn failed_login_does_not_save() {
    let w = World::new();
    *w.login_error.lock().unwrap() = Some("bad credentials".to_string());
    let (code, _, err) = w.run(&["accounts", "add", "a", "--host", "h", "--password-stdin"], "p\n");
    assert_eq!(code, 1);
    assert!(err.contains("login check failed"), "{err}");
    let (code, out, _) = w.run(&["accounts", "list"], "");
    assert_eq!(code, 0);
    assert!(out.contains("no accounts"), "{out}");
}

#[test]
fn set_password_and_rotate_key() {
    let mut w = World::new();
    let (code, _, _) = w.run(&["accounts", "add", "a", "--host", "h", "--no-verify", "--password-stdin"], "old\n");
    assert_eq!(code, 0);
    let (code, _, _) = w.run(&["accounts", "set-password", "a", "--password-stdin"], "new\n");
    assert_eq!(code, 0);
    let last = w.logins.lock().unwrap().last().unwrap().clone();
    assert_eq!(last, ("a".to_string(), "new".to_string()));

    let old = w.env["AIMAP_SECRET_KEY"].clone();
    let new = generate_key();
    w.set("AIMAP_SECRET_KEY", &format!("{new},{old}"));
    let (code, _, _) = w.run(&["accounts", "rotate-key"], "");
    assert_eq!(code, 0);
    let token = w.stored()[0]["credentials"].as_str().unwrap().to_string();
    assert_eq!(Cipher::new(&new).unwrap().decrypt("a", &token).unwrap(), "new");
}

#[test]
fn password_prompt_needs_tty_or_stdin_flag() {
    let w = World::new();
    let (code, _, err) = w.run(&["accounts", "add", "a", "--host", "h"], "");
    assert_eq!(code, 1);
    assert!(err.contains("--password-stdin"), "{err}");
}

#[test]
fn keygen_prints_a_valid_key_and_ignores_a_leading_aimap() {
    let w = World::new();
    let (code, out, _) = w.run(&["keygen"], "");
    assert_eq!(code, 0);
    Cipher::new(out.trim()).expect("a valid key");

    let (code, out, _) = w.run(&["aimap", "keygen"], "");
    assert_eq!(code, 0);
    Cipher::new(out.trim()).expect("a valid key");
}

#[test]
fn parse_errors_exit_2() {
    let w = World::new();
    let (code, _, err) = w.run(&["frobnicate"], "");
    assert_eq!(code, 2);
    assert!(err.contains("unknown command"), "{err}");
    let (code, _, _) = w.run(&["jobs"], "");
    assert_eq!(code, 2);
    let (code, _, _) = w.run(&["profiles", "set", "name"], "");
    assert_eq!(code, 2); // --file is required
}

// --------------------------------------------------------------------------------------
// DB commands (test_cli_db.py)
// --------------------------------------------------------------------------------------

#[test]
fn migrate_needs_no_secret_key() {
    let mut w = World::new().with_db();
    w.remove("AIMAP_SECRET_KEY");
    let (code, out, err) = w.run(&["migrate"], "");
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("applied 0 migration(s)"), "{out}");
}

#[test]
fn database_url_is_required() {
    let mut w = World::new().with_db();
    w.remove("DATABASE_URL");
    let (code, _, _) = w.run(&["jobs", "status"], "");
    assert_eq!(code, 2);
}

#[test]
fn profiles_patterns_and_account_assignment() {
    let w = World::new().with_db();
    let dir = std::env::temp_dir();
    let work = dir.join(format!("aimap-cli-work-{}.json", std::process::id()));
    std::fs::write(&work, serde_json::json!({"who": "Alex at the studio."}).to_string()).unwrap();
    let csv = dir.join(format!("aimap-cli-p-{}.csv", std::process::id()));
    std::fs::write(
        &csv,
        "insight,importance,action_bucket,tags,example_from,example_subject\n\
         Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 12\n\
         Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 13\n\
         Shop promos,Low,discard,promo,,\n",
    )
    .unwrap();
    let bad = dir.join(format!("aimap-cli-bad-{}.json", std::process::id()));
    std::fs::write(
        &bad,
        serde_json::json!([{"insight": "x", "importance": "Huge", "action_bucket": "skim"}]).to_string(),
    )
    .unwrap();
    let work_s = work.display().to_string();
    let csv_s = csv.display().to_string();
    let bad_s = bad.display().to_string();

    let (code, out, err) = w.run(&["profiles", "set", "work", "--file", &work_s], "");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "saved profile work\n");
    let (code, out, _) = w.run(&["profiles", "show", "work"], "");
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"who": "Alex at the studio."}));

    let (code, out, err) = w.run(&["patterns", "import", "work", &csv_s], "");
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("imported 2 pattern(s)"), "{out}");
    let (code, out, _) = w.run(&["patterns", "list", "work"], "");
    assert_eq!(code, 0);
    assert!(out.contains("act_now") && out.contains("work; billing"), "{out}");

    let (code, _, err) = w.run(&["patterns", "import", "work", &bad_s], "");
    assert_eq!(code, 1);
    assert!(err.contains("importance must be"), "{err}");

    let (code, out, _) = w.run(&["accounts", "set-profile", "me@x.com", "work"], "");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "me@x.com now uses profile work\n");
    let (code, _, err) = w.run(&["accounts", "set-profile", "me@x.com", "nope"], "");
    assert_eq!(code, 1);
    assert!(err.contains("nope"), "{err}");
    let (code, out, _) = w.run(&["profiles", "list"], "");
    assert_eq!(code, 0);
    assert!(out.contains("now uses profile work") || out.lines().last().unwrap().contains("work"), "{out}");

    let _ = (work, csv, bad); // temp files: leave for the OS to clean
}

#[test]
fn backfill_then_jobs() {
    let w = World::new().with_db();
    w.mem.put_plain(BUCKET, "raw/me@x.com/INBOX/1/1.eml", &email_bytes(1));

    let (code, _, err) = w.run(&["backfill"], "");
    assert_eq!(code, 0, "{err}");
    let (code, out, _) = w.run(&["jobs", "status"], "");
    assert_eq!(code, 0);
    assert!(out.contains("classify   pending  1"), "{out}");
    let (code, out, _) = w.run(&["jobs", "retry"], "");
    assert_eq!(code, 0);
    assert!(out.contains("requeued 0 job(s)"), "{out}"); // only failed ones without --done
    let mut conn = w.conn();
    conn.execute("UPDATE jobs SET status = 'done'", &[]).unwrap();
    let (code, out, _) = w.run(&["jobs", "retry", "--done"], "");
    assert_eq!(code, 0);
    assert!(out.contains("requeued 1 job(s)"), "{out}");
}

#[test]
fn classify_needs_api_key() {
    let w = World::new().with_db();
    let (code, _, _) = w.run(&["classify", "--once"], "");
    assert_eq!(code, 2);
}

#[test]
fn api_refuses_to_start_without_firebase_settings() {
    let mut w = World::new().with_db();
    for key in ["FIREBASE_PROJECT_ID", "AIMAP_ALLOWED_EMAILS"] {
        w.remove(key);
    }
    let (code, _, _) = w.run(&["migrate"], "");
    assert_eq!(code, 0);
    let (code, _, _) = w.run(&["api"], "");
    assert_eq!(code, 2);

    w.set("FIREBASE_PROJECT_ID", "proj");
    let (code, _, _) = w.run(&["api"], "");
    assert_eq!(code, 2);
}

fn email_bytes(n: u32) -> Vec<u8> {
    format!(
        "From: Shop <deals@shop.example>\r\nTo: me@example.com\r\nSubject: m{n}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m{n}@example.com>\r\n\
         \r\nbody {n}\r\n"
    )
    .into_bytes()
}

// silence the unused helper when only DB-less tests are filtered in
#[allow(unused)]
fn _account_args_typecheck(_: Vec<String>) -> Vec<String> {
    account_args("a", "b")
}
