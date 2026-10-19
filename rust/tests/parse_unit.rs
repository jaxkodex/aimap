//! Port of `tests/test_parse.py`. Same inputs, same expected outcomes.

use aimap::parse::{
    body_text, derive_thread_id, fallback_message_id, normalize_subject, parse_internal_date,
    parse_meta, strip_quoted,
};

/// The `email_bytes` fixture builder from tests/conftest.py.
fn email_bytes(
    n: u32,
    message_id: Option<&str>,
    sender: &str,
    subject: Option<&str>,
    body: Option<&str>,
    extra: &str,
) -> Vec<u8> {
    let mid = message_id.map(|m| m.to_string()).unwrap_or(format!("<m{}@example.com>", n));
    let mut headers = format!(
        "From: {}\r\nTo: me@example.com\r\nSubject: {}\r\n",
        sender,
        subject.unwrap_or(&format!("m{}", n))
    );
    headers += "Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n";
    if !mid.is_empty() {
        headers += &format!("Message-ID: {}\r\n", mid);
    }
    format!("{}{}\r\n{}\r\n", headers, extra, body.unwrap_or(&format!("body {}", n))).into_bytes()
}

#[test]
fn meta_from_headers() {
    let raw = email_bytes(
        1,
        Some("<m1@example.com>"),
        "\"Ana Diaz\" <Ana@Example.com>",
        None,
        None,
        "List-Unsubscribe: <mailto:u@x>\r\nIn-Reply-To: <prev@x>\r\n",
    );
    let m = parse_meta(&raw);
    assert_eq!(m.rfc822_message_id, "<m1@example.com>");
    assert_eq!(m.from_name.as_deref(), Some("Ana Diaz"));
    assert_eq!(m.from_email.as_deref(), Some("ana@example.com"));
    assert_eq!(m.sent_at.map(|t| t.to_string()), Some("2026-01-01T10:00:00Z".to_string()));
    assert!(m.has_list_headers);
    assert_eq!(m.in_reply_to.as_deref(), Some("<prev@x>"));
}

#[test]
fn missing_message_id_falls_back_to_content_hash() {
    let raw = email_bytes(1, Some(""), "Shop <deals@shop.example>", None, None, "");
    assert_eq!(parse_meta(&raw).rfc822_message_id, fallback_message_id(&raw));
}

#[test]
fn garbage_does_not_raise() {
    let m = parse_meta(b"\xff\xfe not an email");
    assert!(m.rfc822_message_id.starts_with("sha256:"));
    assert!(m.sent_at.is_none());
}

#[test]
fn body_prefers_plain_and_falls_back_to_html_and_trims() {
    let mut html = b"From: a@x\r\nContent-Type: text/html\r\n\r\n<html><style>x{}</style><p>Hello &amp; welcome ".to_vec();
    html.extend_from_slice(b"to the long newsletter https://x.example/track</p>");
    for _ in 0..400 {
        html.extend_from_slice(b"<p>word</p>");
    }
    html.extend_from_slice(b"</html>");
    let text = body_text(&html, 100);
    assert!(text.starts_with("Hello & welcome"), "got: {:?}", text);
    assert!(!text.contains("https"));
    assert!(!text.contains("x{}"));
    assert!(text.ends_with("[...]"));
    assert!(text.chars().count() <= 106, "len={}", text.chars().count());
    assert_eq!(text, "Hello & welcome to the long [...]");
}

#[test]
fn internal_date() {
    let t = parse_internal_date("01-Jan-2026 10:00:00 +0000").unwrap();
    assert_eq!(t.to_string(), "2026-01-01T10:00:00Z");
    assert!(parse_internal_date("").is_none());
    assert!(parse_internal_date("garbage").is_none());
    assert_eq!(
        parse_internal_date("15-Jun-2025 09:05:07 +0200").map(|t| t.to_string()),
        Some("2025-06-15T07:05:07Z".to_string())
    );
}

#[test]
fn normalize_subject_cases() {
    assert_eq!(normalize_subject(Some("RE: Hello")), "hello");
    assert_eq!(normalize_subject(Some("Re: FW: Test")), "test");
    assert_eq!(normalize_subject(Some("AW: Something")), "something");
    assert_eq!(normalize_subject(Some("  RE:  Lots   of    spaces  ")), "lots of spaces");
    assert_eq!(normalize_subject(None), "");
    assert_eq!(normalize_subject(Some("")), "");
}

#[test]
fn derive_thread_id_from_references() {
    let tid = derive_thread_id(
        Some("<a@x> <b@x> <c@x>"),
        Some("<c@x>"),
        Some("Test"),
        &["me@x".to_string(), "you@x".to_string()],
        None,
    );
    assert_eq!(tid, "<a@x>");
}

#[test]
fn derive_thread_id_from_in_reply_to() {
    let tid = derive_thread_id(
        None,
        Some("<root@x>"),
        Some("Test"),
        &["me@x".to_string(), "you@x".to_string()],
        None,
    );
    assert_eq!(tid, "<root@x>");
}

#[test]
fn derive_thread_id_from_subject() {
    let participants = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
    let tid1 = derive_thread_id(None, None, Some("Meeting tomorrow"), &participants("alice@x", "bob@x"), None);
    let tid2 = derive_thread_id(None, None, Some("RE: Meeting tomorrow"), &participants("bob@x", "alice@x"), None);
    // same normalized subject + sorted participants
    assert_eq!(tid1, tid2);
    assert!(tid1.starts_with("thread:"));
}

const OUTLOOK_REPLY: &str = "Thanks for the update.\n\nI'll get back to you tomorrow.\n\n\
-----Original Message-----\nFrom: Bob <bob@example.com>\nSent: Monday, January 1, 2026 10:00 AM\n\
To: Alice <alice@example.com>\nSubject: RE: Project status\n\nHere's the latest update.\n";

#[test]
fn strip_quoted_outlook() {
    let stripped = strip_quoted(OUTLOOK_REPLY);
    assert!(stripped.contains("Thanks for the update"));
    assert!(stripped.contains("I'll get back to you tomorrow"));
    assert!(!stripped.contains("Original Message"));
    assert!(!stripped.contains("bob@example.com"));
}

#[test]
fn strip_quoted_handles_signature() {
    let text = "See you soon!\n\n-- \nBest regards,\nAlice\n";
    let stripped = strip_quoted(text);
    assert!(stripped.contains("See you soon"));
    assert!(!stripped.contains("Best regards"));
}

#[test]
fn strip_quoted_handles_gmail_style() {
    let text = "Sounds good.\n\nOn Mon, Jan 1, 2026 at 10:00 AM Bob <bob@example.com> wrote:\n\
> Here's my proposal.\n> Let me know what you think.\n";
    let stripped = strip_quoted(text);
    assert!(stripped.contains("Sounds good"));
    assert!(!stripped.contains("Here's my proposal"));
    assert!(!stripped.contains("wrote:"));
}

#[test]
fn clean_text_trims_unicode_whitespace_like_python() {
    // U+2007 FIGURE SPACE is isspace() in Python: trimmed at line boundaries,
    // kept mid-line. Found by diffing against real S3-stored mail.
    use aimap::parse::clean_text;
    assert_eq!(clean_text("a\u{2007} b\u{2007} \n\u{2007} d \u{2007}\n"), "a\u{2007} b\nd");
    // \x1c and U+2028 are line separators for Python's splitlines()
    assert_eq!(clean_text("x \u{1c} y \n"), "x\ny");
    assert_eq!(clean_text("web. \u{2028} Los"), "web.\nLos");
}
