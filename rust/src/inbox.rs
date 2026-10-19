//! Port of `src/aimap/inbox.py`: queries behind the API — messages with their
//! latest labels, per account.
//!
//! A message is unread when none of its locations carries \Seen, and flagged
//! when any carries \Flagged. Flags are the ones seen at ingestion, so they go
//! stale when mail is read elsewhere until flag sync lands. Lists page by
//! (time, id), newest first, with an opaque cursor.
//!
//! Everything here reads, except `set_state`: the one write aimap does, and it
//! only touches aimap's own message_state table.

use base64::Engine;
use postgres::GenericClient;
use std::str::FromStr;
use serde_json::{json, Value};

use crate::home::{reasons, Item};
use crate::parse::{body_text, strip_quoted};
use crate::pgtime::{iso, Ts};
use crate::storage::{S3Client, S3Error, Store};

/// DB and S3 errors surface separately here, like psycopg errors and S3
/// exceptions do in Python.
#[derive(Debug)]
pub enum InboxError {
    Postgres(postgres::Error),
    S3(S3Error),
    Cursor(CursorError),
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboxError::Postgres(e) => write!(f, "{e}"),
            InboxError::S3(e) => write!(f, "s3 error: {}", e.code),
            InboxError::Cursor(_) => write!(f, "invalid cursor"),
        }
    }
}
impl std::error::Error for InboxError {}

impl From<postgres::Error> for InboxError {
    fn from(e: postgres::Error) -> Self {
        InboxError::Postgres(e)
    }
}
impl From<CursorError> for InboxError {
    fn from(e: CursorError) -> Self {
        InboxError::Cursor(e)
    }
}

pub const LATEST: &str = r#"
    LEFT JOIN LATERAL (
        SELECT importance, action_bucket, tags, insight, needs_review, priority, signals, created_at
        FROM classifications WHERE message_id = m.id
        ORDER BY created_at DESC, id DESC LIMIT 1
    ) c ON true"#;
pub const UNREAD: &str =
    r"NOT EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Seen' = ANY(l.flags))";
pub const FLAGGED: &str =
    r"EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Flagged' = ANY(l.flags))";
pub const AT: &str = "coalesce(m.sent_at, m.created_at)";
pub const STATE: &str = "LEFT JOIN message_state s ON s.message_id = m.id";

pub const FILTER_NAMES: [&str; 4] = ["all", "unread", "flagged", "needs_review"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorError(pub String);

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid cursor")
    }
}
impl std::error::Error for CursorError {}

fn filter_sql(filter: &str) -> Result<&'static str, CursorError> {
    Ok(match filter {
        "all" => "",
        "unread" => r"AND NOT EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Seen' = ANY(l.flags))",
        "flagged" => r"AND EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Flagged' = ANY(l.flags))",
        "needs_review" => "AND c.needs_review",
        _ => {
            let mut names: Vec<&str> = FILTER_NAMES.to_vec();
            names.sort_unstable();
            // Python: ValueError(f"filter must be one of {sorted(FILTERS)}")
            return Err(CursorError(format!("filter must be one of {names:?}")));
        }
    })
}

/// Decide if a message needs a reply based on labels already stored.
///
/// Returns {"needed", "reason"} when labels exist, or None if not yet
/// classified. Reply needed when: action_bucket is 'reply', or 'act_now' with
/// real_person signal and not bulk.
pub fn needs_reply(action_bucket: Option<&str>, signals: &Value, bulk: bool) -> Option<Value> {
    let action_bucket = action_bucket?;
    if action_bucket == "reply" {
        let signal_reasons = reasons(signals);
        let reason = signal_reasons
            .first()
            .cloned()
            .unwrap_or_else(|| "Someone expects a reply.".to_string());
        return Some(json!({"needed": true, "reason": reason}));
    }
    if action_bucket == "act_now" {
        let has_real_person = signals.get("real_person").and_then(Value::as_f64).unwrap_or(0.0) > 0.0;
        if has_real_person && !bulk {
            let signal_reasons = reasons(signals);
            let reason = signal_reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "A person wrote to you.".to_string());
            return Some(json!({"needed": true, "reason": reason}));
        }
    }
    Some(json!({"needed": false, "reason": null}))
}

pub fn encode_cursor(at: jiff::Timestamp, message_id: i64) -> String {
    let raw = format!("{}|{}", iso(at), message_id);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

pub fn decode_cursor(cursor: &str) -> Result<(jiff::Timestamp, i64), CursorError> {
    let padded = format!("{}{}", cursor, "=".repeat((4 - cursor.len() % 4) % 4));
    let raw = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or(CursorError("invalid cursor".into()))?;
    let (at, message_id) = raw.rsplit_once('|').ok_or(CursorError("invalid cursor".into()))?;
    let at = jiff::Timestamp::from_str(at).map_err(|_| CursorError("invalid cursor".into()))?;
    let message_id: i64 = message_id.parse().map_err(|_| CursorError("invalid cursor".into()))?;
    Ok((at, message_id))
}

pub fn home_items<C: GenericClient>(conn: &mut C, since: jiff::Timestamp, account: Option<&str>) -> Result<Vec<Item>, postgres::Error> {
    let sql = format!(
        "SELECT m.id, a.address, m.from_email, m.from_name, m.subject, m.sent_at, {UNREAD},
                c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority::float8, c.signals, p.name,
                s.state, s.changed_at
         FROM messages m
         JOIN accounts a ON a.id = m.account_id
         JOIN profiles p ON p.id = a.profile_id
         {LATEST}
         {STATE}
         WHERE {AT} >= $1 AND ($2::text IS NULL OR a.address = $2)"
    );
    let account_param: Option<&str> = account;
    let rows = conn.query(&sql, &[&Ts(since), &account_param])?;
    Ok(rows
        .iter()
        .map(|r| Item {
            message_id: r.get(0),
            account: r.get(1),
            from_email: r.get(2),
            from_name: r.get(3),
            subject: r.get(4),
            sent_at: r.get::<_, Option<Ts>>(5).map(|t| t.0),
            unread: r.get(6),
            importance: r.get(7),
            action_bucket: r.get(8),
            tags: r.get::<_, Option<Vec<String>>>(9).unwrap_or_default(),
            insight: r.get(10),
            needs_review: r.get::<_, Option<bool>>(11).unwrap_or(false),
            priority: r.get::<_, Option<f64>>(12).unwrap_or(0.0),
            signals: r.get::<_, Option<Value>>(13).unwrap_or_else(|| json!({})),
            profile: r.get(14),
            state: r.get(15),
            state_changed_at: r.get::<_, Option<Ts>>(16).map(|t| t.0),
        })
        .collect())
}

/// How many messages were marked handled since `since`, for one account or all
/// of them.
pub fn handled_since<C: GenericClient>(conn: &mut C, since: jiff::Timestamp, account: Option<&str>) -> Result<i64, postgres::Error> {
    Ok(conn
        .query_one(
            "SELECT count(*) FROM message_state s
             JOIN messages m ON m.id = s.message_id
             JOIN accounts a ON a.id = m.account_id
             WHERE s.state = 'handled' AND s.changed_at >= $1
               AND ($2::text IS NULL OR a.address = $2)",
            &[&Ts(since), &account],
        )?
        .get(0))
}

/// Mark a message handled or later, or clear it with `state=None` (undo).
/// None if there is no such message. Idempotent, and every call refreshes
/// `changed_at`, so the newest 'later' sorts last on Home.
pub fn set_state<C: GenericClient>(
    conn: &mut C,
    message_id: i64,
    state: Option<&str>,
    changed_by: &str,
) -> Result<Option<Value>, postgres::Error> {
    if conn
        .query_opt("SELECT 1 FROM messages WHERE id = $1", &[&message_id])?
        .is_none()
    {
        return Ok(None);
    }
    match state {
        None => {
            conn.execute("DELETE FROM message_state WHERE message_id = $1", &[&message_id])?;
            Ok(Some(json!({"message_id": message_id, "state": null, "changed_at": null})))
        }
        Some(state) => {
            let r = conn.query_one(
                "INSERT INTO message_state (message_id, state, changed_by) VALUES ($1, $2, $3)
                 ON CONFLICT (message_id) DO UPDATE
                     SET state = excluded.state, changed_by = excluded.changed_by, changed_at = now()
                 RETURNING state, changed_at",
                &[&message_id, &state, &changed_by],
            )?;
            Ok(Some(json!({
                "message_id": message_id,
                "state": r.get::<_, String>(0),
                "changed_at": r.get::<_, Option<Ts>>(1).map(|t| iso(t.0)),
            })))
        }
    }
}

/// When the newest classification was stored, for one account or all of them.
pub fn sorted_at<C: GenericClient>(conn: &mut C, account: Option<&str>) -> Result<Option<jiff::Timestamp>, postgres::Error> {
    Ok(conn
        .query_one(
            "SELECT max(c.created_at) FROM classifications c
             JOIN messages m ON m.id = c.message_id
             JOIN accounts a ON a.id = m.account_id
             WHERE $1::text IS NULL OR a.address = $1",
            &[&account],
        )?
        .get::<_, Option<Ts>>(0)
        .map(|t| t.0))
}

/// One page of messages, newest first, and the cursor for the next page
/// (None on the last one).
pub fn list_messages<C: GenericClient>(
    conn: &mut C,
    account: Option<&str>,
    filter: &str,
    cursor: Option<&str>,
    limit: i64,
) -> Result<(Vec<Value>, Option<String>), InboxError> {
    let filter_sql = filter_sql(filter)?;
    let after = match cursor {
        Some(c) => Some(decode_cursor(c)?),
        None => None,
    };
    let sql = format!(
        "SELECT m.id, a.address, m.from_email, m.from_name, m.subject, m.sent_at, {AT}, {UNREAD}, {FLAGGED},
                c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority::float8
         FROM messages m
         JOIN accounts a ON a.id = m.account_id
         {LATEST}
         WHERE ($1::text IS NULL OR a.address = $1)
           AND ($2::timestamptz IS NULL OR ({AT}, m.id) < ($2, $3))
           {filter_sql}
         ORDER BY {AT} DESC, m.id DESC
         LIMIT $4"
    );
    let (at_param, id_param): (Option<Ts>, Option<i64>) = match &after {
        Some((at, id)) => (Some(Ts(*at)), Some(*id)),
        None => (None, None),
    };
    let rows = conn.query(&sql, &[&account, &at_param, &id_param, &(limit + 1)])?;
    let page: Vec<&postgres::Row> = rows.iter().take(limit as usize).collect();
    let items: Vec<Value> = page
        .iter()
        .map(|r| {
            json!({
                "message_id": r.get::<_, i64>(0),
                "account": r.get::<_, String>(1),
                "from_email": r.get::<_, Option<String>>(2),
                "sender": r.get::<_, Option<String>>(3).or(r.get::<_, Option<String>>(2)).unwrap_or_else(|| "(unknown)".into()),
                "subject": r.get::<_, Option<String>>(4),
                "sent_at": r.get::<_, Option<Ts>>(5).map(|t| iso(t.0)),
                "unread": r.get::<_, bool>(7),
                "flagged": r.get::<_, bool>(8),
                "importance": r.get::<_, Option<String>>(9),
                "action_bucket": r.get::<_, Option<String>>(10),
                "tags": r.get::<_, Option<Vec<String>>>(11).unwrap_or_default(),
                "insight": r.get::<_, Option<String>>(12),
                "needs_review": r.get::<_, Option<bool>>(13),
                "priority": r.get::<_, Option<f64>>(14),
            })
        })
        .collect();
    let next_cursor = if rows.len() > page.len() {
        let last = page.last().unwrap();
        let at: Ts = last.get(6);
        Some(encode_cursor(at.0, last.get(0)))
    } else {
        None
    };
    Ok((items, next_cursor))
}

/// Metadata, latest labels with their signals, and where the message is stored.
pub fn get_message<C: GenericClient>(conn: &mut C, message_id: i64) -> Result<Option<Value>, postgres::Error> {
    let sql = format!(
        "SELECT m.id, a.address, m.rfc822_message_id, m.from_email, m.from_name, m.subject, m.sent_at,
                m.in_reply_to, m.has_list_headers, {UNREAD}, {FLAGGED},
                c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority::float8, c.signals,
                c.created_at, s.state, p.name
         FROM messages m
         JOIN accounts a ON a.id = m.account_id
         JOIN profiles p ON p.id = a.profile_id
         {LATEST}
         {STATE}
         WHERE m.id = $1"
    );
    let Some(r) = conn.query_opt(&sql, &[&message_id])? else {
        return Ok(None);
    };
    let action_bucket: Option<String> = r.get(12);
    let signals: Value = r.get::<_, Option<Value>>(17).unwrap_or_else(|| json!({}));
    let bulk: bool = r.get(8);
    let reply = if action_bucket.is_some() {
        needs_reply(action_bucket.as_deref(), &signals, bulk)
    } else {
        None
    };
    let locations = conn.query(
        "SELECT mailbox, flags, s3_key FROM message_locations WHERE message_id = $1
         ORDER BY ingested_at DESC, id DESC",
        &[&message_id],
    )?;

    Ok(Some(json!({
        "message_id": r.get::<_, i64>(0),
        "account": r.get::<_, String>(1),
        "profile": r.get::<_, String>(20),
        "rfc822_message_id": r.get::<_, String>(2),
        "from_email": r.get::<_, Option<String>>(3),
        "sender": r.get::<_, Option<String>>(4).or(r.get::<_, Option<String>>(3)).unwrap_or_else(|| "(unknown)".into()),
        "subject": r.get::<_, Option<String>>(5),
        "sent_at": r.get::<_, Option<Ts>>(6).map(|t| iso(t.0)),
        "in_reply_to": r.get::<_, Option<String>>(7),
        "bulk": bulk,
        "unread": r.get::<_, bool>(9),
        "flagged": r.get::<_, bool>(10),
        "state": r.get::<_, Option<String>>(19),
        "labels": action_bucket.is_some().then(|| json!({
            "importance": r.get::<_, Option<String>>(11),
            "action_bucket": action_bucket,
            "tags": r.get::<_, Option<Vec<String>>>(13).unwrap_or_default(),
            "insight": r.get::<_, Option<String>>(14),
            "needs_review": r.get::<_, Option<bool>>(15),
            "priority": r.get::<_, Option<f64>>(16),
            "signals": signals,
            "classified_at": r.get::<_, Option<Ts>>(18).map(|t| iso(t.0)),
        })),
        "reply": reply,
        "mailboxes": locations.iter().map(|l| json!({
            "mailbox": l.get::<_, String>(0),
            "flags": l.get::<_, Vec<String>>(1),
        })).collect::<Vec<_>>(),
        "s3_key": locations.first().map(|l| l.get::<_, String>(2)),
    })))
}

pub fn accounts<C: GenericClient>(conn: &mut C) -> Result<Vec<Value>, postgres::Error> {
    let rows = conn.query(
        &format!(
            "SELECT a.address, p.name, count(m.id), count(m.id) FILTER (WHERE {UNREAD})
             FROM accounts a
             JOIN profiles p ON p.id = a.profile_id
             LEFT JOIN messages m ON m.account_id = a.id
             GROUP BY a.id, a.address, p.name
             ORDER BY a.address"
        ),
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "address": r.get::<_, String>(0),
                "profile": r.get::<_, String>(1),
                "messages": r.get::<_, i64>(2),
                "unread": r.get::<_, i64>(3),
            })
        })
        .collect())
}

/// The messages in the same thread, newest first, with excerpts from S3.
pub fn get_thread<C: GenericClient, S: S3Client>(
    conn: &mut C,
    message_id: i64,
    store: &Store<S>,
) -> Result<Option<Value>, InboxError> {
    let Some(row) = conn.query_opt("SELECT thread_id FROM messages WHERE id = $1", &[&message_id])? else {
        return Ok(None);
    };
    let thread_id: String = row.get(0);

    let rows = conn.query(
        "SELECT m.id, m.from_name, m.from_email, m.subject, m.sent_at,
                (SELECT s3_key FROM message_locations WHERE message_id = m.id ORDER BY ingested_at DESC LIMIT 1),
                a.address
         FROM messages m
         JOIN accounts a ON a.id = m.account_id
         WHERE m.thread_id = $1
         ORDER BY coalesce(m.sent_at, m.created_at) DESC, m.id DESC",
        &[&thread_id],
    )?;

    let mut messages = Vec::new();
    for r in &rows {
        let msg_id: i64 = r.get(0);
        let from_name: Option<String> = r.get(1);
        let from_email: Option<String> = r.get(2);
        let subject: Option<String> = r.get(3);
        let sent_at: Option<Ts> = r.get(4);
        let s3_key: Option<String> = r.get(5);
        let account: String = r.get(6);

        // Read body from S3 and create the excerpt
        let mut excerpt: Option<String> = None;
        if let Some(s3_key) = &s3_key {
            let got = store.get_raw(s3_key).map_err(InboxError::S3)?;
            if let Some((raw, _)) = got {
                let full_text = body_text(&raw, 50_000);
                let stripped = strip_quoted(&full_text);
                excerpt = Some(if stripped.chars().count() > 240 {
                    let cut: String = stripped.chars().take(240).collect();
                    let head = cut.rsplit_once(' ').map(|(h, _)| h).unwrap_or(&cut);
                    format!("{head}\u{2026}")
                } else {
                    stripped
                });
            }
        }

        let from_recipient = from_email
            .as_deref()
            .map(|e| e.to_lowercase() == account.to_lowercase())
            .unwrap_or(false);

        messages.push(json!({
            "message_id": msg_id,
            "sender": from_name.clone().or(from_email.clone()).unwrap_or_else(|| "(unknown)".into()),
            "from_email": from_email,
            "subject": subject,
            "sent_at": sent_at.map(|t| iso(t.0)),
            "from_recipient": from_recipient,
            "excerpt": excerpt,
        }));
    }

    Ok(Some(json!({
        "message_id": message_id,
        "thread_id": thread_id,
        "messages": messages,
    })))
}

/// Small helper so `Item` construction in tests stays close to the Python kwargs.
#[allow(clippy::too_many_arguments)]
pub fn item(
    message_id: i64,
    account: &str,
    from_email: Option<&str>,
    from_name: Option<&str>,
    subject: Option<&str>,
    sent_at: Option<jiff::Timestamp>,
    unread: bool,
) -> Item {
    Item {
        message_id,
        account: account.to_string(),
        from_email: from_email.map(str::to_string),
        from_name: from_name.map(str::to_string),
        subject: subject.map(str::to_string),
        sent_at,
        unread,
        ..Default::default()
    }
}

pub use crate::home::reasons as generate_reasons;
