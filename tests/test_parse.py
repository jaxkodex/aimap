from datetime import UTC, datetime

from conftest import email_bytes

from aimap.parse import body_text, fallback_message_id, parse_internal_date, parse_meta


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
