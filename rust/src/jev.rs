//! Port of `src/aimap/jev.py`: questions for TypeSafe Jev and the code that
//! turns its answers into labels.
//!
//! One Jev request per message. Every question goes into that request, so they
//! are answered in parallel against the same state. `decide` uses the pattern
//! if Jev picked one with enough confidence; otherwise it falls back to the
//! independent judgments and flags the message for review. Only this module
//! knows the question layout.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::parse::{attachment_names, body_text, MessageMeta};

/// action bucket -> description, in definition order (the order matters for
/// the questions payload, so this is an array of pairs, not a map).
pub const ACTIONS: [(&str, &str); 7] = [
    ("discard", "Nothing to do. Delete or unsubscribe: marketing, promos, vanity stats, generic news, spent one-time codes."),
    ("batch_review", "Automated alert tied to one of the recipient's saved searches or watch lists. Look at it in a weekly batch."),
    ("skim", "Informational and possibly relevant (newsletters from providers the recipient uses, bulletins, price trackers). Read quickly; no action expected."),
    ("review", "Read with care: an account, policy, process or relationship update that may matter but has no immediate deadline."),
    ("verify", "A security or account event (new login, card added, token activated, charge). Confirm it was the recipient, then archive."),
    ("act_now", "Needs the recipient to do something soon: confirm an appointment, upload or sign documents, fix a service that is about to break or be deleted."),
    ("reply", "A real person wrote directly to the recipient and is waiting for an answer."),
];

pub const IMPORTANCE_LEVELS: [&str; 3] = ["Low", "Medium", "High"];

pub const IMPORTANCE_CRITERIA: [&str; 3] = [
    "Low: marketing, coupons, vanity metrics, generic news or digests, or anything unrelated to `recipient_profile`. Safe to delete unread.",
    "Medium: relevant to the recipient's priorities or to an account they use, but nothing breaks if it waits a few days (automated alerts, provider policy updates, statements, bulletins).",
    "High: needs the recipient's attention soon: a real person waiting on a reply, a deadline or appointment, a security alert, or a service about to fail.",
];

/// (question, weight in the priority sum), in definition order.
pub const SIGNALS: [(&str, &str, f64); 6] = [
    ("real_person", "Was `email.body` written personally by a human to the recipient, rather than sent by an automated system or a marketing list?", 3.0),
    ("asks_to_act", "Does `email` ask the recipient to do something specific (reply, confirm, upload, sign, pay, fix)?", 2.0),
    ("time_sensitive", "Does `email` mention a deadline, an appointment, or a consequence if the recipient does nothing soon?", 2.0),
    ("security_event", "Does `email` report a security or account event on the recipient's own account, such as a login, access grant, card added, token activated or charge?", 2.0),
    ("active_priority", "Is `email` directly about one of the priorities in `recipient_profile`?", 1.5),
    ("promotional", "Is the main purpose of `email` to sell or promote something?", -3.0),
];

pub const NONE_OF_THESE: &str = "none_of_these";

static NOREPLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"no-?reply|noresponder|notification|newsletter|info@|news").unwrap()
});

/// `JevError` covers the KeyError/ValueError paths in jev.py.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevError(pub String);

impl std::fmt::Display for JevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for JevError {}

/// Known inbox pattern. `examples` entries are `{"from": ..., "subject": ...}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    pub insight: String,
    pub importance: String,
    pub action_bucket: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<Value>,
}

impl Pattern {
    pub fn validate(&self) -> Result<(), JevError> {
        if !IMPORTANCE_LEVELS.contains(&self.importance.as_str()) {
            return Err(JevError(format!(
                "pattern {:?}: importance must be one of {:?}",
                self.insight, IMPORTANCE_LEVELS
            )));
        }
        if !ACTIONS.iter().any(|(name, _)| *name == self.action_bucket) {
            let mut sorted: Vec<&str> = ACTIONS.iter().map(|(n, _)| *n).collect();
            sorted.sort_unstable();
            return Err(JevError(format!(
                "pattern {:?}: action_bucket must be one of {:?}",
                self.insight, sorted
            )));
        }
        if self.insight == NONE_OF_THESE {
            return Err(JevError(format!("{:?} is reserved", NONE_OF_THESE)));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// accept a known pattern at or above this Choice confidence
    pub pattern_confidence: f64,
    /// Noul probability for a tag to apply
    pub tag_threshold: f64,
    /// flag for review below this importance/action confidence
    pub review_confidence: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            pattern_confidence: 0.5,
            tag_threshold: 0.8,
            review_confidence: 0.4,
        }
    }
}

/// Labels for one message (`Decision` in jev.py).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Decision {
    pub importance: String,
    pub action_bucket: String,
    pub tags: Vec<String>,
    pub insight: Option<String>,
    /// "pattern" | "judgment"
    pub source: String,
    pub needs_review: bool,
    pub priority: f64,
    pub signals: Map<String, Value>,
}

pub fn tag_examples(patterns: &[Pattern], per_tag: usize) -> Map<String, Value> {
    let mut ex: Map<String, Value> = Map::new();
    for p in patterns {
        for t in &p.tags {
            let bucket = ex
                .entry(t.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(items) = bucket {
                for e in &p.examples {
                    if items.len() < per_tag {
                        items.push(e.clone());
                    }
                }
            }
        }
    }
    // Python returns dict(sorted(ex.items())) — sorted by tag name.
    let mut keys: Vec<String> = ex.keys().cloned().collect();
    keys.sort();
    let mut sorted_map = Map::new();
    for k in keys {
        sorted_map.insert(k.clone(), ex.remove(&k).unwrap());
    }
    sorted_map
}

/// All questions for one message, as a raw JSON object so the payload is easy
/// to hash. Insertion order mirrors the Python dict.
pub fn build_questions(patterns: &[Pattern]) -> Value {
    let mut q = Map::new();
    if !patterns.is_empty() {
        let mut criteria = Map::new();
        for p in patterns {
            criteria.insert(
                p.insight.clone(),
                json!({"tags": p.tags, "example_emails": p.examples}),
            );
        }
        criteria.insert(
            NONE_OF_THESE.to_string(),
            Value::String("The email does not match any of the other patterns: a different sender type, topic or purpose.".to_string()),
        );
        q.insert(
            "pattern".to_string(),
            json!({
                "type": "choice",
                "instructions": {
                    "question": "Which known inbox pattern does `email` belong to?",
                    "how_to_decide": "Match on the sender's purpose and topic, using each option's tags and example emails. Pick `none_of_these` if no option describes this kind of email.",
                },
                "criteria": criteria,
            }),
        );
    }
    q.insert(
        "importance".to_string(),
        json!({
            "type": "score",
            "instructions": "How important is `email` to the recipient described in `recipient_profile`?",
            "criteria": IMPORTANCE_CRITERIA,
        }),
    );
    let mut actions_map = Map::new();
    for (name, description) in ACTIONS {
        actions_map.insert(name.to_string(), Value::String(description.to_string()));
    }
    q.insert(
        "action".to_string(),
        json!({
            "type": "choice",
            "instructions": "What should the recipient described in `recipient_profile` do with `email`?",
            "criteria": actions_map,
        }),
    );
    for (tag, examples) in tag_examples(patterns, 3) {
        q.insert(
            format!("tag:{}", tag),
            json!({
                "type": "noul",
                "instructions": {
                    "tag": tag,
                    "question": "Does the inbox label `tag` apply to `email`?",
                    "emails_that_have_this_label": examples,
                },
            }),
        );
    }
    for (name, question, _) in SIGNALS {
        q.insert(
            format!("sig:{}", name),
            json!({"type": "noul", "instructions": question}),
        );
    }
    Value::Object(q)
}

/// Same questions, model and profile give the same key, so a message is not
/// classified twice. Must match `json.dumps(..., sort_keys=True,
/// ensure_ascii=False)` byte for byte.
pub fn classifier_key(questions: &Value, model: &str, profile: &Value) -> String {
    let mut root = Map::new();
    root.insert("q".to_string(), questions.clone());
    root.insert("m".to_string(), json!(model));
    root.insert("p".to_string(), profile.clone());
    let blob = py_dumps_sorted(&Value::Object(root));
    let digest = Sha256::digest(blob.as_bytes());
    hex(&digest)[..32].to_string()
}

/// `json.dumps(obj, sort_keys=True, ensure_ascii=False)`: recursively key-sorted
/// object with Python's default `", "` / `": "` separators. Public so tools can
/// diff blobs against Python byte-for-byte.
pub fn py_dumps_sorted(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap(),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_dumps_sorted).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        py_dumps_sorted(&m[*k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Facts code can compute exactly, so Jev does not have to guess them.
pub fn header_facts(raw: &[u8], meta: &MessageMeta, account: &str) -> Value {
    let sender = meta.from_email.as_deref().unwrap_or("").to_lowercase();
    json!({
        "sent_by_recipient_themself": sender == account.to_lowercase(),
        "is_reply_in_thread": meta.in_reply_to.is_some(),
        "bulk_mail_headers": meta.has_list_headers,
        "sender_is_noreply_address": !sender.is_empty() && NOREPLY.is_match(&sender),
        "attachments": attachment_names(raw, 5),
    })
}

/// The Jev state: profile context plus the message facts and a trimmed body.
pub fn build_state(
    profile: &Value,
    account: &str,
    raw: &[u8],
    meta: &MessageMeta,
    body_chars: usize,
) -> Value {
    let sender = match (&meta.from_name, &meta.from_email) {
        (Some(name), email) => format!("{} <{}>", name, email.as_deref().unwrap_or("None")),
        (None, Some(email)) => email.clone(),
        (None, None) => "(unknown)".to_string(),
    };
    let body = body_text(raw, body_chars);
    let body = if body.is_empty() { "(no text body)".to_string() } else { body };
    json!({
        "recipient_profile": profile,
        "email": {
            "to_account": account,
            "from": sender,
            "subject": meta.subject.clone().unwrap_or_default(),
            "facts": header_facts(raw, meta, account),
            "body": body,
        },
    })
}

/// Turn raw Jev answers into labels. Pure, so stored answers can be
/// re-decided later.
pub fn decide(
    answers: &Map<String, Value>,
    patterns: &[Pattern],
    t: &Thresholds,
) -> Result<Decision, JevError> {
    let by_insight: HashMap<&str, &Pattern> =
        patterns.iter().map(|p| (p.insight.as_str(), p)).collect();

    let imp = answers
        .get("importance")
        .ok_or_else(|| JevError("'importance'".to_string()))?;
    let act = answers
        .get("action")
        .ok_or_else(|| JevError("'action'".to_string()))?;
    let pat = answers.get("pattern");

    let score = num_field(imp, "score")?;
    let imp_confidence = num_field(imp, "confidence")?;
    let act_choice = str_field(act, "choice")?;
    let act_confidence = num_field(act, "confidence")?;

    let mut tag_probs: Vec<(String, f64)> = Vec::new();
    for (k, a) in answers {
        if let Some(tag) = k.strip_prefix("tag:") {
            tag_probs.push((tag.to_string(), num_field(a, "noul")?));
        }
    }
    let mut judged_tags: Vec<String> = {
        let mut order: Vec<(String, f64)> = tag_probs.clone();
        order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)); // stable, like sorted()
        order
            .into_iter()
            .filter(|(_, p)| *p >= t.tag_threshold)
            .map(|(tg, _)| tg)
            .collect()
    };
    if judged_tags.is_empty() && !tag_probs.is_empty() {
        // max(tag_probs, key=...) picks the first maximum in insertion order
        let best = tag_probs
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, (tg, _))| tg.clone());
        if let Some(tg) = best {
            judged_tags.push(tg);
        }
    }

    let mut signals_map: Vec<(String, f64)> = Vec::new();
    for (k, a) in answers {
        if let Some(sig) = k.strip_prefix("sig:") {
            signals_map.push((sig.to_string(), num_field(a, "noul")?));
        }
    }
    let mut priority = 0.0;
    for (name, p) in &signals_map {
        if let Some((_, _, weight)) = SIGNALS.iter().find(|(n, _, _)| n == name) {
            priority += weight * p;
        }
    }
    priority += 2.0 * score;

    let judged_index = py_round_i(score).clamp(0, 2) as usize;
    let judged_importance = IMPORTANCE_LEVELS[judged_index].to_string();

    let pat_choice = pat.and_then(|p| str_field(p, "choice").ok());
    let pat_confidence = pat.as_ref().map(|p| num_field(p, "confidence")).transpose()?;
    let use_pattern = pat_choice
        .as_deref()
        .is_some_and(|choice| by_insight.contains_key(choice))
        && pat_confidence.is_some_and(|c| c >= t.pattern_confidence);

    let (importance, action_bucket, tags, insight, source, disagree);
    if use_pattern {
        let p = by_insight[pat_choice.as_deref().unwrap()];
        importance = p.importance.clone();
        action_bucket = p.action_bucket.clone();
        tags = p.tags.clone();
        insight = Some(p.insight.clone());
        source = "pattern".to_string();
        // Two independent routes disagreeing is a cheap, useful review signal.
        disagree = judged_importance != importance || act_choice != action_bucket;
    } else {
        importance = judged_importance.clone();
        action_bucket = act_choice.clone();
        tags = judged_tags.clone();
        source = "judgment".to_string();
        disagree = false;
        insight = None;
    }

    let needs_review =
        !use_pattern || disagree || imp_confidence < t.review_confidence || act_confidence < t.review_confidence;

    let mut closest: Option<String> = None;
    if let Some(p) = pat {
        if let Some(Value::Object(probs)) = p.get("probabilities") {
            for (k, v) in probs {
                if k == NONE_OF_THESE {
                    continue;
                }
                let Some(val) = v.as_f64() else { continue };
                if closest
                    .as_ref()
                    .map(|c| val > probs[c].as_f64().unwrap_or(f64::NEG_INFINITY))
                    .unwrap_or(true)
                {
                    closest = Some(k.clone());
                }
            }
        }
    }

    let mut signals = Map::new();
    for (name, v) in &signals_map {
        signals.insert(name.clone(), json!(py_round_n(*v, 3)));
    }
    signals.insert(
        "pattern_confidence".to_string(),
        match pat_confidence {
            Some(c) => json!(py_round_n(c, 3)),
            None => Value::Null,
        },
    );
    signals.insert("closest_pattern".to_string(), closest.map(Value::String).unwrap_or(Value::Null));
    signals.insert("judged_importance".to_string(), json!(judged_importance));
    signals.insert("importance_score".to_string(), json!(py_round_n(score, 3)));
    signals.insert("importance_confidence".to_string(), json!(py_round_n(imp_confidence, 3)));
    signals.insert("judged_action".to_string(), json!(act_choice));
    signals.insert("action_confidence".to_string(), json!(py_round_n(act_confidence, 3)));
    signals.insert("judged_tags".to_string(), json!(judged_tags));

    Ok(Decision {
        importance,
        action_bucket,
        tags,
        insight,
        source,
        needs_review,
        priority: py_round_n(priority, 2),
        signals,
    })
}

fn num_field(v: &Value, key: &str) -> Result<f64, JevError> {
    v.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| JevError(format!("missing number '{}.{}'", v, key)))
}

fn str_field(v: &Value, key: &str) -> Result<String, JevError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| JevError(format!("missing string '{}.{}'", v, key)))
}

/// Python's round() on floats: half to even.
fn py_round_i(x: f64) -> i64 {
    let frac = x - x.trunc();
    if (frac - 0.5).abs() < f64::EPSILON {
        let t = x.trunc();
        if t % 2.0 == 0.0 {
            t as i64
        } else {
            (t + x.signum()) as i64
        }
    } else {
        x.round() as i64
    }
}

/// Python's round(x, n): correct rounding of the exact binary value, ties to
/// even — what Rust's {:.n} formatting does too.
fn py_round_n(x: f64, n: usize) -> f64 {
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
