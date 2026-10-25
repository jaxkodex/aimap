//! The HTTP API for the app: `aimap api`. Port of `src/aimap/api.py`, on a
//! small threaded HTTP/1.1 server of its own instead of FastAPI/uvicorn (the
//! route surface is ten JSON endpoints; the sync Postgres pool drives all of
//! them, and a hand-rolled server keeps the tree free of an async runtime).
//!
//! The one write is aimap's own message state (handled, later, undo). The
//! mailbox, the bucket and the labels are never changed; IMAP stays
//! read-only.
//!
//! Every route except /healthz needs a Firebase ID token (see `auth.rs`).
//! [`handle`] is the whole routing logic and is driven directly by the tests
//! (FastAPI's TestClient role); `serve` is the thin socket loop around it.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::{User, VerifyError};
use crate::db::Pool;
use crate::draft;
use crate::home;
use crate::inbox::{self, InboxError};
use crate::logs::{LogValue, Logger};
use crate::parse;
use crate::storage::{S3Client, Store};

pub const BODY_CHARS: usize = 50_000;
/// Refuse absurd request bodies instead of reading them.
const MAX_BODY: usize = 1024 * 1024;

/// The verifier role (`auth.Verifier`); production wraps `FirebaseVerifier`.
pub trait Verifier: Send + Sync {
    fn verify(&self, token: &str) -> Result<User, VerifyError>;
}

impl<F: crate::auth::Fetcher> Verifier for crate::auth::FirebaseVerifier<F> {
    fn verify(&self, token: &str) -> Result<User, VerifyError> {
        crate::auth::FirebaseVerifier::verify(self, token)
    }
}

pub struct AppState<C: S3Client> {
    pub pool: Pool,
    pub store: Store<C>,
    pub verifier: Arc<dyn Verifier>,
    pub draft_config: Option<draft::DraftConfig>,
    pub draft_transport: Arc<dyn draft::ChatTransport>,
    pub log: Logger,
}

/// What `handle` returns: status, JSON body, and any extra headers
/// (WWW-Authenticate).
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
    pub headers: Vec<(String, String)>,
}

impl ApiResponse {
    fn ok(body: Value) -> ApiResponse {
        ApiResponse { status: 200, body, headers: vec![] }
    }
    fn error(status: u16, detail: &str) -> ApiResponse {
        ApiResponse { status, body: json!({"detail": detail}), headers: vec![] }
    }
    fn unauthorized(detail: &str) -> ApiResponse {
        ApiResponse {
            status: 401,
            body: json!({"detail": detail}),
            headers: vec![("WWW-Authenticate".to_string(), "Bearer".to_string())],
        }
    }
}

/// The 401 detail FastAPI raises for a missing bearer token.
const MISSING_TOKEN: &str = "missing bearer token";
const INVALID_TOKEN: &str = "invalid token";

fn current_user<C: S3Client>(state: &AppState<C>, headers: &[(String, String)]) -> Result<User, ApiResponse> {
    let value = header(headers, "authorization").unwrap_or("");
    let (scheme, _, token) = match value.split_once(' ') {
        Some((scheme, rest)) => (scheme, ' ', rest),
        None => (value, ' ', ""),
    };
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(ApiResponse::unauthorized(MISSING_TOKEN));
    }
    match state.verifier.verify(token.trim()) {
        Ok(user) => Ok(user),
        Err(VerifyError::Forbidden(e)) => {
            state.log.warning("api access denied", &[("reason", LogValue::str(e.0))]);
            Err(ApiResponse::error(403, "not allowed"))
        }
        Err(VerifyError::Auth(_)) => Err(ApiResponse::unauthorized(INVALID_TOKEN)),
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// One request handed to the router. `query` is the raw query string (no
/// leading '?').
pub fn handle<C: S3Client>(
    state: &AppState<C>,
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> ApiResponse {
    let path = percent_decode_path(path);
    match (method, path.as_str()) {
        ("GET", "/healthz") => {
            let result = state.pool.get().and_then(|mut conn| conn.batch_execute("SELECT 1"));
            match result {
                Ok(()) => ApiResponse::ok(json!({"ok": true})),
                Err(e) => {
                    state.log.error("database error", &[("error", LogValue::str(e.to_string()))]);
                    ApiResponse::error(500, "Internal Server Error")
                }
            }
        }
        ("GET", "/me") => match current_user(state, headers) {
            Ok(user) => ApiResponse::ok(json!({"uid": user.uid, "email": user.email})),
            Err(resp) => resp,
        },
        ("GET", "/accounts") => match current_user(state, headers) {
            Ok(_) => {
                let mut conn = match state.pool.get() {
                    Ok(conn) => conn,
                    Err(e) => return internal(state, e),
                };
                match inbox::accounts(&mut *conn) {
                    Ok(accounts) => ApiResponse::ok(json!({"accounts": accounts})),
                    Err(e) => internal(state, e),
                }
            }
            Err(resp) => resp,
        },
        ("GET", "/home") => match current_user(state, headers) {
            Ok(_) => home_screen(state, query),
            Err(resp) => resp,
        },
        ("GET", "/messages") => match current_user(state, headers) {
            Ok(_) => messages(state, query),
            Err(resp) => resp,
        },
        ("GET", p) if p.starts_with("/messages/") && p.ends_with("/body") => {
            let trimmed = p.trim_end_matches("/body");
            match message_id(trimmed) {
                Ok(message_id) => match current_user(state, headers) {
                    Ok(_) => message_body(state, message_id),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            }
        }
        ("GET", p) if p.starts_with("/messages/") && p.ends_with("/thread") => {
            let trimmed = p.trim_end_matches("/thread");
            match message_id(trimmed) {
                Ok(message_id) => match current_user(state, headers) {
                    Ok(_) => message_thread(state, message_id),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            }
        }
        ("GET", p) if p.starts_with("/messages/") => match message_id(p) {
            Ok(message_id) => match current_user(state, headers) {
                Ok(_) => message_detail(state, message_id),
                Err(resp) => resp,
            },
            Err(resp) => resp,
        },
        ("POST", p) if p.starts_with("/messages/") && p.ends_with("/actions") => {
            let trimmed = p.trim_end_matches("/actions");
            match message_id(trimmed) {
                Ok(message_id) => match current_user(state, headers) {
                    Ok(user) => message_action(state, message_id, body, &user),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            }
        }
        ("POST", p) if p.starts_with("/messages/") && p.ends_with("/draft") => {
            let trimmed = p.trim_end_matches("/draft");
            match message_id(trimmed) {
                Ok(message_id) => match current_user(state, headers) {
                    Ok(_) => create_draft(state, message_id, body),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            }
        }
        // a known path with the wrong method, like FastAPI's 405
        (_, "/healthz" | "/me" | "/accounts" | "/home" | "/messages") => {
            ApiResponse::error(405, "Method Not Allowed")
        }
        (_, p)
            if p.starts_with("/messages/")
                && (message_id(p).is_ok()
                    || p.ends_with("/actions")
                    || p.ends_with("/body")
                    || p.ends_with("/thread")
                    || p.ends_with("/draft")) =>
        {
            ApiResponse::error(405, "Method Not Allowed")
        }
        _ => ApiResponse::error(404, "Not Found"),
    }
}

fn internal<C: S3Client>(state: &AppState<C>, e: impl std::fmt::Debug) -> ApiResponse {
    state.log.error("database error", &[("error", LogValue::str(format!("{e:?}")))]);
    ApiResponse::error(500, "Internal Server Error")
}

/// `/messages/{id}` and its suffixes; a non-integer id is FastAPI's 422.
fn message_id(path: &str) -> Result<i64, ApiResponse> {
    let id = path.trim_start_matches("/messages/");
    id.parse::<i64>()
        .map_err(|_| ApiResponse::error(422, "value is not a valid integer"))
}

/// Query params: `parse_qsl` semantics — `&`-separated, `+` is a space,
/// percent-decoded. Repeated keys keep the first value, like
/// `request.query_params.get` in FastAPI.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').filter(|p| !p.is_empty()).find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode_query(k) == name {
            Some(percent_decode_query(v))
        } else {
            None
        }
    })
}

/// Paths never treat `+` as a space; queries do (form encoding).
fn percent_decode_path(path: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(path.as_bytes(), false)).into_owned()
}

fn percent_decode_query(text: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(text.as_bytes(), true)).into_owned()
}

fn percent_decode_bytes(bytes: &[u8], plus_as_space: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn home_screen<C: S3Client>(state: &AppState<C>, query: &str) -> ApiResponse {
    // days: int 1..=90, default 7; anything else is FastAPI's 422
    let days = match query_param(query, "days") {
        Some(text) => match text.parse::<i64>() {
            Ok(n) if (1..=90).contains(&n) => n,
            _ => return ApiResponse::error(422, "value is not a valid integer"),
        },
        None => 7,
    };
    let account = query_param(query, "account");
    // new_since: an RFC 3339 timestamp, optional
    let new_since = match query_param(query, "new_since") {
        Some(text) => match text.parse::<jiff::Timestamp>() {
            Ok(ts) => ts,
            Err(_) => return ApiResponse::error(422, "invalid datetime"),
        },
        None => jiff::Timestamp::now()
            .checked_sub(jiff::Span::new().hours(24))
            .unwrap_or_default(),
    };
    // tz: an IANA time zone name; it decides which hours brief.by_hour calls today
    let tz_name = query_param(query, "tz").unwrap_or_else(|| "UTC".to_string());
    let zone = match jiff::tz::TimeZone::get(&tz_name) {
        Ok(zone) => zone,
        Err(_) => return ApiResponse::error(400, &format!("unknown time zone: {tz_name}")),
    };
    let now = jiff::Timestamp::now();
    // timedelta(days=days): uniform 24-hour units (Timestamp arithmetic has
    // no calendar units in jiff, and Python's days are 24 hours here too).
    let midnight = now
        .to_zoned(zone.clone())
        .with()
        .hour(0)
        .minute(0)
        .second(0)
        .subsec_nanosecond(0)
        .build()
        .map(|z| z.timestamp())
        .unwrap_or(now);
    let since = match now.checked_sub(jiff::Span::new().hours(days as i32 * 24)) {
        Ok(ts) => ts,
        Err(_) => return ApiResponse::error(422, "invalid days"),
    };
    let mut conn = match state.pool.get() {
        Ok(conn) => conn,
        Err(e) => return internal(state, e),
    };
    let items = match inbox::home_items(&mut *conn, since, account.as_deref()) {
        Ok(items) => items,
        Err(e) => return internal(state, e),
    };
    let last = match inbox::sorted_at(&mut *conn, account.as_deref()) {
        Ok(last) => last,
        Err(e) => return internal(state, e),
    };
    let handled_today = match inbox::handled_since(&mut *conn, midnight, account.as_deref()) {
        Ok(n) => n,
        Err(e) => return internal(state, e),
    };
    ApiResponse::ok(home::build(&items, new_since, now, &zone, last, handled_today))
}

fn messages<C: S3Client>(state: &AppState<C>, query: &str) -> ApiResponse {
    let account = query_param(query, "account");
    let filter = query_param(query, "filter").unwrap_or_else(|| "all".to_string());
    if !matches!(filter.as_str(), "all" | "unread" | "flagged" | "needs_review") {
        return ApiResponse::error(422, "unexpected value");
    }
    let cursor = query_param(query, "cursor").filter(|c| !c.is_empty());
    let limit = match query_param(query, "limit") {
        Some(text) => match text.parse::<i64>() {
            Ok(n) if (1..=200).contains(&n) => n,
            _ => return ApiResponse::error(422, "value is not a valid integer"),
        },
        None => 50,
    };
    let mut conn = match state.pool.get() {
        Ok(conn) => conn,
        Err(e) => return internal(state, e),
    };
    let result = inbox::list_messages(&mut *conn, account.as_deref(), &filter, cursor.as_deref(), limit);
    match result {
        Ok((items, next_cursor)) => {
            ApiResponse::ok(json!({"messages": items, "next_cursor": next_cursor}))
        }
        Err(InboxError::Cursor(e)) => ApiResponse::error(400, &e.0),
        Err(e) => internal(state, e),
    }
}

/// The message row or the 404 the handlers share.
fn _message<C: S3Client>(state: &AppState<C>, message_id: i64) -> Result<Value, ApiResponse> {
    let mut conn = state.pool.get().map_err(|e| internal(state, e))?;
    match inbox::get_message(&mut *conn, message_id) {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Err(ApiResponse::error(404, "no such message")),
        Err(e) => Err(internal(state, e)),
    }
}

fn message_detail<C: S3Client>(state: &AppState<C>, message_id: i64) -> ApiResponse {
    let mut m = match _message(state, message_id) {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    if let Some(obj) = m.as_object_mut() {
        obj.remove("s3_key");
        if let Some(labels) = obj.get_mut("labels").filter(|l| !l.is_null()).and_then(Value::as_object_mut) {
            let signals = labels.get("signals").cloned().unwrap_or_else(|| json!({}));
            labels.insert("reasons".into(), json!(home::reasons(&signals)));
        }
    }
    ApiResponse::ok(m)
}

fn message_action<C: S3Client>(state: &AppState<C>, message_id: i64, body: &[u8], user: &User) -> ApiResponse {
    // {"action": "handled" | "later" | "undo"}; anything else is 422
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return ApiResponse::error(422, "Input should be a valid dictionary"),
    };
    let action = match parsed.get("action").and_then(Value::as_str) {
        Some(a) if matches!(a, "handled" | "later" | "undo") => a,
        _ => return ApiResponse::error(422, "Input should be 'handled', 'later' or 'undo'"),
    };
    let mut conn = match state.pool.get() {
        Ok(conn) => conn,
        Err(e) => return internal(state, e),
    };
    let state_value = if action == "undo" { None } else { Some(action) };
    let result = inbox::set_state(&mut *conn, message_id, state_value, &user.email);
    match result {
        Ok(Some(state)) => ApiResponse::ok(state),
        Ok(None) => ApiResponse::error(404, "no such message"),
        Err(e) => internal(state, e),
    }
}

fn message_body<C: S3Client>(state: &AppState<C>, message_id: i64) -> ApiResponse {
    let key = match _message(state, message_id) {
        Ok(m) => m.get("s3_key").and_then(Value::as_str).map(str::to_string),
        Err(resp) => return resp,
    };
    // The text body, read from S3 for this request only. Nothing is cached.
    let got = match key {
        Some(key) if !key.is_empty() => state.store.get_raw(&key),
        _ => Ok(None),
    };
    let (raw, _) = match got {
        Ok(Some(raw)) => raw,
        Ok(None) => return ApiResponse::error(404, "message body is not in the bucket"),
        Err(e) => return internal(state, e),
    };
    ApiResponse::ok(json!({"message_id": message_id, "text": parse::body_text(&raw, BODY_CHARS)}))
}

fn message_thread<C: S3Client>(state: &AppState<C>, message_id: i64) -> ApiResponse {
    let mut conn = match state.pool.get() {
        Ok(conn) => conn,
        Err(e) => return internal(state, e),
    };
    let result = inbox::get_thread(&mut *conn, message_id, &state.store);
    match result {
        Ok(Some(thread)) => ApiResponse::ok(thread),
        Ok(None) => ApiResponse::error(404, "no such message"),
        Err(InboxError::Cursor(e)) => ApiResponse::error(400, &e.0),
        Err(e) => internal(state, e),
    }
}

fn create_draft<C: S3Client>(state: &AppState<C>, message_id: i64, body: &[u8]) -> ApiResponse {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return ApiResponse::error(422, "Input should be a valid dictionary"),
    };
    let instructions: Option<String> = match parsed.get("instructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()), // "" stays "", like Python
        Some(_) => return ApiResponse::error(422, "Input should be a valid string"),
    };
    let instructions = instructions.as_deref();
    // Like Python: with no explicit config the endpoint drafts with the
    // default budget, which fails with 503 until an API key is configured.
    let default_config = || draft::DraftConfig {
        base_url: "https://api.deepseek.com".into(),
        api_key: None,
        model: "deepseek-flash".into(),
        timeout_secs: 45,
        max_tokens: 700,
        reasoning_effort: "low".into(),
    };
    let config = state.draft_config.clone().unwrap_or_else(default_config);
    let mut conn = match state.pool.get() {
        Ok(conn) => conn,
        Err(e) => return internal(state, e),
    };
    let result = draft::generate_draft(
        &mut *conn,
        &state.store,
        &*state.draft_transport,
        message_id,
        instructions,
        &config,
        jiff::Timestamp::now(),
        &crate::logs::Logger { name: "aimap.draft".into(), ..state.log.clone() },
    );
    match result {
        Ok(draft) => ApiResponse::ok(draft),
        Err(draft::GenerateError::Draft(e)) => match e {
            draft::DraftError::MessageNotFound => ApiResponse::error(404, e.api_detail()),
            draft::DraftError::InstructionsTooLong => ApiResponse::error(422, e.api_detail()),
            draft::DraftError::DraftingNotConfigured => ApiResponse::error(503, e.api_detail()),
            draft::DraftError::ModelTimeout | draft::DraftError::ModelRequestFailed(_) => {
                ApiResponse::error(502, e.api_detail())
            }
        },
        Err(e) => internal(state, e),
    }
}

// --------------------------------------------------------------------------------------
// the socket loop
// --------------------------------------------------------------------------------------

/// Serve requests until the process is killed. One thread per connection,
/// keep-alive unless the client says otherwise; access lines go to the log.
pub fn serve<C: S3Client + Send + Sync + 'static>(
    state: Arc<AppState<C>>,
    host: &str,
    port: u16,
) -> std::io::Result<()> {
    let listener = TcpListener::bind((host, port))?;
    serve_listener(listener, state)
}

/// `serve` on an already-bound listener (tests bind port 0 and read back the
/// port).
pub fn serve_listener<C: S3Client + Send + Sync + 'static>(
    listener: TcpListener,
    state: Arc<AppState<C>>,
) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let _ = connection(state, stream);
        });
    }
    Ok(())
}

fn connection<C: S3Client + Send + Sync + 'static>(
    state: Arc<AppState<C>>,
    mut stream: TcpStream,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let peer = stream.peer_addr().ok();
    loop {
        let request = match read_request(&mut stream)? {
            Some(request) => request,
            None => return Ok(()), // clean EOF between requests
        };
        let started = Instant::now();
        // FastAPI keeps the app name out of the redirect; no redirects here,
        // so no special handling. `Expect: 100-continue` gets its interim.
        if request
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("expect") && v.to_ascii_lowercase().contains("100-continue"))
        {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        let keep_alive = request.keep_alive;
        let resp = handle(
            &state,
            &request.method,
            &request.path,
            &request.query,
            &request.headers,
            &request.body,
        );
        state.log.info(
            "request",
            &[
                ("method", LogValue::Str(request.method.clone())),
                ("path", LogValue::Str(request.raw_path.clone())),
                ("status", LogValue::Int(resp.status as i64)),
                ("ms", LogValue::Int(started.elapsed().as_millis() as i64)),
                ("remote", LogValue::Str(
                    header(&request.headers, "x-forwarded-for").map(str::to_string).or_else(|| peer.map(|p| p.to_string())).unwrap_or_default(),
                )),
            ],
        );
        let body = serde_json::to_vec(&resp.body).unwrap_or_else(|_| b"{}".to_vec());
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            resp.status,
            reason_phrase(resp.status),
            body.len()
        );
        for (k, v) in &resp.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(if keep_alive { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
        stream.write_all(head.as_bytes())?;
        stream.write_all(&body)?;
        stream.flush()?;
        if !keep_alive {
            return Ok(());
        }
    }
}

struct Incoming {
    method: String,
    /// percent-decoded path (query split off)
    path: String,
    raw_path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    keep_alive: bool,
}

/// Read one request; None on a clean EOF before any byte (the keep-alive
/// close). Truncated requests simply end the connection, like uvicorn's
/// h11 behavior on bad input.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Incoming>> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8 * 1024];
    let head_len = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(None); // head too large: drop the connection
        }
        let n = match stream.read(&mut chunk) {
            Ok(0) => {
                return Ok(None); // clean close between requests
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // idle keep-alive connections time out on the 30s read timeout
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_len]).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 request head")
    })?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Ok(None); // refuse by dropping; nothing here expects large bodies
    }
    let mut body = buf[head_len..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let keep_alive = match header(&headers, "connection").map(|v| v.to_ascii_lowercase()) {
        Some(v) if v.contains("close") => false,
        Some(v) if v.contains("keep-alive") => true,
        // HTTP/1.0 defaults to close unless the header asks otherwise
        _ => version.eq_ignore_ascii_case("HTTP/1.1"),
    };
    Ok(Some(Incoming {
        method,
        path: percent_decode_path(&raw_path),
        raw_path,
        query,
        headers,
        body,
        keep_alive,
    }))
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}
