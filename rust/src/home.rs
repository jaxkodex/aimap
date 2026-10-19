//! Port of `src/aimap/home.py`: turn labelled messages into the app's Home
//! screen. Pure: no database, no Jev.
//!
//! - act_now: act_now and verify buckets, highest priority first.
//! - waiting: reply bucket, longest waiting first. Until Sent mail is synced
//!   this means "someone expects an answer", not "you have not replied".
//! - sorted: everything else, grouped by pattern insight, first tag, or bucket.
//!
//! A message the user handled in the app leaves act_now and waiting; one pushed
//! to later keeps its section but sorts after every other card, newest 'later'
//! last. Reasons and summaries are templates over stored fields, so they cost
//! no inference.

use std::collections::HashMap;

use jiff::{Timestamp, tz::TimeZone};
use serde_json::{json, Map, Value};

pub const ACT_NOW: [&str; 2] = ["act_now", "verify"];
pub const WAITING: [&str; 1] = ["reply"];

/// Group name for a message with neither a pattern nor tags.
pub fn bucket_group(action_bucket: &str) -> &'static str {
    match action_bucket {
        "discard" => "Can discard",
        "batch_review" => "Alerts",
        "skim" => "To skim",
        "review" => "To review",
        "act_now" => "Act now",
        "verify" => "Verify",
        "reply" => "Replies",
        _ => "Other",
    }
}

/// Signal -> reason, in the order they are shown. A signal counts at or above
/// REASON_THRESHOLD.
pub const REASONS: [(&str, &str); 5] = [
    ("security_event", "Security event on your account"),
    ("time_sensitive", "Mentions a deadline"),
    ("asks_to_act", "Asks you to do something"),
    ("real_person", "Written to you by a person"),
    ("active_priority", "About one of your priorities"),
];
pub const REASON_THRESHOLD: f64 = 0.5;

/// One labelled message as Home sees it (`Item` in home.py).
#[derive(Debug, Clone, Default)]
pub struct Item {
    pub message_id: i64,
    pub account: String,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub subject: Option<String>,
    pub sent_at: Option<Timestamp>,
    pub unread: bool,
    pub importance: Option<String>,
    pub action_bucket: Option<String>,
    pub tags: Vec<String>,
    pub insight: Option<String>,
    pub needs_review: bool,
    pub priority: f64,
    pub signals: Value,
    pub profile: Option<String>,
    /// None, "handled" or "later"
    pub state: Option<String>,
    pub state_changed_at: Option<Timestamp>,
}

impl Item {
    pub fn sender(&self) -> String {
        self.from_name
            .clone()
            .or_else(|| self.from_email.clone())
            .unwrap_or_else(|| "(unknown)".to_string())
    }

    pub fn from_self(&self) -> bool {
        self.from_email
            .as_deref()
            .map(|e| e.to_lowercase())
            .unwrap_or_default()
            == self.account.to_lowercase()
    }
}

pub fn reasons(signals: &Value) -> Vec<String> {
    REASONS
        .iter()
        .filter(|(name, _)| {
            signals
                .get(name)
                .and_then(Value::as_f64)
                .is_some_and(|v| v >= REASON_THRESHOLD)
        })
        .map(|(_, text)| text.to_string())
        .collect()
}

pub fn group_name(item: &Item) -> String {
    if let Some(insight) = &item.insight {
        return insight.clone();
    }
    if let Some(tag) = item.tags.first() {
        return tag.clone();
    }
    bucket_group(item.action_bucket.as_deref().unwrap_or("")).to_string()
}

/// `"AWS Billing, Uber + 3 more"`: the most frequent senders, then a count of
/// the rest. `Counter.most_common` ties keep insertion order.
pub fn group_summary(items: &[&Item], senders: usize) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for i in items {
        let name = i.sender();
        if let Some(entry) = counts.iter_mut().find(|(n, _)| *n == name) {
            entry.1 += 1;
        } else {
            counts.push((name, 1));
        }
    }
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n)); // stable: insertion order on ties
    let top: Vec<String> = counts.iter().take(senders).map(|(n, _)| n.clone()).collect();
    let rest = counts.len().saturating_sub(top.len());
    let mut summary = top.join(", ");
    if rest > 0 {
        summary.push_str(&format!(" + {rest} more"));
    }
    summary
}

pub fn card(item: &Item) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("message_id".into(), json!(item.message_id));
    m.insert("account".into(), json!(item.account));
    m.insert("profile".into(), json!(item.profile));
    m.insert("sender".into(), json!(item.sender()));
    m.insert("from_email".into(), json!(item.from_email));
    m.insert("subject".into(), json!(item.subject));
    m.insert(
        "sent_at".into(),
        json!(item.sent_at.as_ref().map(|t| crate::pgtime::iso(*t))),
    );
    m.insert("unread".into(), json!(item.unread));
    m.insert("action_bucket".into(), json!(item.action_bucket));
    m.insert("importance".into(), json!(item.importance));
    m.insert("priority".into(), json!(item.priority));
    m.insert("needs_review".into(), json!(item.needs_review));
    m.insert("reasons".into(), json!(reasons(&item.signals)));
    m.insert("state".into(), json!(item.state));
    m
}

fn ts_of(item: &Item) -> f64 {
    item.sent_at.map(|t| t.as_second() as f64).unwrap_or(0.0)
}

/// Sort key prefix: later cards go after the rest, and the one marked most
/// recently goes last.
fn later_key(item: &Item) -> (i32, f64) {
    if item.state.as_deref() != Some("later") {
        return (0, 0.0);
    }
    (1, item.state_changed_at.map(|t| t.as_second() as f64).unwrap_or(0.0))
}

/// Which Home section a message lands in: act_now, waiting, sorted, or
/// unclassified.
pub fn section(item: &Item) -> &'static str {
    let Some(bucket) = &item.action_bucket else {
        return "unclassified";
    };
    if ACT_NOW.contains(&bucket.as_str()) {
        return "act_now";
    }
    if WAITING.contains(&bucket.as_str()) {
        "waiting"
    } else {
        "sorted"
    }
}

/// 24 rows, one per hour of today in `tz`, counting the messages that arrived
/// in each section.
pub fn by_hour(items: &[Item], now: Timestamp, tz: &TimeZone) -> Vec<Map<String, Value>> {
    let now_local = now.to_zoned(tz.clone());
    let today = now_local.date();
    let mut hours: Vec<Map<String, Value>> = (0..24)
        .map(|h| {
            let mut m = Map::new();
            m.insert("hour".into(), json!(h));
            m.insert("act_now".into(), json!(0));
            m.insert("waiting".into(), json!(0));
            m.insert("sorted".into(), json!(0));
            m.insert("unclassified".into(), json!(0));
            m
        })
        .collect();
    for i in items {
        if i.from_self() {
            continue;
        }
        let Some(sent_at) = i.sent_at else { continue };
        let local = sent_at.to_zoned(tz.clone());
        if local.date() == today {
            let hour = &mut hours[local.hour() as usize];
            let key = section(i);
            let n = hour.get(key).and_then(Value::as_i64).unwrap_or(0);
            hour.insert(key.to_string(), json!(n + 1));
        }
    }
    hours
}

/// Home sections for the labelled messages in the window. Unlabelled ones only
/// count as new.
pub fn build(
    items: &[Item],
    new_since: Timestamp,
    now: Timestamp,
    tz: &TimeZone,
    sorted_at: Option<Timestamp>,
    handled_today: i64,
) -> Value {
    let inbox: Vec<&Item> = items.iter().filter(|i| !i.from_self()).collect();
    let labelled: Vec<&Item> = inbox.iter().copied().filter(|i| i.action_bucket.is_some()).collect();
    let live: Vec<&Item> = labelled.iter().copied().filter(|i| i.state.as_deref() != Some("handled")).collect();

    let mut act: Vec<&Item> = live
        .iter()
        .copied()
        .filter(|i| i.action_bucket.as_deref().is_some_and(|b| ACT_NOW.contains(&b)))
        .collect();
    act.sort_by(|a, b| {
        later_key(a).0.cmp(&later_key(b).0).then(later_key(a).1.total_cmp(&later_key(b).1))
            .then(b.priority.partial_cmp(&a.priority).unwrap_or(std::cmp::Ordering::Equal))
            .then(ts_of(b).partial_cmp(&ts_of(a)).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut waiting: Vec<&Item> = live
        .iter()
        .copied()
        .filter(|i| i.action_bucket.as_deref().is_some_and(|b| WAITING.contains(&b)))
        .collect();
    waiting.sort_by(|a, b| later_key(a).0.cmp(&later_key(b).0).then(later_key(a).1.total_cmp(&later_key(b).1)).then(ts_of(a).total_cmp(&ts_of(b))));

    let rest: Vec<&Item> = labelled
        .iter()
        .copied()
        .filter(|i| {
            let b = i.action_bucket.as_deref().unwrap_or("");
            !ACT_NOW.contains(&b) && !WAITING.contains(&b)
        })
        .collect();

    let mut groups: HashMap<String, Vec<&Item>> = HashMap::new();
    for i in &rest {
        groups.entry(group_name(i)).or_default().push(i);
    }
    let mut group_keys: Vec<(String, usize)> =
        groups.iter().map(|(name, g)| (name.clone(), g.len())).collect();
    group_keys.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let sorted_groups: Vec<Value> = group_keys
        .iter()
        .map(|(name, _)| {
            let g = &groups[name];
            let unread: i64 = g.iter().filter(|i| i.unread).count() as i64;
            let latest_at = g
                .iter()
                .filter_map(|i| i.sent_at)
                .max()
                .map(crate::pgtime::iso);
            json!({
                "name": name,
                "count": g.len(),
                "unread": unread,
                "latest_at": latest_at,
                "summary": group_summary(g, 2),
            })
        })
        .collect();

    let new = inbox
        .iter()
        .filter(|i| i.sent_at.is_some_and(|t| t >= new_since))
        .count() as i64;

    json!({
        "brief": {
            "new": new,
            "act_now": act.len(),
            "waiting": waiting.len(),
            "sorted": rest.len(),
            "unclassified": inbox.len() - labelled.len(),
            "handled_today": handled_today,
            "text": brief_text(new, act.len() as i64, waiting.len() as i64),
            "sorted_at": sorted_at.map(crate::pgtime::iso),
            "by_hour": by_hour(&inbox.iter().copied().cloned().collect::<Vec<_>>(), now, tz),
        },
        "act_now": act.iter().map(|i| Value::Object(card(i))).collect::<Vec<_>>(),
        "waiting": waiting
            .iter()
            .map(|i| {
                let mut c = card(i);
                c.insert("waiting_since".into(), json!(i.sent_at.as_ref().map(|t| crate::pgtime::iso(*t))));
                Value::Object(c)
            })
            .collect::<Vec<_>>(),
        "sorted": sorted_groups,
    })
}

pub fn brief_text(new: i64, act: i64, waiting: i64) -> String {
    if new == 0 && act == 0 && waiting == 0 {
        return "Nothing new. You're all caught up.".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if act > 0 {
        parts.push(format!(
            "{} need{} you now",
            act,
            if act == 1 { "s" } else { "" }
        ));
    }
    if waiting > 0 {
        parts.push(format!(
            "{} {} waiting on a reply",
            waiting,
            if waiting == 1 { "person is" } else { "people are" }
        ));
    }
    let head = format!("{new} new email{}.", if new == 1 { "" } else { "s" });
    if parts.is_empty() {
        return head + " None of them need you right now.";
    }
    format!("{} {}.", head, parts.join(" and "))
}
