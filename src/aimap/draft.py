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

from aimap.inbox import get_message, get_thread

log = logging.getLogger(__name__)

MAX_THREAD_CHARS = 12000
MAX_THREAD_MESSAGES = 6


class DraftError(Exception):
    """Base for all draft errors."""


class MessageNotFound(DraftError):
    """The message does not exist."""


class InstructionsTooLong(DraftError):
    """The instructions field exceeded 2000 characters."""


class DraftingNotConfigured(DraftError):
    """Drafting is not configured (no API key or model)."""


class ModelTimeout(DraftError):
    """The model timed out."""


class ModelRequestFailed(DraftError):
    """The model request failed."""


@dataclass(frozen=True)
class DraftConfig:
    base_url: str
    api_key: str | None
    model: str
    timeout: float
    max_tokens: int
    reasoning_effort: str = "low"  # reasoning models burn the token budget on chain-of-thought otherwise

    def is_configured(self) -> bool:
        return self.api_key is not None and bool(self.model)


def build_prompt(thread_messages: list[dict[str, Any]], instructions: str | None) -> str:
    """Build the prompt from thread messages (newest first) and optional instructions.

    Returns a prompt that asks for a plain-text reply body only, no quoted original,
    no subject line inside the body.
    """
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

    Raises MessageNotFound, InstructionsTooLong, DraftingNotConfigured,
    ModelTimeout, or ModelRequestFailed on error.
    """
    if not config.is_configured():
        raise DraftingNotConfigured("Drafting is not configured.")

    if instructions and len(instructions) > 2000:
        raise InstructionsTooLong("instructions too long")

    msg = get_message(conn, message_id)
    if msg is None:
        raise MessageNotFound("no such message")

    reply_info = msg.get("reply")
    needs = reply_info["needed"] if reply_info else False
    reason = reply_info["reason"] if reply_info else None

    thread_data = get_thread(conn, message_id, store)
    if not thread_data:
        raise MessageNotFound("no such message")

    messages = thread_data["messages"][:MAX_THREAD_MESSAGES]
    used_ids = []
    capped_messages = []
    total_chars = 0

    for thread_msg in messages:
        excerpt = thread_msg.get("excerpt") or ""
        if total_chars + len(excerpt) > MAX_THREAD_CHARS:
            break
        capped_messages.append(thread_msg)
        used_ids.append(thread_msg["message_id"])
        total_chars += len(excerpt)

    if not capped_messages:
        capped_messages = messages[:1]
        used_ids = [messages[0]["message_id"]]

    prompt = build_prompt(capped_messages, instructions)

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
                "reasoning_effort": config.reasoning_effort,
            },
            timeout=config.timeout,
        )
        response.raise_for_status()
        data = response.json()
        body = data["choices"][0]["message"]["content"].strip()
        if not body:
            finish_reason = data["choices"][0].get("finish_reason")
            log.warning("draft model returned empty body", extra={"finish_reason": finish_reason})
            raise ModelRequestFailed("The model returned an empty reply.")
    except requests.Timeout as e:
        raise ModelTimeout("The model timed out.") from e
    except requests.RequestException as e:
        log.warning("draft model call failed", extra={"error": str(e)})
        raise ModelRequestFailed("The model request failed.") from e
    except (KeyError, IndexError, json.JSONDecodeError) as e:
        log.warning("draft model response parse failed", extra={"error": str(e)})
        raise ModelRequestFailed("The model response was invalid.") from e

    reply_subject = msg["subject"] or ""
    if reply_subject and not reply_subject.lower().strip().startswith("re:"):
        reply_subject = f"RE: {reply_subject}"
    elif not reply_subject:
        reply_subject = "RE:"

    to = [msg["from_email"]] if msg["from_email"] else []

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
