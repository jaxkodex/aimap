from datetime import UTC, datetime

from conftest import email_bytes

from aimap.parse import (
    body_text,
    derive_thread_id,
    fallback_message_id,
    normalize_subject,
    parse_internal_date,
    parse_meta,
    strip_quoted,
)


def test_meta_from_headers():
    raw = email_bytes(1, sender='"Ana Diaz" <Ana@Example.com>', extra="List-Unsubscribe: <mailto:u@x>\r\n"
                      "In-Reply-To: <prev@x>\r\n")
    m = parse_meta(raw)
    assert m.rfc822_message_id == "<m1@example.com>"
    assert (m.from_name, m.from_email) == ("Ana Diaz", "ana@example.com")
    assert m.sent_at == datetime(2026, 1, 1, 10, tzinfo=UTC)
    assert m.has_list_headers and m.in_reply_to == "<prev@x>"


def test_missing_message_id_falls_back_to_content_hash():
    raw = email_bytes(1, message_id="")
    assert parse_meta(raw).rfc822_message_id == fallback_message_id(raw)


def test_garbage_does_not_raise():
    m = parse_meta(b"\xff\xfe not an email")
    assert m.rfc822_message_id.startswith("sha256:") and m.sent_at is None


def test_body_prefers_plain_and_falls_back_to_html_and_trims():
    html = (b"From: a@x\r\nContent-Type: text/html\r\n\r\n<html><style>x{}</style><p>Hello &amp; welcome "
            b"to the long newsletter https://x.example/track</p>" + b"<p>word</p>" * 400 + b"</html>")
    text = body_text(html, max_chars=100)
    assert text.startswith("Hello & welcome") and "https" not in text and "x{}" not in text
    assert text.endswith("[...]") and len(text) <= 106


def test_internal_date():
    assert parse_internal_date("01-Jan-2026 10:00:00 +0000") == datetime(2026, 1, 1, 10, tzinfo=UTC)
    assert parse_internal_date("") is None


def test_normalize_subject():
    assert normalize_subject("RE: Hello") == "hello"
    assert normalize_subject("Re: FW: Test") == "test"
    assert normalize_subject("AW: Something") == "something"
    assert normalize_subject("  RE:  Lots   of    spaces  ") == "lots of spaces"
    assert normalize_subject(None) == ""
    assert normalize_subject("") == ""


def test_derive_thread_id_from_references():
    tid = derive_thread_id("<a@x> <b@x> <c@x>", "<c@x>", "Test", ["me@x", "you@x"])
    assert tid == "<a@x>"


def test_derive_thread_id_from_in_reply_to():
    tid = derive_thread_id(None, "<root@x>", "Test", ["me@x", "you@x"])
    assert tid == "<root@x>"


def test_derive_thread_id_from_subject():
    tid1 = derive_thread_id(None, None, "Meeting tomorrow", ["alice@x", "bob@x"])
    tid2 = derive_thread_id(None, None, "RE: Meeting tomorrow", ["bob@x", "alice@x"])
    assert tid1 == tid2  # same normalized subject + sorted participants
    assert tid1.startswith("thread:")


def test_strip_quoted():
    outlook_reply = """Thanks for the update.

I'll get back to you tomorrow.

-----Original Message-----
From: Bob <bob@example.com>
Sent: Monday, January 1, 2026 10:00 AM
To: Alice <alice@example.com>
Subject: RE: Project status

Here's the latest update.
"""
    stripped = strip_quoted(outlook_reply)
    assert "Thanks for the update" in stripped
    assert "I'll get back to you tomorrow" in stripped
    assert "Original Message" not in stripped
    assert "bob@example.com" not in stripped


def test_strip_quoted_handles_signature():
    text = """See you soon!

-- 
Best regards,
Alice
"""
    stripped = strip_quoted(text)
    assert "See you soon" in stripped
    assert "Best regards" not in stripped


def test_strip_quoted_handles_gmail_style():
    text = """Sounds good.

On Mon, Jan 1, 2026 at 10:00 AM Bob <bob@example.com> wrote:
> Here's my proposal.
> Let me know what you think.
"""
    stripped = strip_quoted(text)
    assert "Sounds good" in stripped
    assert "Here's my proposal" not in stripped
    assert "wrote:" not in stripped
