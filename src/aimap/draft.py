"""Draft reply generation using an OpenAI-compatible chat completions endpoint.

The draft reads the thread (newest first), caps the context to a sane character limit,
and calls the model with a prompt that produces a plain-text reply body.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any

import requests
from psycopg import Connection

from aimap.inbox import get_thread
from aimap.parse import normalize_subject

log = logging.getLogger(__name__)

MAX_THREAD_CHARS = 12000
MAX_THREAD_MESSAGES = 6
EXCERPT_CHARS = 50000


@dataclass(frozen=True)
class DraftConfig:
    base_url: str
    api_key: str | None
    model: str
    timeout: float
    max_tokens: int
    
    def is_configured(self) -> bool:
        return self.api_key is not None and bool(self.model)


def build_prompt(thread_messages: list[dict[str, Any]], instructions: str | None) -> str:
    """Build the prompt from thread messages (newest first) and optional instructions.
    
    Returns a prompt that asks for a plain-text reply body only, no quoted original,
    no subject line inside the body.
    """
    # Build thread context
    context_parts = []
    for msg in thread_messages:
        sender = msg.get("sender", "(unknown)")
        excerpt = msg.get("excerpt") or "(no body)"
        context_parts.append(f"From: {sender}\n{excerpt}\n")
    
    context = "\n---\n\n".join(context_parts)
    
    instructions_text = ""
    if instructions:
        instructions_text = f"\n\nUser instructions: {instructions}"
    
    return f"""You are drafting a reply to this email thread (newest message first):

{context}

---

Draft a plain-text reply. Do not include quoted text, do not include a subject line. Just the body of the reply.\
{instructions_text}"""


def generate_draft(
    conn: Connection,
    store: Any,  # Store type
    message_id: int,
    instructions: str | None,
    config: DraftConfig,
) -> dict[str, Any]:
    """Generate a draft reply for the given message.
    
    Raises HTTPException (404, 422, 502, 503) on error.
    """
    from fastapi import HTTPException
    
    # Check configuration
    if not config.is_configured():
        raise HTTPException(503, "Drafting is not configured.")
    
    # Get the message to check if it exists and get reply info
    row = conn.execute("""
        SELECT m.id, m.from_email, m.subject, m.has_list_headers, c.action_bucket, c.signals
        FROM messages m
        LEFT JOIN LATERAL (
            SELECT action_bucket, signals
            FROM classifications WHERE message_id = m.id
            ORDER BY created_at DESC, id DESC LIMIT 1
        ) c ON true
        WHERE m.id = %s
    """, (message_id,)).fetchone()
    
    if row is None:
        raise HTTPException(404, "no such message")
    
    msg_id, from_email, subject, bulk, action_bucket, signals = row
    
    # Calculate reply info
    from aimap.inbox import needs_reply
    reply_info = needs_reply(action_bucket, signals or {}, bulk)
    needs = reply_info["needed"] if reply_info else False
    reason = reply_info["reason"] if reply_info else None
    
    # Validate instructions length
    if instructions and len(instructions) > 2000:
        raise HTTPException(422, "instructions too long")
    
    # Get thread
    thread_data = get_thread(conn, message_id, store)
    if not thread_data:
        raise HTTPException(404, "no such message")
    
    # Cap the thread to MAX_THREAD_MESSAGES and MAX_THREAD_CHARS
    messages = thread_data["messages"][:MAX_THREAD_MESSAGES]
    used_ids = []
    capped_messages = []
    total_chars = 0
    
    for msg in messages:
        excerpt = msg.get("excerpt") or ""
        if total_chars + len(excerpt) > MAX_THREAD_CHARS:
            break
        capped_messages.append(msg)
        used_ids.append(msg["message_id"])
        total_chars += len(excerpt)
    
    if not capped_messages:
        # Use at least the first message
        capped_messages = messages[:1]
        used_ids = [messages[0]["message_id"]]
    
    # Build prompt
    prompt = build_prompt(capped_messages, instructions)
    
    # Call the model
    try:
        response = requests.post(
            f"{config.base_url.rstrip('/')}/chat/completions",
            headers={
                "Authorization": f"Bearer {config.api_key}",
                "Content-Type": "application/json",
            },
            json={
                "model": config.model,
                "messages": [{"role": "user", "content": prompt}],
                "max_tokens": config.max_tokens,
                "temperature": 0.7,
            },
            timeout=config.timeout,
        )
        response.raise_for_status()
        data = response.json()
        body = data["choices"][0]["message"]["content"].strip()
    except requests.Timeout as e:
        raise HTTPException(502, "The model timed out.") from e
    except requests.RequestException as e:
        log.warning("draft model call failed", extra={"error": str(e)})
        raise HTTPException(502, "The model request failed.") from e
    except (KeyError, IndexError, json.JSONDecodeError) as e:
        log.warning("draft model response parse failed", extra={"error": str(e)})
        raise HTTPException(502, "The model response was invalid.") from e
    
    # Build reply subject (add RE: once, never double it)
    reply_subject = subject or ""
    norm = normalize_subject(reply_subject)
    if norm and not reply_subject.lower().strip().startswith("re:"):
        reply_subject = f"RE: {reply_subject}"
    elif not norm:
        reply_subject = "RE: "
    
    # Prepare to/cc lists (reply to sender)
    to = [from_email] if from_email else []
    
    return {
        "message_id": message_id,
        "needs_reply": needs,
        "reply_reason": reason,
        "to": to,
        "cc": [],
        "subject": reply_subject,
        "body": body,
        "model": config.model,
        "used_message_ids": used_ids,
        "created_at": datetime.now(UTC),
    }
