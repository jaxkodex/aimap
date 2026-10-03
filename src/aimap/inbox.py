"""Queries behind the API: messages with their latest labels, per account.

A message is unread when none of its locations carries \\Seen, and flagged when
any carries \\Flagged. Flags are the ones seen at ingestion, so they go stale
when mail is read elsewhere until flag sync lands. Lists page by (time, id),
newest first, with an opaque cursor.

Everything here reads, except `set_state`: the one write aimap does, and it only
touches aimap's own message_state table. The mailbox, the bucket and the labels
never change.
"""

from __future__ import annotations

import base64
from datetime import datetime
from typing import TYPE_CHECKING, Any, Literal

from psycopg import Connection

from aimap.home import Item
from aimap.home import reasons as generate_reasons
from aimap.parse import body_text, strip_quoted

if TYPE_CHECKING:
    from aimap.storage import Store

_LATEST = """
    LEFT JOIN LATERAL (
        SELECT importance, action_bucket, tags, insight, needs_review, priority, signals, created_at
        FROM classifications WHERE message_id = m.id
        ORDER BY created_at DESC, id DESC LIMIT 1
    ) c ON true"""
_UNREAD = r"NOT EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Seen' = ANY(l.flags))"
_FLAGGED = r"EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id = m.id AND '\Flagged' = ANY(l.flags))"
_AT = "coalesce(m.sent_at, m.created_at)"
_STATE = "LEFT JOIN message_state s ON s.message_id = m.id"

FILTERS = {
    "all": "",
    "unread": f"AND {_UNREAD}",
    "flagged": f"AND {_FLAGGED}",
    "needs_review": "AND c.needs_review",
}


class CursorError(ValueError):
    pass


def needs_reply(action_bucket: str | None, signals: dict[str, Any], bulk: bool) -> dict[str, Any] | None:
    """Decide if a message needs a reply based on labels already stored.
    
    Returns {"needed": bool, "reason": str | None} when labels exist, or None if not yet classified.
    Reply needed when: action_bucket is 'reply', or 'act_now' with real_person signal and not bulk.
    """
    if action_bucket is None:
        return None  # not classified yet
    
    if action_bucket == "reply":
        # Generate the reason from signals
        signal_reasons = generate_reasons(signals)
        reason = signal_reasons[0] if signal_reasons else "Someone expects a reply."
        return {"needed": True, "reason": reason}
    
    if action_bucket == "act_now":
        has_real_person = signals.get("real_person", 0) > 0
        if has_real_person and not bulk:
            signal_reasons = generate_reasons(signals)
            reason = signal_reasons[0] if signal_reasons else "A person wrote to you."
            return {"needed": True, "reason": reason}
    
    return {"needed": False, "reason": None}


def encode_cursor(at: datetime, message_id: int) -> str:
    return base64.urlsafe_b64encode(f"{at.isoformat()}|{message_id}".encode()).decode().rstrip("=")


def decode_cursor(cursor: str) -> tuple[datetime, int]:
    try:
        raw = base64.urlsafe_b64decode(cursor + "=" * (-len(cursor) % 4)).decode()
        at, message_id = raw.rsplit("|", 1)
        return datetime.fromisoformat(at), int(message_id)
    except (ValueError, UnicodeDecodeError) as e:
        raise CursorError("invalid cursor") from e


def home_items(conn: Connection, since: datetime, account: str | None = None) -> list[Item]:
    rows = conn.execute(f"""
        SELECT m.id, a.address, m.from_email, m.from_name, m.subject, m.sent_at, {_UNREAD},
               c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority, c.signals, p.name,
               s.state, s.changed_at
        FROM messages m
        JOIN accounts a ON a.id = m.account_id
        JOIN profiles p ON p.id = a.profile_id
        {_LATEST}
        {_STATE}
        WHERE {_AT} >= %(since)s AND (%(account)s::text IS NULL OR a.address = %(account)s)""",
                        {"since": since, "account": account}).fetchall()
    return [Item(message_id=r[0], account=r[1], from_email=r[2], from_name=r[3], subject=r[4], sent_at=r[5],
                 unread=r[6], importance=r[7], action_bucket=r[8], tags=r[9] or [], insight=r[10],
                 needs_review=bool(r[11]), priority=r[12] or 0.0, signals=r[13] or {}, profile=r[14],
                 state=r[15], state_changed_at=r[16]) for r in rows]


def handled_since(conn: Connection, since: datetime, account: str | None = None) -> int:
    """How many messages were marked handled since `since`, for one account or all of them."""
    return conn.execute("""
        SELECT count(*) FROM message_state s
        JOIN messages m ON m.id = s.message_id
        JOIN accounts a ON a.id = m.account_id
        WHERE s.state = 'handled' AND s.changed_at >= %(since)s
          AND (%(account)s::text IS NULL OR a.address = %(account)s)""",
                        {"since": since, "account": account}).fetchone()[0]


def set_state(conn: Connection, message_id: int, state: Literal["handled", "later"] | None,
              *, changed_by: str) -> dict[str, Any] | None:
    """Mark a message handled or later, or clear it with `state=None` (undo). None if there is no such message.

    Idempotent, and every call refreshes `changed_at`, so the newest 'later' sorts last on Home."""
    if conn.execute("SELECT 1 FROM messages WHERE id = %s", (message_id,)).fetchone() is None:
        return None
    if state is None:
        conn.execute("DELETE FROM message_state WHERE message_id = %s", (message_id,))
        return {"message_id": message_id, "state": None, "changed_at": None}
    r = conn.execute("""
        INSERT INTO message_state (message_id, state, changed_by) VALUES (%s, %s, %s)
        ON CONFLICT (message_id) DO UPDATE
            SET state = excluded.state, changed_by = excluded.changed_by, changed_at = now()
        RETURNING state, changed_at""", (message_id, state, changed_by)).fetchone()
    return {"message_id": message_id, "state": r[0], "changed_at": r[1]}


def sorted_at(conn: Connection, account: str | None = None) -> datetime | None:
    """When the newest classification was stored, for one account or all of them."""
    return conn.execute("""
        SELECT max(c.created_at) FROM classifications c
        JOIN messages m ON m.id = c.message_id
        JOIN accounts a ON a.id = m.account_id
        WHERE %(account)s::text IS NULL OR a.address = %(account)s""", {"account": account}).fetchone()[0]


def list_messages(conn: Connection, *, account: str | None = None, filter: str = "all", cursor: str | None = None,
                  limit: int = 50) -> tuple[list[dict[str, Any]], str | None]:
    """One page of messages, newest first, and the cursor for the next page (None on the last one)."""
    if filter not in FILTERS:
        raise ValueError(f"filter must be one of {sorted(FILTERS)}")
    after = decode_cursor(cursor) if cursor else None
    rows = conn.execute(f"""
        SELECT m.id, a.address, m.from_email, m.from_name, m.subject, m.sent_at, {_AT}, {_UNREAD}, {_FLAGGED},
               c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority
        FROM messages m
        JOIN accounts a ON a.id = m.account_id
        {_LATEST}
        WHERE (%(account)s::text IS NULL OR a.address = %(account)s)
          AND (%(at)s::timestamptz IS NULL OR ({_AT}, m.id) < (%(at)s, %(id)s))
          {FILTERS[filter]}
        ORDER BY {_AT} DESC, m.id DESC
        LIMIT %(limit)s""",
                        {"account": account, "at": after[0] if after else None, "id": after[1] if after else None,
                         "limit": limit + 1}).fetchall()
    page = rows[:limit]
    items = [{
        "message_id": r[0], "account": r[1], "from_email": r[2], "sender": r[3] or r[2] or "(unknown)",
        "subject": r[4], "sent_at": r[5], "unread": r[7], "flagged": r[8], "importance": r[9],
        "action_bucket": r[10], "tags": r[11] or [], "insight": r[12], "needs_review": r[13], "priority": r[14],
    } for r in page]
    next_cursor = encode_cursor(page[-1][6], page[-1][0]) if len(rows) > limit else None
    return items, next_cursor


def get_message(conn: Connection, message_id: int) -> dict[str, Any] | None:
    """Metadata, latest labels with their signals, and where the message is stored."""
    r = conn.execute(f"""
        SELECT m.id, a.address, m.rfc822_message_id, m.from_email, m.from_name, m.subject, m.sent_at,
               m.in_reply_to, m.has_list_headers, {_UNREAD}, {_FLAGGED},
               c.importance, c.action_bucket, c.tags, c.insight, c.needs_review, c.priority, c.signals,
               c.created_at, s.state, p.name
        FROM messages m
        JOIN accounts a ON a.id = m.account_id
        JOIN profiles p ON p.id = a.profile_id
        {_LATEST}
        {_STATE}
        WHERE m.id = %s""", (message_id,)).fetchone()
    if r is None:
        return None
    locations = conn.execute("""
        SELECT mailbox, flags, s3_key FROM message_locations WHERE message_id = %s
        ORDER BY ingested_at DESC, id DESC""", (message_id,)).fetchall()
    
    reply = needs_reply(r[12], r[17] or {}, r[8]) if r[12] is not None else None
    
    return {
        "message_id": r[0], "account": r[1], "profile": r[20], "rfc822_message_id": r[2], "from_email": r[3],
        "sender": r[4] or r[3] or "(unknown)", "subject": r[5], "sent_at": r[6], "in_reply_to": r[7],
        "bulk": r[8], "unread": r[9], "flagged": r[10], "state": r[19],
        "labels": None if r[12] is None else {
            "importance": r[11], "action_bucket": r[12], "tags": r[13] or [], "insight": r[14],
            "needs_review": r[15], "priority": r[16], "signals": r[17] or {}, "classified_at": r[18],
        },
        "reply": reply,
        "mailboxes": [{"mailbox": mb, "flags": flags} for mb, flags, _ in locations],
        "s3_key": locations[0][2] if locations else None,
    }


def accounts(conn: Connection) -> list[dict[str, Any]]:
    rows = conn.execute(f"""
        SELECT a.address, p.name, count(m.id), count(m.id) FILTER (WHERE {_UNREAD})
        FROM accounts a
        JOIN profiles p ON p.id = a.profile_id
        LEFT JOIN messages m ON m.account_id = a.id
        GROUP BY a.id, a.address, p.name
        ORDER BY a.address""").fetchall()
    return [{"address": r[0], "profile": r[1], "messages": r[2], "unread": r[3]} for r in rows]


def get_thread(conn: Connection, message_id: int, store: Store) -> dict[str, Any] | None:
    """The messages in the same thread, newest first, with excerpts from S3."""
    # First check if the message exists and get its thread_id
    row = conn.execute("SELECT thread_id FROM messages WHERE id = %s", (message_id,)).fetchone()
    if row is None:
        return None
    thread_id = row[0]
    
    # Get all messages in the thread, newest first
    rows = conn.execute("""
        SELECT m.id, m.from_name, m.from_email, m.subject, m.sent_at,
               (SELECT s3_key FROM message_locations WHERE message_id = m.id ORDER BY ingested_at DESC LIMIT 1),
               a.address
        FROM messages m
        JOIN accounts a ON a.id = m.account_id
        WHERE m.thread_id = %s
        ORDER BY coalesce(m.sent_at, m.created_at) DESC, m.id DESC""", (thread_id,)).fetchall()
    
    messages = []
    for r in rows:
        msg_id, from_name, from_email, subject, sent_at, s3_key, account = r
        
        # Read body from S3 and create excerpt
        excerpt = None
        if s3_key:
            got = store.get_raw(s3_key)
            if got:
                full_text = body_text(got[0], max_chars=50_000)
                stripped = strip_quoted(full_text)
                excerpt = stripped[:240].rsplit(" ", 1)[0] + "…" if len(stripped) > 240 else stripped
        
        # Check if this message is from the recipient (account address)
        from_recipient = from_email and from_email.lower() == account.lower()
        
        messages.append({
            "message_id": msg_id,
            "sender": from_name or from_email or "(unknown)",
            "from_email": from_email,
            "subject": subject,
            "sent_at": sent_at,
            "from_recipient": from_recipient,
            "excerpt": excerpt,
        })
    
    return {
        "message_id": message_id,
        "thread_id": thread_id,
        "messages": messages,
    }
