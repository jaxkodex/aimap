//! The `aimap` binary: wires the CLI to the real S3 client, the real IMAP
//! login check and the real stdio (`aimap/__main__.py` in the Python image).

use aimap::cli::{self, Hooks};
use aimap::config::{self, S3Config};
use aimap::s3::{HttpS3Client, S3Settings};
use std::collections::HashMap;

fn s3_settings(cfg: &S3Config, env: &HashMap<String, String>) -> S3Settings {
    let region = cfg
        .region
        .clone()
        .or_else(|| env.get("AWS_REGION").cloned())
        .or_else(|| env.get("AWS_DEFAULT_REGION").cloned())
        .unwrap_or_else(|| "us-east-1".to_string());
    let access_key_id = cfg
        .access_key_id
        .clone()
        .or_else(|| env.get("AWS_ACCESS_KEY_ID").cloned())
        .unwrap_or_default();
    let secret_access_key = cfg
        .secret_access_key
        .clone()
        .or_else(|| env.get("AWS_SECRET_ACCESS_KEY").cloned())
        .unwrap_or_default();
    S3Settings {
        endpoint_url: cfg.endpoint_url.clone(),
        scheme: env.get("S3_SCHEME").cloned().unwrap_or_else(|| "https".to_string()),
        region,
        access_key_id,
        secret_access_key,
        force_path_style: cfg.force_path_style || cfg.endpoint_url.is_some(),
    }
}

fn main() {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    // load_dotenv: variables from .env, existing environment wins
    for (key, value) in config::read_dotenv(std::path::Path::new(".env")) {
        env.entry(key).or_insert(value);
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let verify_login = |acct: &aimap::accounts::Account, timeout: f64| cli::real_verify_login(acct, timeout);
    let s3_factory = |cfg: &S3Config| HttpS3Client::new(cfg.bucket.clone(), s3_settings(cfg, &env));

    let mut hooks = Hooks {
        verify_login: &verify_login,
        s3: &s3_factory,
        stdin: &mut std::io::stdin(),
        stdin_tty: atty(),
        out: &mut std::io::stdout(),
        err: &mut std::io::stderr(),
    };
    let code = cli::main_with(&env, &argv, &mut hooks);
    std::process::exit(code);
}

/// stdin.isatty() (libc::isatty(0)).
fn atty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}
