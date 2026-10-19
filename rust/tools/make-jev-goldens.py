"""Generate golden vectors for aimap.jev with the real Python implementation.

Writes tests/fixtures/jev_golden.json: build_questions output (with insertion
order), classifier_key hexes, header_facts, build_state and decide results for
several answer sets. The Rust port must reproduce them exactly.

Run:  python3 tools/make-jev-goldens.py [real.eml ...]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "src"))

from aimap import jev  # noqa: E402
from aimap.parse import parse_meta  # noqa: E402

FIXTURES = Path(__file__).resolve().parent.parent / "tests" / "fixtures"

PATTERNS = [
    jev.Pattern("Shop promotions", "Low", "discard", ["shopping", "promo"],
                [{"from": "Shop <deals@shop.example>", "subject": "20% off"}]),
    jev.Pattern("Client invoices", "High", "act_now", ["work", "billing"], []),
]
PROFILE = {"who": "Alex", "priorities": ["fútbol", "invoices"], "notes": "line\nbreak \"quoted\""}


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


def main() -> None:
    out: dict = {"patterns": [
        {"insight": p.insight, "importance": p.importance, "action_bucket": p.action_bucket,
         "tags": p.tags, "examples": p.examples} for p in PATTERNS],
        "profile": PROFILE,
    }

    q = jev.build_questions(PATTERNS)
    out["questions"] = q
    out["questions_no_patterns"] = jev.build_questions([])

    out["classifier_keys"] = [
        {"model": "jev-1", "profile": PROFILE, "questions_variant": "full", "key": jev.classifier_key(q, "jev-1", PROFILE)},
        {"model": "jev-2", "profile": PROFILE, "questions_variant": "full", "key": jev.classifier_key(q, "jev-2", PROFILE)},
        {"model": "jev-1", "profile": {"who": "Sam"}, "questions_variant": "full", "key": jev.classifier_key(q, "jev-1", {"who": "Sam"})},
        {"model": "jev-1", "profile": PROFILE, "questions_variant": "first_pattern", "key": jev.classifier_key(jev.build_questions(PATTERNS[:1]), "jev-1", PROFILE)},
    ]

    decisions = [
        {"name": "confident_pattern", "answers": answers(), "thresholds": None},
        {"name": "disagreement", "answers": answers(action="skim"), "thresholds": None},
        {"name": "low_confidence_judgment",
         "answers": answers(pconf=0.3, score=1.8, action="reply", tags={"work": 0.95, "promo": 0.2}),
         "thresholds": None},
        {"name": "best_tag_fallback", "answers": answers(pattern="none_of_these", tags={"work": 0.5, "promo": 0.3}),
         "thresholds": None},
        {"name": "no_patterns", "answers": {k: v for k, v in answers(tags={}).items() if k != "pattern"},
         "thresholds": None},
        {"name": "half_score_rounds_to_even", "answers": answers(score=0.5, sigs={"real_person": 1.0, "asks_to_act": 1.0}),
         "thresholds": None},
        {"name": "custom_thresholds",
         "answers": answers(pconf=0.55, tags={"work": 0.55}, sigs={}),
         "thresholds": {"pattern_confidence": 0.6, "tag_threshold": 0.5, "review_confidence": 0.2}},
        {"name": "priority_rounding", "answers": answers(score=0.1,
                                                         sigs={"real_person": 0.123456, "promotional": 0.5,
                                                               "time_sensitive": 0.111111}),
         "thresholds": None},
    ]
    out["decisions"] = []
    for case in decisions:
        t = None
        if case["thresholds"]:
            t = jev.Thresholds(**case["thresholds"])
        d = jev.decide(case["answers"], PATTERNS if "pattern" in case["answers"] or case["name"] != "no_patterns" else [],
                       t)
        out["decisions"].append({
            "name": case["name"],
            "answers": case["answers"],
            "thresholds": case["thresholds"],
            "decision": {
                "importance": d.importance, "action_bucket": d.action_bucket, "tags": d.tags,
                "insight": d.insight, "source": d.source, "needs_review": d.needs_review,
                "priority": d.priority, "signals": d.signals,
            },
        })

    # header_facts / build_state over synthetic + real stored mail
    simple = (b'From: "Ana Diaz" <Ana@Example.com>\r\nTo: me@example.com\r\nSubject:  Quarterly   report\r\n'
              b"Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m1@example.com>\r\n"
              b"List-Unsubscribe: <mailto:u@x>\r\nIn-Reply-To: <prev@x>\r\n\r\nHi there\r\n")
    out["states"] = [{"name": "synthetic", "raw_b64": __import__("base64").b64encode(simple).decode(),
                      "account": "me@example.com", "body_chars": 1500}]

    real = sys.argv[1:2]
    if real:
        raw = Path(real[0]).read_bytes()
        out["states"].append({"name": "real", "raw_b64": __import__("base64").b64encode(raw).decode(),
                              "account": "me@example.com", "body_chars": 1500})

    for s in out["states"]:
        from aimap.parse import parse_meta as pm
        meta = pm(__import__("base64").b64decode(s["raw_b64"]))
        s["header_facts"] = jev.header_facts(__import__("base64").b64decode(s["raw_b64"]), meta, s["account"])
        s["state"] = jev.build_state(PROFILE, s["account"], __import__("base64").b64decode(s["raw_b64"]),
                                     meta, s["body_chars"])

    FIXTURES.mkdir(parents=True, exist_ok=True)
    (FIXTURES / "jev_golden.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {FIXTURES / 'jev_golden.json'}")


if __name__ == "__main__":
    main()
