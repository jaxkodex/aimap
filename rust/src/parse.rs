//! Port of `src/aimap/parse.py`: read what we need out of a raw RFC 822 message.
//!
//! `parse_meta` returns only headers, which is what goes into Postgres. `body_text`
//! extracts a cleaned, trimmed text body for the classifier. It is kept in memory
//! and never stored.
//!
//! Like the Python original, nothing here fails hard on malformed input: a broken
//! message yields whatever headers could be read and a content-hash message id.

use std::sync::LazyLock;

use jiff::tz::{Offset, TimeZone};
use jiff::{civil, Timestamp};
use mailparse::body::Body;
use mailparse::{parse_mail, DispositionType, MailHeaderMap, ParsedMail};
use regex::Regex;
use sha2::{Digest, Sha256};

/// Message headers, as stored in Postgres (`parse_meta` in parse.py).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MessageMeta {
    pub rfc822_message_id: String,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub subject: Option<String>,
    pub sent_at: Option<Timestamp>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub has_list_headers: bool,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const HTML_SKIP_TAGS: [&str; 4] = ["style", "script", "head", "title"];
const HTML_BLOCK_TAGS: [&str; 11] = [
    "p", "div", "br", "tr", "li", "h1", "h2", "h3", "h4", "table", "td",
];

static REPLY_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(re|fw|fwd|aw|r|tr|sv|enc|rif|res|wg|antw|vs|ynt)\s*:\s*").unwrap()
});
static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+|www\.\S+").unwrap());
static INVISIBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{200b}-\u{200f}\u{2060}\u{ad}\u{34f}\u{feff}\u{a0}]+").unwrap()
});
static ON_WROTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^On\s+.+\s+wrote:\s*$").unwrap());
static SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());

pub fn fallback_message_id(raw: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(raw)))
}

pub fn normalize_subject(subject: Option<&str>) -> String {
    let Some(s) = subject.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    // Strip RE:/FW:/FWD: and their localized variants, repeatedly.
    let mut s = s.to_string();
    loop {
        let prev = s.clone();
        s = REPLY_PREFIX.replace(&s, "").into_owned();
        if s == prev {
            break;
        }
    }
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    casefold(&s)
}

/// Derive a thread identifier from References chain root, In-Reply-To, or the
/// message's own ID; otherwise hash the normalized subject with the sorted
/// participant addresses.
pub fn derive_thread_id(
    references: Option<&str>,
    in_reply_to: Option<&str>,
    subject: Option<&str>,
    participants: &[String],
    rfc822_message_id: Option<&str>,
) -> String {
    if let Some(references) = references.filter(|r| !r.is_empty()) {
        let first = references
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .find(|p| p.starts_with('<') && p.ends_with('>'));
        if let Some(id) = first {
            return id.to_string();
        }
    }
    if let Some(irt) = in_reply_to.filter(|v| v.starts_with('<') && v.ends_with('>')) {
        return irt.to_string();
    }
    if let Some(mid) = rfc822_message_id.filter(|v| v.starts_with('<') && v.ends_with('>')) {
        return mid.to_string();
    }
    let norm_subject = normalize_subject(subject);
    let mut sorted: Vec<String> = participants
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.to_lowercase())
        .collect();
    sorted.sort();
    let text = format!("{}|{}", norm_subject, sorted.join("|"));
    let digest = Sha256::digest(text.as_bytes());
    format!("thread:{}", &hex(&digest)[..16])
}

pub fn parse_meta(raw: &[u8]) -> MessageMeta {
    let msg = parse_mail(raw).ok();
    let hdr = |name: &str| msg.as_ref().and_then(|m| header_value(m, name));

    let (from_name, from_email) = match hdr("From") {
        Some(v) => {
            let (name, email) = parse_addr(&v);
            (name, email.map(|e| e.to_lowercase()).filter(|e| !e.is_empty()))
        }
        None => (None, None),
    };
    let sent_at = hdr("Date").and_then(|d| parse_rfc822_date(&d));
    let precedence = hdr("Precedence").map(|p| p.to_lowercase()).unwrap_or_default();

    MessageMeta {
        rfc822_message_id: hdr("Message-ID").unwrap_or_else(|| fallback_message_id(raw)),
        from_email,
        from_name,
        subject: hdr("Subject"),
        sent_at,
        in_reply_to: hdr("In-Reply-To"),
        references: hdr("References"),
        has_list_headers: hdr("List-Unsubscribe").is_some()
            || hdr("List-Id").is_some()
            || precedence == "bulk"
            || precedence == "list",
    }
}

/// IMAP INTERNALDATE, e.g. '01-Jan-2026 10:00:00 +0000'.
pub fn parse_internal_date(value: &str) -> Option<Timestamp> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let [date, time, zone] = tokens.as_slice() else {
        return None;
    };

    let (day, rest) = date.split_once('-')?;
    let (mon, year) = rest.split_once('-')?;
    let day: i8 = day.parse().ok()?;
    let year: i16 = year.parse().ok()?;
    let month = MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(mon))? as i8
        + 1;

    let mut t = time.split(':');
    let (h, mi, s) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if t.next().is_some() {
        return None;
    }

    let offset_secs = parse_zone_offset(zone)?;
    let dt = civil::date(year, month, day).at(h, mi, s, 0);
    let fixed = Offset::from_seconds(offset_secs).ok()?;
    Some(dt.to_zoned(TimeZone::fixed(fixed)).ok()?.timestamp())
}

/// Parse a `+HHMM` / `-HHMM` / `+HH:MM` / `Z` offset into seconds.
fn parse_zone_offset(zone: &str) -> Option<i32> {
    if zone == "Z" || zone.eq_ignore_ascii_case("UTC") {
        return Some(0);
    }
    let (sign, rest) = match zone.split_at(1) {
        ("+", r) => (1, r),
        ("-", r) => (-1, r),
        _ => return None,
    };
    let rest = rest.replace(':', "");
    if rest.len() != 4 {
        return None;
    }
    let h: i32 = rest[..2].parse().ok()?;
    let m: i32 = rest[2..].parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

pub fn attachment_names(raw: &[u8], limit: usize) -> Vec<String> {
    let Ok(msg) = parse_mail(raw) else {
        return Vec::new();
    };
    if !msg.ctype.mimetype.starts_with("multipart/") {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut body_seen = false;
    for part in &msg.subparts {
        // The first non-attachment leaf subpart is the body; everything after it
        // that carries a filename is an attachment (approximates iter_attachments).
        let is_attachment =
            part.get_content_disposition().disposition == DispositionType::Attachment;
        if !body_seen && !is_attachment && !part.ctype.mimetype.starts_with("multipart/") {
            body_seen = true;
            continue;
        }
        if let Some(name) = part_filename(part) {
            names.push(name);
            if names.len() >= limit {
                break;
            }
        }
    }
    names
}

/// `get_filename()` in parse.py: Content-Disposition filename, then Content-Type name.
fn part_filename(part: &ParsedMail) -> Option<String> {
    part.get_content_disposition()
        .params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned())
}

// --------------------------------------------------------------------------------------
// body text for the classifier
// --------------------------------------------------------------------------------------

pub fn clean_text(text: &str) -> String {
    let unescaped = html_unescape(text);
    let t = INVISIBLE_RE.replace_all(&unescaped, " ");
    let t = URL_RE.replace_all(&t, "");
    py_splitlines(&t)
        .into_iter()
        .map(|ln| py_trim(&SPACES_RE.replace_all(ln, " ")).to_string())
        .filter(|ln| !ln.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plain text body, falling back to text extracted from HTML, cleaned and cut at max_chars.
pub fn body_text(raw: &[u8], max_chars: usize) -> String {
    let Ok(msg) = parse_mail(raw) else {
        return String::new();
    };
    let mut text = clean_text(&part_text(&msg, "plain"));
    if text.chars().count() < 80 {
        let html_body = part_text(&msg, "html");
        if !html_body.is_empty() {
            let cleaned = clean_text(&extract_html_text(&html_body));
            if !cleaned.is_empty() {
                text = cleaned;
            }
        }
    }
    if text.chars().count() > max_chars {
        let cut: String = text.chars().take(max_chars).collect();
        let trimmed = match cut.rsplit_once(' ') {
            Some((head, _)) => head.to_string(),
            None => cut,
        };
        text = format!("{} [...]", trimmed);
    }
    text
}

/// `part.get_content()` for a text part, with Python's error behavior:
/// strict decode of the declared charset, falling back to UTF-8-replace
/// of the transport-decoded bytes (mailparse's own get_as_string would
/// silently fall back to Latin-1 instead, which differs on bad UTF-8).
fn part_text<'a>(msg: &'a ParsedMail<'a>, subtype: &str) -> String {
    let Some(part) = find_body(msg, subtype) else {
        return String::new();
    };
    let decoded: Vec<u8> = match part.get_body_encoded() {
        Body::SevenBit(b) | Body::EightBit(b) => b.get_raw().to_vec(),
        Body::QuotedPrintable(b) | Body::Base64(b) => b.get_decoded().unwrap_or_default(),
        Body::Binary(b) => b.get_raw().to_vec(),
    };
    let charset = part
        .ctype
        .params
        .get("charset")
        .map(|s| s.as_str())
        .unwrap_or("us-ascii");
    match charset.to_ascii_lowercase().as_str() {
        "us-ascii" | "ascii" | "utf-8" | "utf8" => match String::from_utf8(decoded) {
            Ok(s) => s,
            Err(e) => lossy(e.as_bytes()),
        },
        cs => charset::Charset::for_label(cs.as_bytes())
            .map(|c| c.decode(&decoded).0.into_owned())
            .unwrap_or_else(|| lossy(&decoded)),
    }
}

/// Approximates `EmailMessage.get_body(preferencelist=(subtype,))`.
fn find_body<'a>(m: &'a ParsedMail<'a>, subtype: &str) -> Option<&'a ParsedMail<'a>> {
    let want = format!("text/{}", subtype);
    let mt = m.ctype.mimetype.as_str();
    if !mt.starts_with("multipart/") {
        return if mt == want { Some(m) } else { None };
    }
    match mt.split('/').nth(1) {
        Some("alternative") => m.subparts.iter().find(|p| p.ctype.mimetype == want),
        Some("related") => {
            let start = m.ctype.params.get("start").map(|s| {
                s.trim().trim_start_matches('<').trim_end_matches('>').to_string()
            });
            let root = m
                .subparts
                .iter()
                .find(|p| {
                    start.as_ref().is_some_and(|s| {
                        p.headers.get_first_value("Content-ID").is_some_and(|cid| {
                            cid.trim().trim_start_matches('<').trim_end_matches('>') == s
                        })
                    })
                })
                .or_else(|| m.subparts.first())?;
            find_body(root, subtype)
                .or_else(|| m.subparts.iter().find_map(|p| find_body(p, subtype)))
        }
        _ => m.subparts.iter().find_map(|p| find_body(p, subtype)),
    }
}

// --------------------------------------------------------------------------------------
// HTML text extraction (port of the _TextExtractor HTMLParser subclass)
// --------------------------------------------------------------------------------------

/// Extract visible text from HTML: skip style/script/head/title, emit newlines
/// for block tags, decode entities. Mirrors parse.py's `_TextExtractor`.
pub fn extract_html_text(html: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut skip: usize = 0;
    let mut rest = html;

    while let Some(lt) = rest.find('<') {
        if skip == 0 {
            parts.push(decode_entities(&rest[..lt]));
        }
        let after = &rest[lt + 1..];
        if after.starts_with("!--") {
            match after.find("-->") {
                Some(p) => rest = &after[p + 3..],
                None => break, // unterminated comment swallows the rest
            }
            continue;
        }
        match find_tag_end(after) {
            Some((tag_src, consumed)) => {
                handle_tag(tag_src, &mut parts, &mut skip);
                rest = &after[consumed..];
            }
            None => {
                // Unterminated tag: the rest is data (Python emits it on close()).
                if skip == 0 {
                    parts.push(decode_entities(&rest[lt..]));
                }
                break;
            }
        }
    }
    if skip == 0 && !rest.is_empty() {
        parts.push(decode_entities(rest));
    }
    parts.join("")
}

/// Find the `>` closing a tag, respecting quoted attributes. Returns the text
/// between `<` and `>` and how many bytes were consumed past the `>`.
fn find_tag_end(s: &str) -> Option<(&str, usize)> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'>' {
                    return Some((&s[..i], i + 1));
                }
            }
        }
    }
    None
}

fn handle_tag(tag_src: &str, parts: &mut Vec<String>, skip: &mut usize) {
    let src = tag_src.trim();
    if src.starts_with('!') || src.starts_with('?') {
        return; // doctype / processing instruction
    }
    let endtag = src.starts_with('/');
    let selfclosing = src.ends_with('/');
    let name: String = src
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':')
        .collect::<String>()
        .to_ascii_lowercase();
    if name.is_empty() {
        return; // "<3" and friends are data in Python's parser too (roughly)
    }
    if HTML_SKIP_TAGS.contains(&name.as_str()) {
        if endtag {
            *skip = skip.saturating_sub(1);
        } else if !selfclosing {
            // A self-closing skip tag nets out to zero, like starttag+endtag in Python.
            *skip += 1;
        }
    } else if HTML_BLOCK_TAGS.contains(&name.as_str()) && !endtag {
        parts.push("\n".to_string());
    }
}

// --------------------------------------------------------------------------------------
// quoted replies and signatures
// --------------------------------------------------------------------------------------

/// Remove quoted replies and signatures from a message body.
///
/// Handles leading '>' blocks, 'On <date> <person> wrote:', '-----Original Message-----',
/// '-----Original Appointment-----', and '-- ' signature cuts.
pub fn strip_quoted(text: &str) -> String {
    let mut result: Vec<&str> = Vec::new();
    for line in py_splitlines(text) {
        if py_trim_end(line) == "--" {
            break;
        }
        let stripped = py_trim(line);
        if stripped.starts_with("-----Original Message-----")
            || stripped.starts_with("-----Original Appointment-----")
        {
            break;
        }
        if ON_WROTE_RE.is_match(stripped) {
            break;
        }
        if line.trim_start_matches(py_is_space).starts_with('>') {
            continue;
        }
        result.push(line);
    }
    py_trim(&result.join("\n")).to_string()
}

// --------------------------------------------------------------------------------------
// helpers
// --------------------------------------------------------------------------------------

/// `_header`: first header value, RFC2047-decoded and unfolded by mailparse,
/// then all whitespace runs collapsed like Python's `" ".join(value.split())`.
fn header_value(msg: &ParsedMail, name: &str) -> Option<String> {
    let value = msg.get_headers().get_first_value(name)?;
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// `email.utils.parseaddr` for the cases we care about:
/// `"Name" <addr>`, `Name <addr>`, `addr`. Never fails.
fn parse_addr(value: &str) -> (Option<String>, Option<String>) {
    let v = value.trim();
    if let Some(lt) = v.find('<') {
        if let Some(gt) = v[lt..].find('>') {
            let name = strip_quotes(v[..lt].trim());
            let addr = v[lt + 1..lt + gt].trim();
            return (non_empty(name), non_empty(addr));
        }
    }
    (None, non_empty(strip_quotes(v)))
}

fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s)
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Python `str.strip()` / `rstrip()` semantics: Unicode whitespace plus the
/// ASCII file/record/unit/line separators (\x1c-\x1f), which Python's
/// isspace() includes but Rust's is_whitespace() does not.
fn py_is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()` — also used by the draft module for `.lower().strip()`.
pub(crate) fn py_trim(s: &str) -> &str {
    s.trim_matches(py_is_space)
}

fn py_trim_end(s: &str) -> &str {
    s.trim_end_matches(py_is_space)
}

/// Python `str.splitlines()`: splits on \n, \r, \r\n and the exotic Unicode
/// line boundaries (\v, \f, \x1c-\x1e, \x85 NEL, U+2028, U+2029) that Rust's
/// str::lines() ignores. Found in real mail (U+2028 inside text/plain).
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut iter = s.char_indices().peekable();
    while let Some((idx, c)) = iter.next() {
        let is_boundary = matches!(
            c,
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'..='\u{1e}' | '\u{85}'
                | '\u{2028}' | '\u{2029}'
        );
        if is_boundary {
            out.push(&s[start..idx]);
            if c == '\r' && iter.peek() == Some(&(idx + 1, '\n')) {
                iter.next();
            }
            start = idx + c.len_utf8();
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn parse_rfc822_date(value: &str) -> Option<Timestamp> {
    let secs = mailparse::dateparse(value).ok()?;
    Timestamp::from_second(secs).ok()
}

/// Python str.casefold, restricted to the difference that matters in practice
/// (sharp s folds to "ss" under casefold but not under to_lowercase).
fn casefold(s: &str) -> String {
    s.to_lowercase().replace('ß', "ss")
}

fn html_unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

fn decode_entities(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
