//! Port of the pure parts of `src/aimap/draft.py`: draft reply generation using
//! an OpenAI-compatible chat completions endpoint.
//!
//! The draft reads the thread (newest first), caps the context to a sane
//! character limit, and calls the model with a prompt that produces a
//! plain-text reply body. The HTTP call goes through the [`ChatTransport`]
//! trait so tests can fake the model exactly like the Python tests patch
//! `requests.post`; the DB-touching orchestration in Python's
//! `generate_draft` (get_message / get_thread) arrives with the db port.

use serde_json::{json, Value};

use crate::inbox;
use crate::pgtime::iso;
use crate::storage::Store;

pub const MAX_THREAD_CHARS: usize = 12000;
pub const MAX_THREAD_MESSAGES: usize = 6;

/// Draft error taxonomy (`DraftError` + subclasses in draft.py).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftError {
    MessageNotFound,
    InstructionsTooLong,
    DraftingNotConfigured,
    ModelTimeout,
    ModelRequestFailed(String),
}

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DraftError::MessageNotFound => write!(f, "no such message"),
            DraftError::InstructionsTooLong => write!(f, "instructions too long"),
            DraftError::DraftingNotConfigured => write!(f, "Drafting is not configured."),
            DraftError::ModelTimeout => write!(f, "The model timed out."),
            DraftError::ModelRequestFailed(m) => write!(f, "{}", m),
        }
    }
}
impl std::error::Error for DraftError {}

impl DraftError {
    /// The message the Python code puts in the 503/502 API responses.
    pub fn api_detail(&self) -> &str {
        match self {
            DraftError::MessageNotFound => "no such message",
            DraftError::InstructionsTooLong => "instructions too long",
            DraftError::DraftingNotConfigured => "Drafting is not configured.",
            DraftError::ModelTimeout => "The model timed out.",
            DraftError::ModelRequestFailed(m) => m,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout_secs: u64,
    pub max_tokens: u32,
    /// reasoning models burn the token budget on chain-of-thought otherwise
    pub reasoning_effort: String,
}

impl DraftConfig {
    pub fn is_configured(&self) -> bool {
        self.api_key.is_some() && !self.model.is_empty()
    }
}

/// One thread message as `build_prompt` and the capping loop see it
/// (the dict rows `get_thread` returns in Python).
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadMessage {
    pub message_id: i64,
    pub sender: Option<String>,
    pub excerpt: Option<String>,
}

/// The chat completions request `requests.post` would send, as pure data.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCall {
    pub url: String,
    pub authorization: String,
    pub payload: Value,
    pub timeout_secs: u64,
}

/// Transport errors, mapped like requests.Timeout / requests.RequestException.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    Failed(String),
}

pub trait ChatTransport {
    fn post(&self, call: &ModelCall) -> Result<Value, TransportError>;
}

/// Build the prompt from thread messages (newest first) and optional
/// instructions. Asks for a plain-text reply body only, no quoted original,
/// no subject line inside the body.
pub fn build_prompt(thread_messages: &[ThreadMessage], instructions: Option<&str>) -> String {
    let context_parts: Vec<String> = thread_messages
        .iter()
        .map(|msg| {
            let sender = msg.sender.clone().unwrap_or_else(|| "(unknown)".to_string());
            let excerpt = match msg.excerpt.as_deref() {
                Some(e) if !e.is_empty() => e.to_string(),
                _ => "(no body)".to_string(),
            };
            format!("From: {}\n{}\n", sender, excerpt)
        })
        .collect();
    let context = context_parts.join("\n---\n\n");

    let instructions_text = match instructions {
        Some(i) => format!("\n\nUser instructions: {}", i),
        None => String::new(),
    };

    format!(
        "You are drafting a reply to this email thread (newest message first):\n\n\
{context}\n\n\
---\n\n\
Draft a plain-text reply. Do not include quoted text, do not include a subject line. Just the body of the reply.\
{instructions_text}"
    )
}

/// `len(instructions) > 2000` gate (counted in code points, like Python).
pub fn validate_instructions(instructions: Option<&str>) -> Result<(), DraftError> {
    match instructions {
        Some(i) if i.chars().count() > 2000 => Err(DraftError::InstructionsTooLong),
        _ => Ok(()),
    }
}

/// The chat completions request body and URL for a prompt.
pub fn plan_model_call(config: &DraftConfig, prompt: &str) -> ModelCall {
    ModelCall {
        url: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
        authorization: format!("Bearer {}", config.api_key.clone().unwrap_or_default()),
        payload: json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": config.max_tokens,
            "temperature": 0.7,
            "reasoning_effort": config.reasoning_effort,
        }),
        timeout_secs: config.timeout_secs,
    }
}

/// `data["choices"][0]["message"]["content"].strip()`, with empty bodies
/// rejected the way generate_draft does.
pub fn extract_reply(data: &Value) -> Result<String, DraftError> {
    let content = data
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            DraftError::ModelRequestFailed("The model response was invalid.".to_string())
        })?;
    let body = content.trim().to_string();
    if body.is_empty() {
        return Err(DraftError::ModelRequestFailed(
            "The model returned an empty reply.".to_string(),
        ));
    }
    Ok(body)
}

/// The capping loop: keep newest messages until MAX_THREAD_CHARS would be
/// exceeded; fall back to the newest single message if even that is too long.
/// Returns the capped messages and the ids that were used.
pub fn cap_thread(messages: &[ThreadMessage]) -> (Vec<ThreadMessage>, Vec<i64>) {
    let head: Vec<ThreadMessage> = messages.iter().take(MAX_THREAD_MESSAGES).cloned().collect();
    let mut capped: Vec<ThreadMessage> = Vec::new();
    let mut used_ids: Vec<i64> = Vec::new();
    let mut total_chars = 0usize;
    for msg in &head {
        let excerpt = msg.excerpt.clone().unwrap_or_default();
        let len = excerpt.chars().count();
        if total_chars + len > MAX_THREAD_CHARS {
            break;
        }
        used_ids.push(msg.message_id);
        total_chars += len;
        capped.push(msg.clone());
    }
    if capped.is_empty() {
        if let Some(first) = head.first() {
            used_ids = vec![first.message_id];
            capped = vec![first.clone()];
        }
    }
    (capped, used_ids)
}

/// `RE:` subject derivation: never doubled, `RE:` alone for empty subjects.
pub fn reply_subject(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or("");
    if !subject.is_empty() && !crate::parse::py_trim(&subject.to_lowercase()).starts_with("re:") {
        format!("RE: {}", subject)
    } else if subject.is_empty() {
        "RE:".to_string()
    } else {
        subject.to_string()
    }
}

/// Errors from the full `generate_draft` orchestration: DB and S3 failures
/// surface separately, like psycopg errors and S3 exceptions do in Python.
#[derive(Debug)]
pub enum GenerateError {
    Db(postgres::Error),
    S3(crate::storage::S3Error),
    Draft(DraftError),
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerateError::Db(e) => write!(f, "{e}"),
            GenerateError::S3(e) => write!(f, "s3 error: {}", e.code),
            GenerateError::Draft(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for GenerateError {}

impl From<crate::inbox::InboxError> for GenerateError {
    fn from(e: crate::inbox::InboxError) -> Self {
        match e {
            crate::inbox::InboxError::Postgres(p) => GenerateError::Db(p),
            crate::inbox::InboxError::S3(s) => GenerateError::S3(s),
            crate::inbox::InboxError::Cursor(c) => GenerateError::Draft(DraftError::ModelRequestFailed(format!("invalid cursor: {c}"))),
        }
    }
}

impl From<DraftError> for GenerateError {
    fn from(e: DraftError) -> Self {
        GenerateError::Draft(e)
    }
}

/// The full `generate_draft`: message metadata and thread from Postgres,
/// excerpts from S3, prompt, model call, and the draft the API returns.
/// `now` is filled in as `created_at` by the caller's clock.
pub fn generate_draft<C: postgres::GenericClient, S: crate::storage::S3Client, T: ChatTransport>(
    conn: &mut C,
    store: &Store<S>,
    transport: &T,
    message_id: i64,
    instructions: Option<&str>,
    config: &DraftConfig,
    now: jiff::Timestamp,
) -> Result<Value, GenerateError> {
    if !config.is_configured() {
        return Err(DraftError::DraftingNotConfigured.into());
    }
    validate_instructions(instructions)?;

    let msg = inbox::get_message(conn, message_id).map_err(GenerateError::Db)?;
    let Some(msg) = msg else {
        return Err(DraftError::MessageNotFound.into());
    };
    let needs_reply = msg["reply"]["needed"].as_bool().unwrap_or(false);
    let reply_reason = msg["reply"]["reason"].as_str().map(str::to_string);

    let thread_data = inbox::get_thread(conn, message_id, store).map_err(GenerateError::from)?;
    let Some(thread_data) = thread_data else {
        return Err(DraftError::MessageNotFound.into());
    };
    let thread: Vec<ThreadMessage> = thread_data["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .map(|m| ThreadMessage {
                    message_id: m["message_id"].as_i64().unwrap_or(0),
                    sender: m["sender"].as_str().map(str::to_string),
                    excerpt: m["excerpt"].as_str().map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();

    let (capped, used_ids) = cap_thread(&thread);
    if capped.is_empty() {
        return Err(DraftError::MessageNotFound.into());
    }
    let prompt = build_prompt(&capped, instructions);

    let call = plan_model_call(config, &prompt);
    let data = transport.post(&call).map_err(|e| match e {
        TransportError::Timeout => DraftError::ModelTimeout,
        TransportError::Failed(_) => {
            DraftError::ModelRequestFailed("The model request failed.".to_string())
        }
    })?;
    let body = extract_reply(&data)?;

    let subject = msg["subject"].as_str().map(str::to_string);
    let to: Vec<&str> = msg["from_email"].as_str().map(|s| vec![s]).unwrap_or_default();
    Ok(json!({
        "message_id": message_id,
        "needs_reply": needs_reply,
        "reply_reason": reply_reason,
        "to": to,
        "cc": [],
        "subject": reply_subject(subject.as_deref()),
        "body": body,
        "model": config.model,
        "used_message_ids": used_ids,
        "created_at": iso(now),
    }))
}

/// The non-DB part of `generate_draft`: given the message fields and its
/// thread, call the model and assemble the draft the API returns.
///
/// `message` is the row `get_message` returned (subject, from_email, reply
/// info); `thread` is `get_thread`'s newest-first message list.
#[allow(clippy::too_many_arguments)]
pub fn complete_draft(
    config: &DraftConfig,
    transport: &dyn ChatTransport,
    message_id: i64,
    instructions: Option<&str>,
    message_subject: Option<&str>,
    message_from_email: Option<&str>,
    needs_reply: bool,
    reply_reason: Option<&str>,
    thread: &[ThreadMessage],
) -> Result<Value, DraftError> {
    if !config.is_configured() {
        return Err(DraftError::DraftingNotConfigured);
    }
    validate_instructions(instructions)?;

    let (capped, used_ids) = cap_thread(thread);
    if capped.is_empty() {
        return Err(DraftError::MessageNotFound);
    }
    let prompt = build_prompt(&capped, instructions);

    let call = plan_model_call(config, &prompt);
    let data = transport.post(&call).map_err(|e| match e {
        TransportError::Timeout => DraftError::ModelTimeout,
        TransportError::Failed(_) => {
            DraftError::ModelRequestFailed("The model request failed.".to_string())
        }
    })?;
    let body = extract_reply(&data)?;

    let to: Vec<&str> = message_from_email.into_iter().collect();
    Ok(json!({
        "message_id": message_id,
        "needs_reply": needs_reply,
        "reply_reason": reply_reason,
        "to": to,
        "cc": [],
        "subject": reply_subject(message_subject),
        "body": body,
        "model": config.model,
        "used_message_ids": used_ids,
        // "created_at" is filled by the caller (datetime.now / jiff now())
    }))
}
