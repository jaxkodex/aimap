from conftest import email_bytes

from aimap import jev
from aimap.parse import parse_meta

PATTERNS = [
    jev.Pattern("Shop promotions", "Low", "discard", ["shopping", "promo"],
                [{"from": "Shop <deals@shop.example>", "subject": "20% off"}]),
    jev.Pattern("Client invoices", "High", "act_now", ["work", "billing"]),
]


def answers(pattern="Shop promotions", pconf=0.9, score=0.1, iconf=0.9, action="discard", aconf=0.9,
            tags=None, sigs=None):
    a = {
        "importance": {"score": score, "confidence": iconf},
        "action": {"choice": action, "confidence": aconf},
        "pattern": {"choice": pattern, "confidence": pconf,
                    "probabilities": {"Shop promotions": 0.6, "Client invoices": 0.1, "none_of_these": 0.3}},
    }
    for t, p in (tags if tags is not None else {"shopping": 0.9, "promo": 0.85, "work": 0.1}).items():
        a["tag:" + t] = {"noul": p}
    for s, p in (sigs or {"promotional": 1.0, "real_person": 0.0}).items():
        a["sig:" + s] = {"noul": p}
    return a


def test_questions_cover_patterns_tags_and_signals():
    q = jev.build_questions(PATTERNS)
    assert set(q["pattern"]["criteria"]) == {"Shop promotions", "Client invoices", "none_of_these"}
    assert {"tag:shopping", "tag:promo", "tag:work", "tag:billing"} <= q.keys()
    assert all("sig:" + s in q for s in jev.SIGNALS)
    assert q["tag:promo"]["instructions"]["emails_that_have_this_label"][0]["subject"] == "20% off"


def test_no_patterns_means_no_pattern_or_tag_questions():
    q = jev.build_questions([])
    assert "pattern" not in q and not any(k.startswith("tag:") for k in q)
    d = jev.decide({k: v for k, v in answers(tags={}).items() if k != "pattern"}, [])
    assert d.source == "judgment" and d.needs_review and d.tags == []


def test_confident_pattern_is_copied():
    d = jev.decide(answers(), PATTERNS)
    assert (d.source, d.importance, d.action_bucket, d.tags) == ("pattern", "Low", "discard", ["shopping", "promo"])
    assert not d.needs_review
    assert d.priority == round(-3.0 + 2 * 0.1, 2)


def test_disagreement_flags_review():
    d = jev.decide(answers(action="skim"), PATTERNS)
    assert d.source == "pattern" and d.needs_review


def test_low_confidence_falls_back_to_judgment():
    d = jev.decide(answers(pconf=0.3, score=1.8, action="reply", tags={"work": 0.95, "promo": 0.2}), PATTERNS)
    assert (d.source, d.importance, d.action_bucket, d.tags, d.insight) == (
        "judgment", "High", "reply", ["work"], None)
    assert d.needs_review and d.signals["closest_pattern"] == "Shop promotions"


def test_best_tag_used_when_none_passes_threshold():
    d = jev.decide(answers(pattern="none_of_these", tags={"work": 0.5, "promo": 0.3}), PATTERNS)
    assert d.tags == ["work"]


def test_classifier_key_changes_with_inputs():
    q = jev.build_questions(PATTERNS)
    k = jev.classifier_key(q, "jev-1", {"who": "Alex"})
    assert k == jev.classifier_key(jev.build_questions(PATTERNS), "jev-1", {"who": "Alex"})
    assert k != jev.classifier_key(q, "jev-2", {"who": "Alex"})
    assert k != jev.classifier_key(q, "jev-1", {"who": "Sam"})
    assert k != jev.classifier_key(jev.build_questions(PATTERNS[:1]), "jev-1", {"who": "Alex"})


def test_state_has_facts_and_body():
    raw = email_bytes(1, sender="me@example.com", extra="List-Id: x\r\n", body="Please confirm the meeting.")
    s = jev.build_state({"who": "Alex"}, "me@example.com", raw, parse_meta(raw))
    assert s["recipient_profile"] == {"who": "Alex"}
    assert s["email"]["facts"]["sent_by_recipient_themself"] and s["email"]["facts"]["bulk_mail_headers"]
    assert s["email"]["body"] == "Please confirm the meeting."
