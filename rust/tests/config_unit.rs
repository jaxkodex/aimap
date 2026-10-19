//! Port of the config parts of `tests/test_config.py`.

use std::collections::HashMap;

use aimap::config::{load_settings, ConfigError};
use aimap::crypto::generate_key;

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn err(pairs: &[(&str, &str)]) -> String {
    load_settings(&env(pairs), true).unwrap_err().0
}

#[test]
fn source_defaults_to_bucket_and_needs_key() {
    assert!(err(&[("S3_BUCKET", "b")]).contains("AIMAP_SECRET_KEY"));
    let key = generate_key();
    let s = load_settings(&env(&[("S3_BUCKET", "b"), ("AIMAP_SECRET_KEY", &key)]), true).unwrap();
    assert_eq!(s.accounts_source, "bucket");
    assert_eq!(s.accounts_path, "config/accounts.json");
    assert!(!format!("{s:?}").contains(&key));
}

#[test]
fn source_defaults_to_env_when_imap_user_set() {
    let s = load_settings(&env(&[("S3_BUCKET", "b"), ("IMAP_USER", "a")]), true).unwrap();
    assert_eq!(s.accounts_source, "env");
    assert_eq!(s.secret_key, None);
}

#[test]
fn prefix_normalised() {
    let s = load_settings(
        &env(&[("S3_BUCKET", "b"), ("AIMAP_SECRET_KEY", &generate_key()), ("S3_PREFIX", "/mail/")]),
        true,
    )
    .unwrap();
    assert_eq!(s.s3.prefix, "mail/");
}

#[test]
fn invalid_settings() {
    assert!(err(&[]).contains("S3_BUCKET"));
    assert!(err(&[("S3_BUCKET", "b"), ("ACCOUNTS_SOURCE", "db")]).contains("ACCOUNTS_SOURCE"));
    assert!(err(&[
        ("S3_BUCKET", "b"),
        ("AIMAP_SECRET_KEY", &generate_key()),
        ("POLL_INTERVAL_SECONDS", "x"),
    ])
    .contains("POLL_INTERVAL"));
}

#[test]
fn api_settings() {
    let s = load_settings(
        &env(&[
            ("S3_BUCKET", "b"),
            ("FIREBASE_PROJECT_ID", "proj"),
            ("PORT", "9000"),
            ("AIMAP_ALLOWED_EMAILS", " Me@Example.com, ,you@example.com "),
        ]),
        false,
    )
    .unwrap();
    assert_eq!(s.api.firebase_project_id.as_deref(), Some("proj"));
    assert_eq!(s.api.port, 9000);
    let want: std::collections::BTreeSet<String> =
        ["me@example.com", "you@example.com"].iter().map(|s| s.to_string()).collect();
    assert_eq!(s.api.allowed_emails, want);
    let s = load_settings(&env(&[("S3_BUCKET", "b")]), false).unwrap();
    assert!(s.api.allowed_emails.is_empty());
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("API_POOL_SIZE", "0")]), false)
        .unwrap_err()
        .0
        .contains("API_POOL_SIZE"));
}

#[test]
fn region_falls_back_to_aws_variables() {
    let s = load_settings(&env(&[("S3_BUCKET", "b"), ("AWS_DEFAULT_REGION", "eu-west-1")]), false).unwrap();
    assert_eq!(s.s3.region.as_deref(), Some("eu-west-1"));
    let s = load_settings(
        &env(&[("S3_BUCKET", "b"), ("AWS_REGION", "us-east-2"), ("AWS_DEFAULT_REGION", "eu-west-1")]),
        false,
    )
    .unwrap();
    assert_eq!(s.s3.region.as_deref(), Some("us-east-2"));
}

#[test]
fn numeric_ranges_are_validated() {
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("POLL_INTERVAL_SECONDS", "0")]), false)
        .unwrap_err()
        .0
        .contains("POLL_INTERVAL_SECONDS"));
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("BATCH_SIZE", "0")]), false)
        .unwrap_err()
        .0
        .contains("BATCH_SIZE"));
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("INITIAL_FETCH_COUNT", "-1")]), false)
        .unwrap_err()
        .0
        .contains("INITIAL_FETCH_COUNT"));
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("CLASSIFY_CONCURRENCY", "0")]), false)
        .unwrap_err()
        .0
        .contains("CLASSIFY_CONCURRENCY"));
    assert!(load_settings(&env(&[("S3_BUCKET", "b"), ("CLASSIFY_MAX_ATTEMPTS", "0")]), false)
        .unwrap_err()
        .0
        .contains("CLASSIFY_MAX_ATTEMPTS"));
}

#[test]
fn classifier_and_draft_defaults_and_overrides() {
    let s = load_settings(&env(&[("S3_BUCKET", "b")]), false).unwrap();
    assert_eq!(s.classifier.model, "jev-1.13.0");
    assert_eq!(s.classifier.thresholds.pattern_confidence, 0.5);
    assert_eq!(s.classifier.body_chars, 1500);
    assert_eq!(s.api.draft.base_url, "https://api.deepseek.com");
    assert_eq!(s.api.draft.model, "deepseek-flash");

    let s = load_settings(
        &env(&[
            ("S3_BUCKET", "b"),
            ("TYPESAFE_API_KEY", "k2"),
            ("JEV_MODEL", "jev-2.0"),
            ("JEV_TAG_THRESHOLD", "0.9"),
            ("AIMAP_DRAFT_BASE_URL", "http://localhost:1234"),
            ("AIMAP_DRAFT_API_KEY", "dk"),
            ("AIMAP_DRAFT_EFFORT", "high"),
        ]),
        false,
    )
    .unwrap();
    assert_eq!(s.classifier.api_key.as_deref(), Some("k2"));
    assert_eq!(s.classifier.model, "jev-2.0");
    assert_eq!(s.classifier.thresholds.tag_threshold, 0.9);
    assert_eq!(s.api.draft.base_url, "http://localhost:1234");
    assert_eq!(s.api.draft.api_key.as_deref(), Some("dk"));
    assert_eq!(s.api.draft.reasoning_effort, "high");
    // JEV_API_KEY wins over TYPESAFE_API_KEY
    let s = load_settings(
        &env(&[("S3_BUCKET", "b"), ("JEV_API_KEY", "k1"), ("TYPESAFE_API_KEY", "k2")]),
        false,
    )
    .unwrap();
    assert_eq!(s.classifier.api_key.as_deref(), Some("k1"));
}

#[test]
fn config_error_displays_its_message() {
    let e = ConfigError("boom".to_string());
    assert_eq!(e.to_string(), "boom");
}
