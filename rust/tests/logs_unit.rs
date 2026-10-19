//! Port of the logging behavior `logs.py` pins: the JSON and text record
//! shapes, level filtering, and the capture sink used by other suites
//! (Python's `caplog`).

use aimap::logs::{format_json, format_text, Capture, Level, LogValue, Logger};
use serde_json::Value;

fn json_of(level: Level, message: &str, extra: &[(&str, LogValue)]) -> Value {
    serde_json::from_str(&format_json(level, "aimap.worker", message, extra)).unwrap()
}

#[test]
fn json_records_carry_time_level_logger_message_then_extras() {
    let v = json_of(
        Level::Info,
        "account added",
        &[("account", LogValue::str("me@x.com"))],
    );
    let obj = v.as_object().unwrap();
    let keys: Vec<&String> = obj.keys().collect();
    assert_eq!(keys, ["time", "level", "logger", "message", "account"]);
    assert_eq!(obj["level"], "info");
    assert_eq!(obj["logger"], "aimap.worker");
    assert_eq!(obj["message"], "account added");
    assert_eq!(obj["account"], "me@x.com");
    // datetime.fromtimestamp(..., UTC).isoformat(timespec="milliseconds")
    let time = obj["time"].as_str().unwrap();
    assert!(
        time.len() >= 24 && time.ends_with("+00:00") && time.contains('.'),
        "unexpected time format: {time}"
    );
}

#[test]
fn json_extras_cover_numbers_floats_bools_and_lists() {
    let v = json_of(
        Level::Error,
        "account sync failed",
        &[
            ("failures", LogValue::Int(2)),
            ("poll_interval_s", LogValue::Float(60.0)),
            ("stopped", LogValue::Bool(true)),
            ("failed_accounts", LogValue::List(vec!["bad".into()])),
        ],
    );
    let obj = v.as_object().unwrap();
    assert_eq!(obj["failures"], 2);
    assert_eq!(obj["poll_interval_s"], 60.0);
    assert_eq!(obj["stopped"], true);
    assert_eq!(obj["failed_accounts"], serde_json::json!(["bad"]));
}

#[test]
fn text_records_show_level_message_and_extras() {
    let line = format_text(
        Level::Warning,
        "no accounts configured; waiting (add one with `aimap accounts add`)",
        &[("account", LogValue::str("a@x"))],
    );
    assert!(line.contains("WARNING no accounts configured; waiting (add one with `aimap accounts add`)  account=a@x"), "{line}");
    // time prefix: "YYYY-MM-DD HH:MM:SS,mmm"
    let time: String = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    assert!(time.contains(',') && time.len() >= 21, "{time}");
    // Python str(list)
    let line2 = format_text(Level::Info, "pass complete", &[
        ("failed_accounts", LogValue::List(vec!["a".into(), "b".into()])),
    ]);
    assert!(line2.contains("failed_accounts=['a', 'b']"), "{line2}");
    // Python str(float)
    let line3 = format_text(Level::Info, "worker started", &[("poll_interval_s", LogValue::Float(60.0))]);
    assert!(line3.contains("poll_interval_s=60.0"), "{line3}");
}

#[test]
fn level_names_filter_records_like_the_root_logger() {
    let capture = Capture::new();
    let logger = Logger::new(capture.clone(), "WARNING", "aimap.worker");
    logger.info("kept out", &[]);
    logger.warning("kept", &[]);
    logger.error("also kept", &[]);
    assert_eq!(capture.messages(Level::Info), Vec::<String>::new());
    assert_eq!(capture.messages(Level::Warning), vec!["kept".to_string()]);
    assert_eq!(capture.messages(Level::Error), vec!["also kept".to_string()]);
}

#[test]
fn levels_parse_from_env_strings() {
    let capture = Capture::new();
    let logger = Logger::new(capture.clone(), "error", "aimap");
    logger.warning("no", &[]);
    logger.error("yes", &[]);
    assert_eq!(capture.messages(Level::Error).len(), 1);
    assert_eq!(capture.messages(Level::Warning).len(), 0);
}
