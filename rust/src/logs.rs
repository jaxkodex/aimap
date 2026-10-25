//! Logging to stdout, JSON by default (Railway parses it), plain text with
//! `LOG_FORMAT=text`. Port of `logs.py`: instead of the std logging module,
//! a `Logger` value is passed to the parts that log (`Worker`, the CLI), and
//! a `Capture` sink stands in for `caplog` in tests.

use jiff::tz::TimeZone;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl Level {
    /// lower-case, like `record.levelname.lower()` in the JSON formatter
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Critical => "critical",
        }
    }

    fn parse(value: &str) -> Level {
        match value.trim().to_uppercase().as_str() {
            "DEBUG" => Level::Debug,
            "WARNING" | "WARN" => Level::Warning,
            "ERROR" => Level::Error,
            "CRITICAL" | "FATAL" => Level::Critical,
            _ => Level::Info,
        }
    }
}

/// A value in a log record's `extra` dict. Display follows Python `str()`
/// (the text formatter prints `f"{k}={v}"`).
#[derive(Debug, Clone)]
pub enum LogValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
    /// Python `None` (renders as `null` in JSON, `None` in text).
    Null,
}

impl LogValue {
    pub fn str(value: impl Into<String>) -> LogValue {
        LogValue::Str(value.into())
    }

    fn display(&self) -> String {
        match self {
            LogValue::Str(s) => s.clone(),
            LogValue::Int(n) => n.to_string(),
            LogValue::Float(f) => py_float(*f),
            LogValue::Bool(b) => b.to_string(),
            LogValue::List(items) => {
                // Python str(['a', 'b']) -> "['a', 'b']"
                let inner: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
                format!("[{}]", inner.join(", "))
            }
            LogValue::Null => "None".to_string(),
        }
    }
}

/// Python str(60.0) is "60.0"; Rust's {} gives "60".
fn py_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

fn to_json(value: &LogValue) -> serde_json::Value {
    match value {
        LogValue::Str(s) => serde_json::Value::String(s.clone()),
        LogValue::Int(n) => (*n).into(),
        LogValue::Float(f) => (*f).into(),
        LogValue::Bool(b) => (*b).into(),
        LogValue::List(items) => serde_json::Value::Array(
            items.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
        ),
        LogValue::Null => serde_json::Value::Null,
    }
}

/// Where formatted records go (stdout in production, memory in tests).
pub trait Sink: Send + Sync {
    fn write_record(&self, level: Level, logger: &str, message: &str, extra: &[(&str, LogValue)]);
}

/// The JSON formatter: time, level, logger, message, then the extra keys
/// (`JsonFormatter.format` in Python; key order preserved).
pub fn format_json(level: Level, logger: &str, message: &str, extra: &[(&str, LogValue)]) -> String {
    let mut out = serde_json::Map::new();
    out.insert("time".into(), serde_json::Value::String(iso_now_utc()));
    out.insert("level".into(), serde_json::Value::String(level.as_str().into()));
    out.insert("logger".into(), serde_json::Value::String(logger.into()));
    out.insert("message".into(), serde_json::Value::String(message.into()));
    for (k, v) in extra {
        out.insert((*k).into(), to_json(v));
    }
    serde_json::Value::Object(out).to_string()
}

/// The text formatter: `time LEVEL message  k=v k=v`.
pub fn format_text(level: Level, message: &str, extra: &[(&str, LogValue)]) -> String {
    let mut line = format!("{:.24} {:<7} {}", local_now(), level.as_str().to_uppercase(), message);
    if !extra.is_empty() {
        let pairs: Vec<String> = extra.iter().map(|(k, v)| format!("{k}={}", v.display())).collect();
        let _ = write!(line, "  {}", pairs.join(" "));
    }
    line
}

/// `datetime.fromtimestamp(created, UTC).isoformat(timespec="milliseconds")`,
/// e.g. "2026-10-19T16:02:03.123+00:00".
fn iso_now_utc() -> String {
    let now = jiff::Timestamp::now().to_zoned(TimeZone::UTC);
    format!("{}.{}+00:00", now.strftime("%Y-%m-%dT%H:%M:%S"), now.subsec_nanosecond() / 1_000_000)
}

/// logging's default formatTime: "2026-10-19 16:02:03,123", local time.
fn local_now() -> String {
    let now = jiff::Zoned::now();
    format!(
        "{},{:03}",
        now.strftime("%Y-%m-%d %H:%M:%S"),
        now.subsec_nanosecond() / 1_000_000
    )
}

pub struct StdoutSink {
    json: bool,
}

impl Sink for StdoutSink {
    fn write_record(&self, level: Level, logger: &str, message: &str, extra: &[(&str, LogValue)]) {
        let line = if self.json {
            format_json(level, logger, message, extra)
        } else {
            format_text(level, message, extra)
        };
        println!("{line}");
    }
}

/// `logs.setup`: a logger writing JSON (or text) to stdout at the given level.
pub fn stdout_logger(level: &str, fmt: &str) -> Logger {
    Logger::stdout_logger_for(level, fmt, "aimap")
}

/// A named logger bound to a sink and a minimum level
/// (`logging.getLogger(__name__)` under the configured root).
#[derive(Clone)]
pub struct Logger {
    pub sink: Arc<dyn Sink>,
    pub min: Level,
    pub name: String,
}

impl Logger {
    pub fn stdout_logger_for(level: &str, fmt: &str, name: impl Into<String>) -> Logger {
        Logger {
            sink: Arc::new(StdoutSink { json: fmt == "json" }),
            min: Level::parse(level),
            name: name.into(),
        }
    }

    pub fn new(sink: Arc<dyn Sink>, level: &str, name: impl Into<String>) -> Logger {
        Logger { sink, min: Level::parse(level), name: name.into() }
    }

    pub fn log(&self, level: Level, message: &str, extra: &[(&str, LogValue)]) {
        if level < self.min {
            return;
        }
        self.sink.write_record(level, &self.name, message, extra);
    }

    pub fn info(&self, message: &str, extra: &[(&str, LogValue)]) {
        self.log(Level::Info, message, extra);
    }

    pub fn warning(&self, message: &str, extra: &[(&str, LogValue)]) {
        self.log(Level::Warning, message, extra);
    }

    pub fn error(&self, message: &str, extra: &[(&str, LogValue)]) {
        self.log(Level::Error, message, extra);
    }
}

/// A sink that throws every record away. For call sites (and tests) that do
/// not assert on logs; the production `Logger` values arrive from `cli`.
pub struct NullSink;

impl Sink for NullSink {
    fn write_record(&self, _: Level, _: &str, _: &str, _: &[(&str, LogValue)]) {}
}

/// A logger that discards everything (stand-in for the tests, and for
/// embedding code that has no logging configured).
pub fn silent_logger(name: impl Into<String>) -> Logger {
    Logger { sink: Arc::new(NullSink), min: Level::Debug, name: name.into() }
}

/// Test stand-in for `caplog`: collects rendered messages (and their levels).
pub struct Capture {
    pub records: Mutex<Vec<(Level, String)>>,
}

impl Capture {
    pub fn new() -> Arc<Capture> {
        Arc::new(Capture { records: Mutex::new(Vec::new()) })
    }

    /// Messages at the given level, like `caplog.records`.
    pub fn messages(&self, level: Level) -> Vec<String> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, _)| *l == level)
            .map(|(_, m)| m.clone())
            .collect()
    }

    pub fn count_containing(&self, needle: &str) -> usize {
        self.records.lock().unwrap().iter().filter(|(_, m)| m.contains(needle)).count()
    }
}

impl Default for Capture {
    fn default() -> Self {
        Capture { records: Mutex::new(Vec::new()) }
    }
}

impl Sink for Capture {
    fn write_record(&self, level: Level, _logger: &str, message: &str, _extra: &[(&str, LogValue)]) {
        self.records.lock().unwrap().push((level, message.to_string()));
    }
}
