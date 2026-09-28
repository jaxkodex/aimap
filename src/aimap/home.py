"""Turn labelled messages into the app's Home screen. Pure: no database, no Jev.

    act_now   act_now and verify buckets, highest priority first.
    waiting   reply bucket, longest waiting first. Until Sent mail is synced this
              means "someone expects an answer", not "you have not replied".
    sorted    everything else, grouped by pattern insight, first tag, or bucket.

Messages the account sent itself never appear. Reasons and summaries are
templates over stored fields, so they cost no inference.
"""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

ACT_NOW = ("act_now", "verify")
WAITING = ("reply",)

# Group name for a message with neither a pattern nor tags.
BUCKET_GROUPS = {
    "discard": "Can discard",
    "batch_review": "Alerts",
    "skim": "To skim",
    "review": "To review",
    "act_now": "Act now",
    "verify": "Verify",
    "reply": "Replies",
}

# Signal -> reason, in the order they are shown. A signal counts at or above REASON_THRESHOLD.
REASONS = [
    ("security_event", "Security event on your account"),
    ("time_sensitive", "Mentions a deadline"),
    ("asks_to_act", "Asks you to do something"),
    ("real_person", "Written to you by a person"),
    ("active_priority", "About one of your priorities"),
]
REASON_THRESHOLD = 0.5


@dataclass(frozen=True)
class Item:
    message_id: int
    account: str
    from_email: str | None
    from_name: str | None
    subject: str | None
    sent_at: datetime | None
    unread: bool
    importance: str | None = None
    action_bucket: str | None = None
    tags: list[str] = field(default_factory=list)
    insight: str | None = None
    needs_review: bool = False
    priority: float = 0.0
    signals: dict[str, Any] = field(default_factory=dict)

    @property
    def sender(self) -> str:
        return self.from_name or self.from_email or "(unknown)"

    @property
    def from_self(self) -> bool:
        return (self.from_email or "").lower() == self.account.lower()


def reasons(signals: dict[str, Any]) -> list[str]:
    return [text for name, text in REASONS
            if isinstance(signals.get(name), int | float) and signals[name] >= REASON_THRESHOLD]


def group_name(item: Item) -> str:
    if item.insight:
        return item.insight
    if item.tags:
        return item.tags[0]
    return BUCKET_GROUPS.get(item.action_bucket or "", "Other")


def group_summary(items: list[Item], senders: int = 2) -> str:
    """"AWS Billing, Uber + 3 more": the most frequent senders, then a count of the rest."""
    counts = Counter(i.sender for i in items)
    top = [name for name, _ in counts.most_common(senders)]
    rest = len(counts) - len(top)
    return ", ".join(top) + (f" + {rest} more" if rest else "")


def _card(item: Item) -> dict[str, Any]:
    return {
        "message_id": item.message_id, "account": item.account, "sender": item.sender,
        "from_email": item.from_email, "subject": item.subject, "sent_at": item.sent_at, "unread": item.unread,
        "action_bucket": item.action_bucket, "importance": item.importance, "priority": item.priority,
        "needs_review": item.needs_review, "reasons": reasons(item.signals),
    }


def _ts(item: Item) -> float:
    return item.sent_at.timestamp() if item.sent_at else 0.0


def build(items: list[Item], new_since: datetime) -> dict[str, Any]:
    """Home sections for the labelled messages in the window. Unlabelled ones only count as new."""
    inbox = [i for i in items if not i.from_self]
    labelled = [i for i in inbox if i.action_bucket]
    act = sorted((i for i in labelled if i.action_bucket in ACT_NOW), key=lambda i: (-i.priority, -_ts(i)))
    waiting = sorted((i for i in labelled if i.action_bucket in WAITING), key=_ts)
    rest = [i for i in labelled if i.action_bucket not in ACT_NOW + WAITING]

    groups: dict[str, list[Item]] = {}
    for i in rest:
        groups.setdefault(group_name(i), []).append(i)
    sorted_groups = [
        {"name": name, "count": len(g), "unread": sum(i.unread for i in g),
         "latest_at": max((i.sent_at for i in g if i.sent_at), default=None), "summary": group_summary(g)}
        for name, g in sorted(groups.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    ]

    new = sum(1 for i in inbox if i.sent_at and i.sent_at >= new_since)
    return {
        "brief": {
            "new": new, "act_now": len(act), "waiting": len(waiting), "sorted": len(rest),
            "unclassified": len(inbox) - len(labelled),
            "text": brief_text(new, len(act), len(waiting)),
        },
        "act_now": [_card(i) for i in act],
        "waiting": [{**_card(i), "waiting_since": i.sent_at} for i in waiting],
        "sorted": sorted_groups,
    }


def brief_text(new: int, act: int, waiting: int) -> str:
    if not new and not act and not waiting:
        return "Nothing new. You're all caught up."
    parts = [f"{act} need{'s' if act == 1 else ''} you now" if act else None,
             f"{waiting} {'person is' if waiting == 1 else 'people are'} waiting on a reply" if waiting else None]
    parts = [p for p in parts if p]
    head = f"{new} new email{'' if new == 1 else 's'}."
    if not parts:
        return head + " None of them need you right now."
    return head + " " + " and ".join(parts) + "."
