//! A minimal blocking HTTP/1.1 client, plain and TLS (`https` via rustls with
//! the RustCrypto provider, like `imap.rs`).
//!
//! ureq's `tls` feature hard-depends on `ring` (C/asm), which the static musl
//! build forbids, so the handful of HTTPS calls the API batch needs — Google's
//! Firebase certificates, the TypeSafe classifier and the draft chat
//! completions endpoint — go through this instead. One connection per request
//! (`Connection: close`); every caller here sends exactly one request and
//! reads one response.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Limits keeping a hostile or broken server from exhausting memory.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The server did not answer within the timeout.
    Timeout,
    Connect(String),
    Tls(String),
    /// Malformed HTTP (truncated body, bad chunk framing, oversized …).
    Protocol(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Timeout => write!(f, "request timed out"),
            HttpError::Connect(m) | HttpError::Tls(m) | HttpError::Protocol(m) => write!(f, "{m}"),
        }
    }
}

/// A parsed URL: scheme, host, port, and the path with query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// path + query, "/" when the URL has neither
    pub path: String,
}

pub fn parse_url(url: &str) -> Result<Url, HttpError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| HttpError::Protocol(format!("URL has no scheme: {url}")))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(HttpError::Protocol(format!("unsupported URL scheme: {scheme}")));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    // [user[:pass]@]host[:port]; userinfo is never used here but strip it.
    let authority = authority.split_once('@').map(|(_, a)| a).unwrap_or(authority);
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| HttpError::Protocol(format!("unclosed IPv6 host: {authority}")))?;
        let port = after
            .strip_prefix(':')
            .map(|p| p.parse::<u16>())
            .transpose()
            .map_err(|_| HttpError::Protocol(format!("bad port: {authority}")))?;
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                Some(p.parse::<u16>())
                    .transpose()
                    .map_err(|_| HttpError::Protocol(format!("bad port: {authority}")))?,
            ),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err(HttpError::Protocol(format!("URL has no host: {url}")));
    }
    let default = if scheme == "https" { 443 } else { 80 };
    Ok(Url { scheme, port: port.unwrap_or(default), host, path })
}

/// One request. `headers` are written in order after the defaults; a header
/// already present is not overwritten.
pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body: Option<&'a [u8]>,
    pub timeout: Duration,
}

/// An HTTP response: status code, headers in arrival order, decoded body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// Case-insensitive header lookup, first match.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The exact request bytes: request line, Host, Content-Length, caller
/// headers, `Connection: close`, blank line, body.
pub fn build_request_bytes(req: &Request<'_>, url: &Url) -> Vec<u8> {
    let mut out = Vec::new();
    let host_header = if (url.scheme == "http" && url.port == 80)
        || (url.scheme == "https" && url.port == 443)
    {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    out.extend_from_slice(format!("{} {} HTTP/1.1\r\n", req.method, url.path).as_bytes());
    out.extend_from_slice(format!("Host: {host_header}\r\n").as_bytes());
    let mut has_ua = false;
    for (k, v) in req.headers {
        if k.eq_ignore_ascii_case("host") {
            continue; // ours wins; a caller-provided Host would desync the socket
        }
        if k.eq_ignore_ascii_case("user-agent") {
            has_ua = true;
        }
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_ua {
        out.extend_from_slice(b"User-Agent: aimap\r\n");
    }
    match req.body {
        Some(body) => out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes()),
        None => out.extend_from_slice(b"Content-Length: 0\r\n"),
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(req.body.unwrap_or(&[]));
    out
}

/// Send one request over an already-connected duplex stream and read the
/// response. The seam the tests drive (`exchange` is all of the wire logic;
/// only socket setup differs between http and https).
pub fn exchange(
    stream: &mut (impl Read + Write),
    req: &Request<'_>,
    url: &Url,
    deadline: Instant,
) -> Result<Response, HttpError> {
    let bytes = build_request_bytes(req, url);
    stream
        .write_all(&bytes)
        .map_err(|e| io_error("write", e, deadline))?;
    stream.flush().map_err(|e| io_error("write", e, deadline))?;
    read_response(stream, deadline, req.method == "HEAD")
}

/// Send one request: connects (TCP, plus TLS for https), exchanges, closes.
pub fn request(req: &Request<'_>) -> Result<Response, HttpError> {
    let url = parse_url(req.url)?;
    let deadline = Instant::now() + req.timeout;
    let mut stream = connect(&url, deadline)?;
    exchange(&mut stream, req, &url, deadline)
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn connect(url: &Url, deadline: Instant) -> Result<Conn, HttpError> {
    let addrs: Vec<_> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Connect(format!("cannot resolve {}': {e}", url.host)))?
        .collect();
    let mut last = HttpError::Connect(format!("no addresses for {}", url.host));
    let tcp = 'outer: {
        for addr in addrs {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match TcpStream::connect_timeout(&addr, remaining) {
                Ok(s) => break 'outer s,
                Err(e) => last = io_error("connect", e, deadline),
            }
        }
        return Err(last);
    };
    tcp.set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| HttpError::Connect(format!("socket: {e}")))?;
    tcp.set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| HttpError::Connect(format!("socket: {e}")))?;
    if url.scheme != "https" {
        return Ok(Conn::Plain(tcp));
    }
    let provider = Arc::new(rustls_rustcrypto::provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| HttpError::Tls(format!("TLS: {e}")))?
        .with_root_certificates(root_store())
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
        .map_err(|e| HttpError::Tls(format!("invalid host name {:?}: {e}", url.host)))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| HttpError::Tls(format!("TLS: {e}")))?;
    Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
}

fn root_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    store
}

fn io_error(what: &str, e: std::io::Error, deadline: Instant) -> HttpError {
    if Instant::now() >= deadline
        || matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
    {
        return HttpError::Timeout;
    }
    HttpError::Protocol(format!("{what}: {e}"))
}

/// Read and decode one response. Handles 1xx interim responses, Content-Length,
/// chunked transfer encoding, and (with no framing headers) read-to-close.
/// Read and decode one response. Handles 1xx interim responses, Content-Length,
/// chunked transfer encoding, and (with no framing headers) read-to-close.
/// HEAD responses carry headers but never a body, whatever they announce.
fn read_response(
    stream: &mut (impl Read + Write),
    deadline: Instant,
    head_only: bool,
) -> Result<Response, HttpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if buf.len() > MAX_HEAD {
            return Err(HttpError::Protocol("response head too large".into()));
        }
        // Try to parse a (possibly interim) head from what we have.
        let head = match parse_head(&buf) {
            Some(head) => head,
            None => {
                read_more(stream, &mut buf, &mut chunk, deadline)?;
                continue;
            }
        };
        if head.status / 100 == 1 {
            // 100 Continue and friends: interim, drop and read the real head.
            buf.drain(..head.head_len);
            continue;
        }
        if head.status == 204 || head_only {
            // 204 No Content / HEAD: no body regardless of headers.
            return Ok(finish(head, Vec::new()));
        }
        let body: Vec<u8> = loop {
            match body_complete(&head, &buf[head.head_len..]) {
                Ok(Some(body)) => break body,
                Ok(None) => {
                    read_more(stream, &mut buf, &mut chunk, deadline)?;
                    if buf.len() - head.head_len > MAX_BODY {
                        return Err(HttpError::Protocol("response body too large".into()));
                    }
                }
                Err(e) => return Err(e),
            }
        };
        return Ok(finish(head, body));
    }
}

fn finish(head: Head, body: Vec<u8>) -> Response {
    Response { status: head.status, headers: head.headers, body }
}

fn read_more(
    stream: &mut (impl Read + Write),
    buf: &mut Vec<u8>,
    chunk: &mut [u8],
    deadline: Instant,
) -> Result<(), HttpError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(HttpError::Timeout);
    }
    // Reads are in small slices so the socket timeout bounds the wait; a
    // longer timeout simply comes back around until the deadline passes.
    match stream.read(chunk) {
        Ok(0) => Err(HttpError::Protocol("connection closed mid-response".into())),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
        Err(e) => Err(io_error("read", e, deadline)),
    }
}

struct Head {
    status: u16,
    headers: Vec<(String, String)>,
    head_len: usize,
}

/// Parse "HTTP/1.1 200 OK\r\nheaders\r\n\r\n" from the front of `buf`;
/// None while incomplete.
fn parse_head(buf: &[u8]) -> Option<Head> {
    let end = find_double_crlf(buf)?;
    let head_bytes = &buf[..end];
    let text = std::str::from_utf8(head_bytes).ok()?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let mut parts = status_line.split(' ');
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = parts.next()?.parse().ok()?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Some(Head { status, headers, head_len: end + 4 })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Some(Ok(body)) when complete per the framing headers, Some(Err) when the
/// framing is broken, None when more bytes are needed.
fn body_complete(head: &Head, body: &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
    let chunked = head
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        return decode_chunked(body);
    }
    let content_length = head
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());
    match content_length {
        Some(n) => {
            if body.len() < n {
                Ok(None)
            } else {
                Ok(Some(body[..n].to_vec()))
            }
        }
        None => Ok(Some(body.to_vec())), // read-to-close: everything so far is the body
    }
}

/// Chunked framing: `SIZE hex\r\n data \r\n` repeated, `0\r\n` trailers `\r\n`.
/// Some(Ok(body)) when the terminating chunk is present, None when more is
/// needed, Some(Err) on malformed framing.
fn decode_chunked(body: &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        // find the size line
        let Some(line_end) = find_crlf(rest) else {
            if rest.len() > 32 {
                return Err(HttpError::Protocol("chunk size line too long".into()));
            }
            return Ok(None); // need more bytes for the size line
        };
        let size_text = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| HttpError::Protocol("non-ASCII chunk size".into()))?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Protocol(format!("bad chunk size {size_text:?}")))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            // Trailer section: ends at an empty line. With no trailers the
            // remaining bytes are exactly the final "\r\n".
            if rest.starts_with(b"\r\n") {
                return Ok(Some(out));
            }
            return match find_double_crlf(rest) {
                Some(_) => Ok(Some(out)),
                None => Ok(None), // more trailer bytes needed
            };
        }
        if out.len() + size > MAX_BODY {
            return Err(HttpError::Protocol("chunked body too large".into()));
        }
        if rest.len() < size + 2 {
            return Ok(None); // need the whole chunk plus its CRLF
        }
        out.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" {
            return Err(HttpError::Protocol("chunk not terminated by CRLF".into()));
        }
        rest = &rest[size + 2..];
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(url: &'a str, method: &'a str, headers: &'a [(String, String)], body: Option<&'a [u8]>) -> Request<'a> {
        Request { method, url, headers, body, timeout: Duration::from_secs(5) }
    }

    #[test]
    fn parses_urls() {
        let u = parse_url("https://api.example.com/v1/systemone").unwrap();
        assert_eq!(u, Url { scheme: "https".into(), host: "api.example.com".into(), port: 443, path: "/v1/systemone".into() });
        let u = parse_url("http://localhost:5000").unwrap();
        assert_eq!(u, Url { scheme: "http".into(), host: "localhost".into(), port: 5000, path: "/".into() });
        let u = parse_url("HTTP://X.example:8080/a?b=c").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/a?b=c");
        let u = parse_url("http://[::1]:9/foo").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9));
        assert!(parse_url("ftp://x/").is_err());
        assert!(parse_url("no-scheme").is_err());
    }

    #[test]
    fn request_bytes_are_exact() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer k".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let r = req("https://api.example.com/v1/systemone", "POST", &headers, Some(b"{}"));
        let url = parse_url(r.url).unwrap();
        let bytes = build_request_bytes(&r, &url);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "POST /v1/systemone HTTP/1.1\r\n\
             Host: api.example.com\r\n\
             Authorization: Bearer k\r\n\
             Content-Type: application/json\r\n\
             User-Agent: aimap\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\
             \r\n\
             {}"
        );
    }

    #[test]
    fn content_length_body() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello";
        let head = parse_head(head).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.head_len, 70);
        assert_eq!(body_complete(&head, b"hello").unwrap().unwrap(), b"hello");
        assert!(body_complete(&head, b"hel").unwrap().is_none());
    }

    #[test]
    fn chunked_body_with_trailers() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\nX-T: 1\r\n\r\n";
        let head = parse_head(raw).unwrap();
        let body = &raw[head.head_len..];
        assert_eq!(decode_chunked(body).unwrap().unwrap(), b"hello, world");
        assert!(decode_chunked(b"5\r\nhel").unwrap().is_none());
        assert!(decode_chunked(b"zz\r\n").is_err());
    }

    #[test]
    fn read_to_close_framing() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nstreamed";
        let head = parse_head(head).unwrap();
        assert_eq!(body_complete(&head, b"streamed").unwrap().unwrap(), b"streamed");
    }

    #[test]
    fn skips_interim_100() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok";
        let resp = read_response(&mut std::io::Cursor::new(raw.to_vec()), Instant::now() + Duration::from_secs(1), false).unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"ok");
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let raw = b"HTTP/1.1 200 OK\r\nCache-Control: public, max-age=3600\r\nContent-Length: 0\r\n\r\n";
        let resp = read_response(&mut std::io::Cursor::new(raw.to_vec()), Instant::now() + Duration::from_secs(1), false).unwrap();
        assert_eq!(resp.header("cache-control"), Some("public, max-age=3600"));
        assert_eq!(resp.header("CACHE-CONTROL"), Some("public, max-age=3600"));
        assert_eq!(resp.header("nope"), None);
    }

    #[test]
    fn head_responses_have_no_body() {
        // S3's HEAD answers carry the object's Content-Length but no bytes.
        let raw = b"HTTP/1.1 200 OK\r\nETag: \"etag-1\"\r\nContent-Length: 12345\r\n\r\n";
        let resp = read_response(&mut std::io::Cursor::new(raw.to_vec()), Instant::now() + Duration::from_secs(1), true).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("ETag"), Some("\"etag-1\""));
        assert!(resp.body.is_empty());
    }
}
