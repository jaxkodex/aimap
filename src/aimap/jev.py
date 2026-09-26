"""Questions for TypeSafe Jev and the code that turns its answers into labels.

One Jev request per message. Every question goes into that request, so they are
answered in parallel against the same state:

    pattern      Choice over the profile's known patterns plus `none_of_these`. A
                 confident match copies that pattern's labels as they are.
    importance   Score, Low / Medium / High, judged against the recipient profile.
    action       Choice over the ACTIONS buckets.
    tag:<name>   One Noul per tag used by the profile's patterns (several can apply).
    sig:<name>   Generic Nouls that feed a weighted priority computed in code.

`decide` uses the pattern if Jev picked one with enough confidence. Otherwise it
falls back to the independent judgments and flags the message for review. Only
this module knows the question layout. The rest of the service handles plain dicts.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from typing import Any

from aimap.parse import MessageMeta, attachment_names, body_text

ACTIONS = {
    "discard": "Nothing to do. Delete or unsubscribe: marketing, promos, vanity stats, generic news, "
               "spent one-time codes.",
    "batch_review": "Automated alert tied to one of the recipient's saved searches or watch lists. "
                    "Look at it in a weekly batch.",
    "skim": "Informational and possibly relevant (newsletters from providers the recipient uses, bulletins, "
            "price trackers). Read quickly; no action expected.",
    "review": "Read with care: an account, policy, process or relationship update that may matter but has "
              "no immediate deadline.",
    "verify": "A security or account event (new login, card added, token activated, charge). Confirm it was "
              "the recipient, then archive.",
    "act_now": "Needs the recipient to do something soon: confirm an appointment, upload or sign documents, "
               "fix a service that is about to break or be deleted.",
    "reply": "A real person wrote directly to the recipient and is waiting for an answer.",
}

IMPORTANCE_LEVELS = ["Low", "Medium", "High"]
IMPORTANCE_CRITERIA = [
    "Low: marketing, coupons, vanity metrics, generic news or digests, or anything unrelated to "
    "`recipient_profile`. Safe to delete unread.",
    "Medium: relevant to the recipient's priorities or to an account they use, but nothing breaks if it waits "
    "a few days (automated alerts, provider policy updates, statements, bulletins).",
    "High: needs the recipient's attention soon: a real person waiting on a reply, a deadline or appointment, "
    "a security alert, or a service about to fail.",
]

# name -> (question, weight in the priority sum)
SIGNALS: dict[str, tuple[str, float]] = {
    "real_person": ("Was `email.body` written personally by a human to the recipient, rather than sent by an "
                    "automated system or a marketing list?", 3.0),
    "asks_to_act": ("Does `email` ask the recipient to do something specific (reply, confirm, upload, sign, "
                    "pay, fix)?", 2.0),
    "time_sensitive": ("Does `email` mention a deadline, an appointment, or a consequence if the recipient "
                       "does nothing soon?", 2.0),
    "security_event": ("Does `email` report a security or account event on the recipient's own account, such "
                       "as a login, access grant, card added, token activated or charge?", 2.0),
    "active_priority": ("Is `email` directly about one of the priorities in `recipient_profile`?", 1.5),
    "promotional": ("Is the main purpose of `email` to sell or promote something?", -3.0),
}

NONE_OF_THESE = "none_of_these"
_NOREPLY = re.compile(r"no-?reply|noresponder|notification|newsletter|info@|news")


@dataclass(frozen=True)
class Pattern:
    insight: str
    importance: str
    action_bucket: str
    tags: list[str] = field(default_factory=list)
    examples: list[dict] = field(default_factory=list)  # [{"from": ..., "subject": ...}], invented or real

    def validate(self) -> None:
        if self.importance not in IMPORTANCE_LEVELS:
            raise ValueError(f"pattern {self.insight!r}: importance must be one of {IMPORTANCE_LEVELS}")
        if self.action_bucket not in ACTIONS:
            raise ValueError(f"pattern {self.insight!r}: action_bucket must be one of {sorted(ACTIONS)}")
        if self.insight == NONE_OF_THESE:
            raise ValueError(f"{NONE_OF_THESE!r} is reserved")


@dataclass(frozen=True)
class Thresholds:
    pattern_confidence: float = 0.5  # accept a known pattern at or above this Choice confidence
    tag_threshold: float = 0.8       # Noul probability for a tag to apply
    review_confidence: float = 0.4   # flag for review below this importance/action confidence


@dataclass(frozen=True)
class Decision:
    importance: str
    action_bucket: str
    tags: list[str]
    insight: str | None
    source: str  # "pattern" | "judgment"
    needs_review: bool
    priority: float
    signals: dict[str, Any]


def tag_examples(patterns: list[Pattern], per_tag: int = 3) -> dict[str, list[dict]]:
    ex: dict[str, list[dict]] = {}
    for p in patterns:
        for t in p.tags:
            bucket = ex.setdefault(t, [])
            for e in p.examples:
                if len(bucket) < per_tag:
                    bucket.append(e)
    return dict(sorted(ex.items()))


def build_questions(patterns: list[Pattern]) -> dict[str, dict]:
    """All questions for one message, as raw dicts so the payload is easy to hash."""
    q: dict[str, dict] = {}
    if patterns:
        criteria: dict[str, Any] = {p.insight: {"tags": p.tags, "example_emails": p.examples} for p in patterns}
        criteria[NONE_OF_THESE] = ("The email does not match any of the other patterns: a different sender "
                                   "type, topic or purpose.")
        q["pattern"] = {
            "type": "choice",
            "instructions": {
                "question": "Which known inbox pattern does `email` belong to?",
                "how_to_decide": "Match on the sender's purpose and topic, using each option's tags and example "
                                 f"emails. Pick `{NONE_OF_THESE}` if no option describes this kind of email.",
            },
            "criteria": criteria,
        }
    q["importance"] = {
        "type": "score",
        "instructions": "How important is `email` to the recipient described in `recipient_profile`?",
        "criteria": IMPORTANCE_CRITERIA,
    }
    q["action"] = {
        "type": "choice",
        "instructions": "What should the recipient described in `recipient_profile` do with `email`?",
        "criteria": ACTIONS,
    }
    for tag, examples in tag_examples(patterns).items():
        q["tag:" + tag] = {
            "type": "noul",
            "instructions": {
                "tag": tag,
                "question": "Does the inbox label `tag` apply to `email`?",
                "emails_that_have_this_label": examples,
            },
        }
    for name, (question, _) in SIGNALS.items():
        q["sig:" + name] = {"type": "noul", "instructions": question}
    return q


def classifier_key(questions: dict, model: str, profile: dict) -> str:
    """Same questions, model and profile give the same key, so a message is not classified twice."""
    blob = json.dumps({"q": questions, "m": model, "p": profile}, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(blob.encode()).hexdigest()[:32]


def header_facts(raw: bytes, meta: MessageMeta, account: str) -> dict:
    """Facts code can compute exactly, so Jev does not have to guess them."""
    sender = (meta.from_email or "").lower()
    return {
        "sent_by_recipient_themself": sender == account.lower(),
        "is_reply_in_thread": bool(meta.in_reply_to),
        "bulk_mail_headers": meta.has_list_headers,
        "sender_is_noreply_address": bool(_NOREPLY.search(sender)),
        "attachments": attachment_names(raw),
    }


def build_state(profile: dict, account: str, raw: bytes, meta: MessageMeta, body_chars: int = 1500) -> dict:
    sender = f"{meta.from_name} <{meta.from_email}>" if meta.from_name else (meta.from_email or "(unknown)")
    return {
        "recipient_profile": profile,
        "email": {
            "to_account": account,
            "from": sender,
            "subject": meta.subject or "",
            "facts": header_facts(raw, meta, account),
            "body": body_text(raw, body_chars) or "(no text body)",
        },
    }


def decide(answers: dict, patterns: list[Pattern], t: Thresholds | None = None) -> Decision:
    """Turn raw Jev answers into labels. Pure, so stored answers can be re-decided later."""
    t = t or Thresholds()
    by_insight = {p.insight: p for p in patterns}
    imp, act, pat = answers["importance"], answers["action"], answers.get("pattern")

    tag_probs = {k[4:]: a["noul"] for k, a in answers.items() if k.startswith("tag:")}
    judged_tags = [tg for tg, p in sorted(tag_probs.items(), key=lambda kv: -kv[1]) if p >= t.tag_threshold]
    if not judged_tags and tag_probs:
        judged_tags = [max(tag_probs, key=tag_probs.get)]

    signals = {k[4:]: a["noul"] for k, a in answers.items() if k.startswith("sig:")}
    priority = sum(SIGNALS[n][1] * p for n, p in signals.items() if n in SIGNALS)
    priority += 2.0 * imp["score"]  # score is 0..2 over Low/Medium/High

    judged_importance = IMPORTANCE_LEVELS[min(2, max(0, round(imp["score"])))]
    use_pattern = (pat is not None and pat["choice"] in by_insight
                   and pat["confidence"] >= t.pattern_confidence)

    if use_pattern:
        p = by_insight[pat["choice"]]
        importance, action_bucket, tags, insight, source = p.importance, p.action_bucket, p.tags, p.insight, "pattern"
        # Two independent routes disagreeing is a cheap, useful review signal.
        disagree = judged_importance != importance or act["choice"] != action_bucket
    else:
        importance, action_bucket, tags, source, disagree = judged_importance, act["choice"], judged_tags, \
            "judgment", False
        insight = None

    needs_review = (not use_pattern) or disagree or imp["confidence"] < t.review_confidence \
        or act["confidence"] < t.review_confidence

    closest = None
    if pat is not None:
        closest = max((k for k in pat.get("probabilities", {}) if k != NONE_OF_THESE),
                      key=lambda k: pat["probabilities"][k], default=None)
    return Decision(
        importance=importance, action_bucket=action_bucket, tags=list(tags), insight=insight, source=source,
        needs_review=needs_review, priority=round(priority, 2),
        signals={
            **{k: round(v, 3) for k, v in signals.items()},
            "pattern_confidence": round(pat["confidence"], 3) if pat else None,
            "closest_pattern": closest,
            "judged_importance": judged_importance,
            "importance_score": round(imp["score"], 3),
            "importance_confidence": round(imp["confidence"], 3),
            "judged_action": act["choice"],
            "action_confidence": round(act["confidence"], 3),
            "judged_tags": judged_tags,
        },
    )
