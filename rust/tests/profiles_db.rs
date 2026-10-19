//! Port of the profiles behaviour exercised by `tests/test_cli_db.py` and
//! `tests/test_classifier.py`, against a real Postgres.

mod support;

use aimap::jev::Pattern;
use aimap::profiles::{
    account_profiles, assign, get, list_all, parse_patterns, patterns, put, replace_patterns,
};
use support::TestDb;

fn pattern(insight: &str, importance: &str, action_bucket: &str, tags: &[&str]) -> Pattern {
    Pattern {
        insight: insight.to_string(),
        importance: importance.to_string(),
        action_bucket: action_bucket.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        examples: Vec::new(),
    }
}

#[test]
fn put_get_and_assignment() {
    let db = TestDb::new();
    let mut conn = db.connect();

    // default profile exists from the migration seed? No: created by callers.
    assert!(get(&mut conn, "work").is_err());

    put(&mut conn, "work", &serde_json::json!({"who": "Alex at the studio."})).unwrap();
    let p = get(&mut conn, "work").unwrap();
    assert_eq!(p.name, "work");
    assert_eq!(p.profile, serde_json::json!({"who": "Alex at the studio."}));

    // put is an upsert
    put(&mut conn, "work", &serde_json::json!({"who": "Alex, now elsewhere."})).unwrap();
    assert_eq!(get(&mut conn, "work").unwrap().profile, serde_json::json!({"who": "Alex, now elsewhere."}));

    // a profile must be a JSON object
    assert_eq!(
        put(&mut conn, "x", &serde_json::json!(["nope"])).unwrap_err().to_string(),
        "a profile must be a JSON object"
    );
    assert_eq!(put(&mut conn, "x", &serde_json::json!(5)).unwrap_err().to_string(), "a profile must be a JSON object");

    // error message matches Python: no profile 'nope'
    assert_eq!(get(&mut conn, "nope").unwrap_err().to_string(), "no profile 'nope'");
    let err = assign(&mut conn, "me@x.com", "nope").unwrap_err().to_string();
    assert_eq!(err, "no profile 'nope'");

    assign(&mut conn, "me@x.com", "work").unwrap();
    assert_eq!(account_profiles(&mut conn).unwrap(), vec![("me@x.com".to_string(), "work".to_string())]);
    assign(&mut conn, "me@x.com", "work").unwrap(); // idempotent
    assert_eq!(account_profiles(&mut conn).unwrap().len(), 1);

    // list_all: (name, accounts, patterns)
    put(&mut conn, "default", &serde_json::json!({})).unwrap();
    let rows = list_all(&mut conn).unwrap();
    assert_eq!(
        rows,
        vec![
            ("default".to_string(), 0, 0),
            ("work".to_string(), 1, 0),
        ]
    );
}

#[test]
fn patterns_round_trip_and_replacement() {
    let db = TestDb::new();
    let mut conn = db.connect();
    put(&mut conn, "default", &serde_json::json!({"who": "Alex"})).unwrap();
    let pid = get(&mut conn, "default").unwrap().id;

    let items = vec![
        pattern("Client invoices", "High", "act_now", &["work", "billing"]),
        pattern("Shop promos", "Low", "discard", &["promo"]),
    ];
    replace_patterns(&mut conn, pid, &items).unwrap();
    // stored ordered by insight
    let got = patterns(&mut conn, pid).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].insight, "Client invoices");
    assert_eq!(got[1].insight, "Shop promos");
    assert_eq!(got[0].tags, vec!["work".to_string(), "billing".to_string()]);

    // replace_patterns replaces every pattern
    replace_patterns(&mut conn, pid, &[pattern("Only this", "Medium", "review", &[])]).unwrap();
    let got = patterns(&mut conn, pid).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].insight, "Only this");

    // counts in list_all reflect the patterns
    put(&mut conn, "other", &serde_json::json!({})).unwrap();
    let rows = list_all(&mut conn).unwrap();
    assert_eq!(rows[0], ("default".to_string(), 0, 1));
    assert_eq!(rows[1], ("other".to_string(), 0, 0));

    // validation: bad importance
    let err = replace_patterns(&mut conn, pid, &[pattern("x", "Huge", "skim", &[])]).unwrap_err().to_string();
    assert!(err.contains("importance must be"), "{err}");
    // validation: bad action bucket
    let err = replace_patterns(&mut conn, pid, &[pattern("x", "High", "nonsense", &[])]).unwrap_err().to_string();
    assert!(err.contains("action_bucket must be"), "{err}");
    // validation: reserved insight
    let err =
        replace_patterns(&mut conn, pid, &[pattern("none_of_these", "High", "review", &[])]).unwrap_err().to_string();
    assert!(err.contains("reserved"), "{err}");
    // duplicates rejected, patterns untouched
    let err = replace_patterns(&mut conn, pid, &[
        pattern("dup", "High", "review", &[]),
        pattern("dup", "Low", "discard", &[]),
    ])
    .unwrap_err()
    .to_string();
    assert!(err.contains("duplicate pattern"), "{err}");
    assert_eq!(patterns(&mut conn, pid).unwrap().len(), 1);
}

#[test]
fn parse_patterns_from_json() {
    let items = parse_patterns(
        r#"[{"insight": "Client invoices", "importance": "High", "action_bucket": "act_now",
             "tags": ["work", "billing"], "examples": [{"from": "Client <c@x>", "subject": "Invoice 12"}]},
            {"insight": "Shop promos", "importance": "Low", "action_bucket": "discard"}]"#,
        "json",
    )
    .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].insight, "Client invoices");
    assert_eq!(items[0].tags, vec!["work".to_string(), "billing".to_string()]);
    assert_eq!(items[0].examples.len(), 1);
    assert_eq!(items[1].tags.len(), 0);
    assert_eq!(items[1].examples.len(), 0);

    // must be a list
    assert_eq!(
        parse_patterns(r#"{"insight": "x"}"#, "json").unwrap_err().to_string(),
        "patterns JSON must be a list"
    );
    // missing keys
    let err = parse_patterns(r#"[{"insight": "x"}]"#, "json").unwrap_err().to_string();
    assert!(err.contains("invalid pattern"), "{err}");
    // parse_patterns output validates and stores
    let db = TestDb::new();
    let mut conn = db.connect();
    put(&mut conn, "default", &serde_json::json!({})).unwrap();
    let pid = get(&mut conn, "default").unwrap().id;
    replace_patterns(&mut conn, pid, &items).unwrap();
    assert_eq!(patterns(&mut conn, pid).unwrap().len(), 2);
    // invalid importance caught by replace_patterns
    let bad = parse_patterns(r#"[{"insight": "x", "importance": "Huge", "action_bucket": "skim"}]"#, "json").unwrap();
    let err = replace_patterns(&mut conn, pid, &bad).unwrap_err().to_string();
    assert!(err.contains("importance must be"), "{err}");
}

#[test]
fn parse_patterns_from_csv() {
    let csv = "insight,importance,action_bucket,tags,example_from,example_subject\n\
               Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 12\n\
               Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 13\n\
               Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 14\n\
               Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 15\n\
               Shop promos,Low,discard,promo,,\n";
    let items = parse_patterns(csv, "csv").unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].insight, "Client invoices");
    assert_eq!(items[0].importance, "High");
    assert_eq!(items[0].action_bucket, "act_now");
    assert_eq!(items[0].tags, vec!["work".to_string(), "billing".to_string()]);
    // examples capped at three, in file order
    assert_eq!(items[0].examples.len(), 3);
    assert_eq!(items[0].examples[0], serde_json::json!({"from": "Client <c@x>", "subject": "Invoice 12"}));
    assert_eq!(items[0].examples[2]["subject"], "Invoice 14");
    // second row without examples
    assert_eq!(items[1].insight, "Shop promos");
    assert_eq!(items[1].examples.len(), 0);

    // missing required columns
    let err = parse_patterns("foo,bar\n1,2\n", "csv").unwrap_err().to_string();
    assert!(err.contains("CSV needs insight"), "{err}");
    // short rows count as missing columns too
    let err = parse_patterns("insight,importance,action_bucket\nok\n", "csv").unwrap_err().to_string();
    assert!(err.contains("CSV needs insight"), "{err}");
    // quoted fields with commas survive
    let items = parse_patterns(
        "insight,importance,action_bucket\n\"Invoices, unpaid\",High,act_now\n",
        "csv",
    )
    .unwrap();
    assert_eq!(items[0].insight, "Invoices, unpaid");
}

#[test]
fn unknown_format() {
    assert_eq!(
        parse_patterns("", "yaml").unwrap_err().to_string(),
        "unknown patterns format 'yaml'"
    );
}
