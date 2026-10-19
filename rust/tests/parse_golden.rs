//! Golden tests: every fixture in `tests/fixtures/*.eml` must parse to exactly
//! the values in its `.expected.json`. The expected files were generated from
//! the Python implementation's behavior (see tools/make-fixtures.mjs), so this
//! pins the Rust port to Python byte-for-byte on the parts that matter.

use aimap::parse::{attachment_names, body_text, derive_thread_id, parse_meta, strip_quoted};
use serde_json::{json, Value};

const FIXTURES: [&str; 6] = [
    "simple",
    "newsletter",
    "reply",
    "attachments",
    "no-message-id",
    "garbage",
];

fn fixture(stem: &str) -> (Vec<u8>, Value) {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(format!("{dir}/{stem}.eml")).unwrap();
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{stem}.expected.json")).unwrap())
            .unwrap();
    (raw, expected)
}

#[test]
fn golden_fixtures() {
    for stem in FIXTURES {
        let (raw, expected) = fixture(stem);
        let meta = parse_meta(&raw);
        let thread_id = derive_thread_id(
            meta.references.as_deref(),
            meta.in_reply_to.as_deref(),
            meta.subject.as_deref(),
            &[],
            Some(&meta.rfc822_message_id),
        );
        let attachments = attachment_names(&raw, 5);
        let body = body_text(&raw, 1500);

        let mut actual = json!({
            "meta": meta,
            "thread_id": thread_id,
            "attachments": attachments,
            "body_text": body,
        });
        if expected.get("body_text_100").is_some() {
            actual["body_text_100"] = json!(body_text(&raw, 100));
        }
        if expected.get("stripped").is_some() {
            actual["stripped"] = json!(strip_quoted(&body));
        }

        assert_eq!(actual, expected, "fixture {stem} diverged from Python");
    }
}
