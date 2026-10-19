//! Read-only IMAP access, the port of `imap.py`. Messages are fetched with
//! BODY.PEEK so nothing is marked as read.
//!
//! The protocol subset the ingester needs is hand-rolled (like `s3.rs`):
//! LOGIN, EXAMINE, UID SEARCH, UID FETCH and LOGOUT, with IMAP literals on
//! both the send and receive side. TLS on port 993 goes through rustls with
//! the RustCrypto provider, so the static musl link stays C-compiler-free.
//! (Unlike imaplib, LOGIN arguments are sent as quoted strings, which servers
//! accept; imaplib sends them raw and breaks on spaces.)

use crate::accounts::Account;
use crate::ingest::{FetchedMessage, MailSource, SyncError};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct ImapError(pub String);

impl std::fmt::Display for ImapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ImapError {}

impl From<ImapError> for SyncError {
    fn from(e: ImapError) -> Self {
        SyncError::Imap(e.0)
    }
}

/// Anything we can talk IMAP over: plain TCP, or rustls-wrapped TCP.
trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

pub struct ImapSource {
    conn: Conn,
    next_tag: u32,
}

/// One untagged response: `meta` is the text of the line with literal
/// markers spliced out (the bytes imaplib hands to `ParseFlags` and the
/// INTERNALDATE regex), `literals` the raw bodies in order.
struct Untagged {
    meta: Vec<u8>,
    literals: Vec<Vec<u8>>,
}

/// Buffered connection: lines and exact byte counts out of a byte stream.
struct Conn {
    stream: Box<dyn Stream>,
    buf: Vec<u8>,
    pos: usize,
}

impl Conn {
    fn new(stream: Box<dyn Stream>) -> Conn {
        Conn { stream, buf: Vec::new(), pos: 0 }
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Ok(None); // EOF
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(byte))
    }

    /// One CRLF (or bare LF) line, without the terminator.
    fn read_line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            match self.read_byte()? {
                None => return Ok(line),
                Some(b'\n') => {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(line);
                }
                Some(byte) => line.push(byte),
            }
        }
    }

    /// Exactly n bytes, for IMAP literals.
    fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            match self.read_byte()? {
                None => break,
                Some(byte) => out.push(byte),
            }
        }
        Ok(out)
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }
}

impl ImapSource {
    /// Connect and log in over TLS (`IMAP4_SSL(...)` + `login` in Python).
    /// Both failures raise the same message, like the Python constructor.
    pub fn connect(account: &Account, timeout: Duration) -> Result<ImapSource, ImapError> {
        Self::connect_tls(&account.host, account.port as u16, &account.user, &account.password, timeout).map_err(|e| {
            ImapError(format!(
                "connect/login to {host}:{port} failed: {e}",
                host = account.host,
                port = account.port
            ))
        })
    }

    fn connect_tls(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<ImapSource, ImapError> {
        let tcp = connect_tcp(host, port, timeout)?;
        let provider = Arc::new(rustls_rustcrypto::provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| ImapError(format!("TLS: {e}")))?
            .with_root_certificates(root_store())
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| ImapError(format!("invalid host name {host:?}: {e}")))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| ImapError(format!("TLS: {e}")))?;
        let mut source = ImapSource { conn: Conn::new(Box::new(rustls::StreamOwned::new(conn, tcp))), next_tag: 1 };
        source.handshake_and_login(user, password)?;
        Ok(source)
    }

    /// Plain-text connection, only for tests against a local fake server.
    #[doc(hidden)]
    pub fn connect_insecure(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<ImapSource, ImapError> {
        let tcp = connect_tcp(host, port, timeout)?;
        let mut source = ImapSource { conn: Conn::new(Box::new(tcp)), next_tag: 1 };
        source.handshake_and_login(user, password)?;
        Ok(source)
    }

    /// Run the protocol over an arbitrary connected stream. Only for tests:
    /// the fake IMAP server is wired to the client through an in-memory
    /// pipe, which keeps the suite off the (unreliable here) loopback TCP.
    #[doc(hidden)]
    pub fn over_stream(stream: impl Read + Write + 'static, user: &str, password: &str) -> Result<ImapSource, ImapError> {
        let mut source = ImapSource { conn: Conn::new(Box::new(stream)), next_tag: 1 };
        source.handshake_and_login(user, password)?;
        Ok(source)
    }

    fn handshake_and_login(&mut self, user: &str, password: &str) -> Result<(), ImapError> {
        let (greeting, _) = self.read_item()?;
        if !greeting.starts_with(b"* ") {
            return Err(ImapError(format!("bad greeting: {}", String::from_utf8_lossy(&greeting))));
        }
        let (status, _) = self.command("LOGIN", &[Arg::str(user), Arg::str(password)])?;
        if status != "OK" {
            return Err(ImapError(format!("LOGIN failed: {status}")));
        }
        Ok(())
    }

    /// `select(mailbox, readonly=True)`: EXAMINE, and UIDVALIDITY from the
    /// untagged `* OK [UIDVALIDITY n]` response.
    pub fn select(&mut self, mailbox: &str) -> Result<i64, ImapError> {
        let (status, untagged) = self.command("EXAMINE", &[Arg::quoted_mailbox(mailbox)])?;
        if status != "OK" {
            return Err(ImapError(format!("cannot open mailbox '{mailbox}'")));
        }
        for u in &untagged {
            if let Some(n) = find_uidvalidity(&u.meta) {
                return Ok(n);
            }
        }
        Err(ImapError(format!("server did not report UIDVALIDITY for '{mailbox}'")))
    }

    /// Ascending UIDs greater than last_uid (last_uid=0 means all).
    pub fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, ImapError> {
        let criteria = if last_uid <= 0 { "ALL".to_string() } else { format!("UID {}:{}", last_uid + 1, '*') };
        let (status, untagged) = self.command("UID", &[Arg::word("SEARCH"), Arg::word(&criteria)])?;
        if status != "OK" {
            return Err(ImapError(format!("UID SEARCH {criteria} failed")));
        }
        let mut uids = Vec::new();
        for u in &untagged {
            if let Some(rest) = u.meta.strip_prefix(b"* SEARCH".as_slice()) {
                for token in String::from_utf8_lossy(rest).split_whitespace() {
                    if let Ok(n) = token.parse() {
                        uids.push(n);
                    }
                }
            }
        }
        // "N:*" always matches the highest UID, even when it is below N, so filter.
        uids.retain(|u| *u > last_uid);
        uids.sort_unstable();
        Ok(uids)
    }

    /// Full message, or None if it disappeared since the search.
    pub fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, ImapError> {
        let (status, untagged) =
            self.command("UID", &[Arg::word("FETCH"), Arg::word(&uid.to_string()), Arg::word("(FLAGS INTERNALDATE BODY.PEEK[])")])?;
        if status != "OK" {
            return Err(ImapError(format!("UID FETCH {uid} failed")));
        }
        let Some(u) = untagged.iter().find(|u| !u.literals.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(FetchedMessage {
            uid,
            raw: u.literals[0].clone(),
            flags: parse_flags(&u.meta),
            internal_date: find_internal_date(&u.meta).unwrap_or_default(),
        }))
    }

    pub fn close(&mut self) {
        // logout, suppressing any error (contextlib.suppress in Python)
        let _ = self.command("LOGOUT", &[]);
    }

    /// Run one command and collect its untagged responses until the tagged
    /// reply with our tag arrives. Returns ("OK"/"NO"/"BAD", untagged).
    fn command(&mut self, name: &str, args: &[Arg]) -> Result<(String, Vec<Untagged>), ImapError> {
        let tag = format!("A{:03}", self.next_tag);
        self.next_tag += 1;
        let mut line = format!("{tag} {name}");
        let mut literal: Option<&[u8]> = None;
        for arg in args {
            line.push(' ');
            match arg {
                Arg::Word(w) => line.push_str(w),
                Arg::Quoted(s) => line.push_str(&quote_string(s)),
                Arg::Literal(bytes) => {
                    let _ = write!(line, "{{{}}}", bytes.len());
                    literal = Some(bytes);
                }
            }
        }
        self.conn.send(line.as_bytes()).map_err(io_err)?;
        self.conn.send(b"\r\n").map_err(io_err)?;
        if let Some(bytes) = literal {
            self.expect_continuation()?;
            self.conn.send(bytes).map_err(io_err)?;
            self.conn.send(b"\r\n").map_err(io_err)?;
        }
        let mut untagged = Vec::new();
        loop {
            let (line, literals) = self.read_item()?;
            if line.starts_with(tag.as_bytes()) {
                let status = String::from_utf8_lossy(&line)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("BAD")
                    .to_string();
                return Ok((status, untagged));
            }
            untagged.push(Untagged { meta: line, literals });
        }
    }

    fn expect_continuation(&mut self) -> Result<(), ImapError> {
        let (line, _) = self.read_item()?;
        if !line.starts_with(b"+") {
            return Err(ImapError(format!(
                "expected command continuation, got: {}",
                String::from_utf8_lossy(&line)
            )));
        }
        Ok(())
    }

    /// Read one protocol "item": a line ending in `{n}` continues with n raw
    /// bytes and then the rest of the line. Returns the line with the literal
    /// markers spliced out, plus the literal bodies in order.
    fn read_item(&mut self) -> Result<(Vec<u8>, Vec<Vec<u8>>), ImapError> {
        let mut meta = Vec::new();
        let mut literals = Vec::new();
        loop {
            let line = self.conn.read_line().map_err(io_err)?;
            if line.is_empty() {
                // a bare EOF (no data); real servers never send empty lines
                return Err(ImapError("connection closed by server".into()));
            }
            match literal_marker(&line) {
                Some(size) => {
                    let marker = line.len() - size.to_string().len() - 2; // "{n"
                    meta.extend_from_slice(&line[..marker]);
                    literals.push(self.conn.read_bytes(size).map_err(io_err)?);
                    // continue with whatever follows the literal on this line
                }
                None => {
                    meta.extend_from_slice(&line);
                    return Ok((meta, literals));
                }
            }
        }
    }
}

impl MailSource for ImapSource {
    fn select(&mut self, mailbox: &str) -> Result<i64, SyncError> {
        ImapSource::select(self, mailbox).map_err(Into::into)
    }

    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError> {
        ImapSource::uids_after(self, last_uid).map_err(Into::into)
    }

    fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, SyncError> {
        ImapSource::fetch(self, uid).map_err(Into::into)
    }

    fn close(&mut self) {
        ImapSource::close(self)
    }
}

fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, ImapError> {
    let stream = TcpStream::connect((host, port)).map_err(io_err)?;
    stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
    // IMAP is request/response with small writes; Nagle only adds latency.
    stream.set_nodelay(true).map_err(io_err)?;
    Ok(stream)
}

fn io_err(e: io::Error) -> ImapError {
    ImapError(e.to_string())
}

fn root_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    store
}

enum Arg {
    Word(String),
    Quoted(String),
    /// sent as an IMAP literal (only for strings with control characters)
    Literal(Vec<u8>),
}

impl Arg {
    /// LOGIN credentials: quoted if clean, literal if they contain
    /// non-printable characters.
    fn str(s: &str) -> Arg {
        if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
            Arg::Literal(s.as_bytes().to_vec())
        } else {
            Arg::Quoted(s.to_string())
        }
    }

    fn quoted_mailbox(s: &str) -> Arg {
        Arg::Quoted(s.to_string())
    }

    fn word(s: &str) -> Arg {
        Arg::Word(s.to_string())
    }
}

/// Python `_quote`: wrap in double quotes, escaping \ and ".
fn quote_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// If the line ends with `{n}`, its size (the literal marker bytes are
/// `"{n"` at the end — the `}` is the last byte).
fn literal_marker(line: &[u8]) -> Option<usize> {
    if line.last() != Some(&b'}') {
        return None;
    }
    let open = line.iter().rposition(|b| *b == b'{')?;
    let digits = &line[open + 1..line.len() - 1];
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// `UIDVALIDITY` inside the `[...]` response code of an untagged OK.
fn find_uidvalidity(meta: &[u8]) -> Option<i64> {
    let marker = b"[UIDVALIDITY ";
    let at = meta.windows(marker.len()).position(|w| w == marker)?;
    let rest = &meta[at + marker.len()..];
    let end = rest.iter().position(|b| *b == b']')?;
    std::str::from_utf8(&rest[..end]).ok()?.trim().parse().ok()
}

/// The flags inside `FLAGS (...)` (`imaplib.ParseFlags`).
fn parse_flags(meta: &[u8]) -> Vec<String> {
    let marker = b"FLAGS (";
    let Some(at) = meta.windows(marker.len()).position(|w| w == marker) else {
        return Vec::new();
    };
    let rest = &meta[at + marker.len()..];
    let end = rest.iter().position(|b| *b == b')').unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).split_whitespace().map(str::to_string).collect()
}

/// The quoted `INTERNALDATE "..."` value, or None.
fn find_internal_date(meta: &[u8]) -> Option<String> {
    let marker = b"INTERNALDATE \"";
    let at = meta.windows(marker.len()).position(|w| w == marker)?;
    let rest = &meta[at + marker.len()..];
    let end = rest.iter().position(|b| *b == b'"')?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_marker_finds_sizes() {
        assert_eq!(literal_marker(b"* 1 FETCH (BODY[] {348}"), Some(348));
        assert_eq!(literal_marker(b"* 1 FETCH (BODY[] {0}"), Some(0));
        assert_eq!(literal_marker(b"* SEARCH 1 2"), None);
        assert_eq!(literal_marker(b"* OK [UIDVALIDITY 3] done"), None);
        assert_eq!(literal_marker(b"* 1 FETCH (FLAGS (\\Recent))"), None);
    }

    #[test]
    fn uidvalidity_from_untagged_ok() {
        assert_eq!(
            find_uidvalidity(b"* OK [UIDVALIDITY 1234] UIDs valid"),
            Some(1234)
        );
        assert_eq!(find_uidvalidity(b"* OK [PERMANENTFLAGS ()] Limitation"), None);
    }

    #[test]
    fn flags_and_internaldate_are_scanned() {
        let meta = b"* 1 FETCH (FLAGS (\\Seen \\Flagged) INTERNALDATE \"01-Jan-2026 10:00:00 +0000\" BODY[] {5}".to_vec();
        assert_eq!(parse_flags(&meta), vec!["\\Seen".to_string(), "\\Flagged".to_string()]);
        assert_eq!(
            find_internal_date(&meta).as_deref(),
            Some("01-Jan-2026 10:00:00 +0000")
        );
        assert_eq!(parse_flags(b"* 1 FETCH (UID 1)"), Vec::<String>::new());
    }

    #[test]
    fn quoting_escapes_like_python() {
        assert_eq!(quote_string("INBOX"), "\"INBOX\"");
        assert_eq!(quote_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_string("a\\b"), "\"a\\\\b\"");
    }
}
