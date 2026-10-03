"""Read what we need out of a raw RFC 822 message.

`parse_meta` returns only headers, which is what goes into Postgres. `body_text`
extracts a cleaned, trimmed text body for the classifier. It is kept in memory
and never stored.
"""

from __future__ import annotations

import email
import email.utils
import hashlib
import html
import re
from dataclasses import dataclass
from datetime import UTC, datetime
from email import policy
from email.message import EmailMessage
from html.parser import HTMLParser


@dataclass(frozen=True)
class MessageMeta:
    rfc822_message_id: str
    from_email: str | None
    from_name: str | None
    subject: str | None
    sent_at: datetime | None
    in_reply_to: str | None
    references: str | None
    has_list_headers: bool


def _message(raw: bytes) -> EmailMessage:
    return email.message_from_bytes(raw, policy=policy.default, _class=EmailMessage)


def _header(msg: EmailMessage, name: str) -> str | None:
    try:
        value = msg.get(name)
    except Exception:  # a malformed header must not stop ingestion
        return None
    if value is None:
        return None
    text = " ".join(str(value).split())
    return text or None


def fallback_message_id(raw: bytes) -> str:
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def normalize_subject(subject: str | None) -> str:
    """Strip RE:/FW:/FWD: and their localized variants, collapse whitespace, casefold."""
    if not subject:
        return ""
    s = subject
    while True:
        prev = s
        s = _REPLY_PREFIX.sub("", s)
        if s == prev:
            break
    s = re.sub(r"\s+", " ", s).strip().casefold()
    return s


def derive_thread_id(references: str | None, in_reply_to: str | None, subject: str | None,
                     participants: list[str]) -> str:
    """Derive a thread identifier from References chain root or a hash of subject + participants.
    
    If References or In-Reply-To exist, the root message-id (leftmost in References or In-Reply-To)
    is the thread_id. Otherwise, hash the normalized subject with sorted participant addresses.
    participants is [from_email, to_email, cc_email, ...] or any list of addresses involved.
    """
    # Try References header first (space or comma separated list of message-ids)
    if references:
        parts = re.split(r"[,\s]+", references.strip())
        message_ids = [p.strip() for p in parts if p.strip().startswith("<") and p.strip().endswith(">")]
        if message_ids:
            return message_ids[0]  # leftmost is the root
    
    # Fall back to In-Reply-To
    if in_reply_to and in_reply_to.startswith("<") and in_reply_to.endswith(">"):
        return in_reply_to
    
    # No reply chain: hash subject + sorted participants
    norm_subject = normalize_subject(subject)
    sorted_participants = sorted(p.lower() for p in participants if p)
    text = norm_subject + "|" + "|".join(sorted_participants)
    return "thread:" + hashlib.sha256(text.encode()).hexdigest()[:16]


def parse_meta(raw: bytes) -> MessageMeta:
    msg = _message(raw)
    from_name, from_email = email.utils.parseaddr(_header(msg, "From") or "")
    sent_at = None
    if date := _header(msg, "Date"):
        try:
            sent_at = email.utils.parsedate_to_datetime(date)
            if sent_at.tzinfo is None:
                sent_at = sent_at.replace(tzinfo=UTC)
        except (TypeError, ValueError, IndexError):
            sent_at = None
    precedence = (_header(msg, "Precedence") or "").lower()
    return MessageMeta(
        rfc822_message_id=_header(msg, "Message-ID") or fallback_message_id(raw),
        from_email=from_email.lower() or None,
        from_name=from_name or None,
        subject=_header(msg, "Subject"),
        sent_at=sent_at,
        in_reply_to=_header(msg, "In-Reply-To"),
        references=_header(msg, "References"),
        has_list_headers=bool(_header(msg, "List-Unsubscribe") or _header(msg, "List-Id")
                              or precedence in {"bulk", "list"}),
    )


def parse_internal_date(value: str) -> datetime | None:
    """IMAP INTERNALDATE, e.g. '01-Jan-2026 10:00:00 +0000'."""
    try:
        return datetime.strptime(value, "%d-%b-%Y %H:%M:%S %z")
    except (TypeError, ValueError):
        return None


def attachment_names(raw: bytes, limit: int = 5) -> list[str]:
    msg = _message(raw)
    if not msg.is_multipart():
        return []
    names = [n for part in msg.iter_attachments() if (n := part.get_filename())]
    return names[:limit]


# --------------------------------------------------------------------------------------
# body text for the classifier
# --------------------------------------------------------------------------------------


class _TextExtractor(HTMLParser):
    SKIP = {"style", "script", "head", "title"}
    BLOCK = {"p", "div", "br", "tr", "li", "h1", "h2", "h3", "h4", "table", "td"}

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self.skip = 0

    def handle_starttag(self, tag, attrs):
        if tag in self.SKIP:
            self.skip += 1
        elif tag in self.BLOCK:
            self.parts.append("\n")

    def handle_endtag(self, tag):
        if tag in self.SKIP and self.skip:
            self.skip -= 1

    def handle_data(self, data):
        if not self.skip:
            self.parts.append(data)


_URL = re.compile(r"https?://\S+|www\.\S+")
_INVISIBLE = re.compile(r"[\u200b-\u200f\u2060\u00ad\u034f\ufeff\u00a0]+")
_REPLY_PREFIX = re.compile(r"^\s*(re|fw|fwd|aw|r|tr|sv|enc|rif|res|wg|antw|vs|ynt)\s*:\s*", re.IGNORECASE)


def clean_text(text: str) -> str:
    text = _INVISIBLE.sub(" ", html.unescape(text))
    text = _URL.sub("", text)
    lines = [re.sub(r"[ \t]+", " ", ln).strip() for ln in text.splitlines()]
    return "\n".join(ln for ln in lines if ln)


def _part_text(msg: EmailMessage, subtype: str) -> str:
    part = msg.get_body(preferencelist=(subtype,))
    if part is None:
        return ""
    try:
        return part.get_content()
    except (LookupError, UnicodeError, AssertionError):
        payload = part.get_payload(decode=True) or b""
        return payload.decode("utf-8", "replace")


def body_text(raw: bytes, max_chars: int = 1500) -> str:
    """Plain text body, falling back to text extracted from HTML, cleaned and cut at max_chars."""
    msg = _message(raw)
    text = clean_text(_part_text(msg, "plain"))
    if len(text) < 80:
        html_body = _part_text(msg, "html")
        if html_body:
            parser = _TextExtractor()
            parser.feed(html_body)
            text = clean_text("".join(parser.parts)) or text
    if len(text) > max_chars:
        text = text[:max_chars].rsplit(" ", 1)[0] + " [...]"
    return text


def strip_quoted(text: str) -> str:
    """Remove quoted replies and signatures from a message body.
    
    Handles leading '>' blocks, 'On <date> <person> wrote:', '-----Original Message-----',
    '-----Original Appointment-----', and '-- ' signature cuts.
    """
    lines = text.splitlines()
    result = []
    
    for line in lines:
        stripped = line.strip()
        
        # Stop at signature marker (-- followed by space or newline)
        if line.rstrip() == "--" or line.rstrip() == "-- ":
            break
        
        # Stop at common reply/forward markers
        if stripped.startswith("-----Original Message-----"):
            break
        if stripped.startswith("-----Original Appointment-----"):
            break
        
        # 'On <date> ... wrote:' pattern (common in Gmail, Outlook replies)
        if re.match(r"^On\s+.+\s+wrote:\s*$", stripped, re.IGNORECASE):
            break
        
        # Skip lines starting with > (quoted text)
        if line.lstrip().startswith(">"):
            continue
        
        result.append(line)
    
    return "\n".join(result).strip()
