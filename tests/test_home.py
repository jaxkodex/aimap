from datetime import UTC, datetime, timedelta
from zoneinfo import ZoneInfo

from aimap import home
from aimap.home import Item

NOW = datetime(2026, 9, 28, 9, 0, tzinfo=UTC)
ME = "me@example.com"


def item(n, bucket, *, hours_ago=1, sender=None, priority=0.0, insight=None, tags=(), signals=None, unread=True,
         from_email=None):
    return Item(message_id=n, account=ME, from_email=from_email or f"s{n}@example.com", from_name=sender,
                subject=f"m{n}", sent_at=NOW - timedelta(hours=hours_ago), unread=unread, importance="Medium",
                action_bucket=bucket, tags=list(tags), insight=insight, priority=priority, signals=signals or {})


def test_sections_split_by_bucket():
    out = home.build([
        item(1, "act_now", priority=5), item(2, "verify", priority=8), item(3, "reply", hours_ago=5),
        item(4, "reply", hours_ago=48), item(5, "skim"), item(6, "discard"),
    ], NOW - timedelta(hours=24))
    assert [c["message_id"] for c in out["act_now"]] == [2, 1]  # highest priority first
    assert [c["message_id"] for c in out["waiting"]] == [4, 3]  # longest waiting first
    assert out["waiting"][0]["waiting_since"] == NOW - timedelta(hours=48)
    assert {g["name"] for g in out["sorted"]} == {"To skim", "Can discard"}
    brief = {k: v for k, v in out["brief"].items() if k != "by_hour"}
    assert brief == {"new": 5, "act_now": 2, "waiting": 2, "sorted": 2, "unclassified": 0, "sorted_at": None,
                     "text": "5 new emails. 2 need you now and 2 people are waiting on a reply."}


def test_own_mail_and_unlabelled_mail():
    out = home.build([item(1, "reply", from_email=ME), item(2, None)], NOW - timedelta(hours=24))
    assert out["waiting"] == [] and out["sorted"] == []
    assert out["brief"]["new"] == 1 and out["brief"]["unclassified"] == 1


def test_groups_use_insight_then_tag_then_bucket_and_summarise_senders():
    items = [
        item(1, "review", insight="Finance & receipts", sender="AWS Billing"),
        item(2, "review", insight="Finance & receipts", sender="Uber", unread=False),
        item(3, "review", insight="Finance & receipts", sender="Uber"),
        item(4, "review", insight="Finance & receipts", sender="Stripe"),
        item(5, "skim", tags=["travel"]),
        item(6, "batch_review"),
    ]
    groups = {g["name"]: g for g in home.build(items, NOW)["sorted"]}
    assert set(groups) == {"Finance & receipts", "travel", "Alerts"}
    fin = groups["Finance & receipts"]
    assert (fin["count"], fin["unread"]) == (4, 3)
    assert fin["summary"] == "Uber, AWS Billing + 1 more"
    assert list(groups)[0] == "Finance & receipts"  # biggest group first


def test_reasons_come_from_strong_signals_in_fixed_order():
    assert home.reasons({"real_person": 0.9, "time_sensitive": 0.7, "security_event": 0.1, "promotional": 0.9,
                         "pattern_confidence": 0.8}) == ["Mentions a deadline", "Written to you by a person"]
    assert home.reasons({}) == []


def test_brief_text():
    assert home.brief_text(0, 0, 0) == "Nothing new. You're all caught up."
    assert home.brief_text(1, 1, 1) == "1 new email. 1 needs you now and 1 person is waiting on a reply."
    assert home.brief_text(12, 0, 0) == "12 new emails. None of them need you right now."


def test_by_hour_counts_todays_arrivals_per_section():
    items = [item(1, "act_now", hours_ago=1), item(2, "reply", hours_ago=1), item(3, "skim", hours_ago=1),
             item(4, "discard", hours_ago=3), item(5, None, hours_ago=3), item(6, "reply", hours_ago=10),
             item(7, "act_now", from_email=ME)]
    hours = home.build(items, NOW, now=NOW)["brief"]["by_hour"]
    assert [h["hour"] for h in hours] == list(range(24))
    assert hours[8] == {"hour": 8, "act_now": 1, "waiting": 1, "sorted": 1, "unclassified": 0}
    assert hours[6] == {"hour": 6, "act_now": 0, "waiting": 0, "sorted": 1, "unclassified": 1}
    # 23:00 yesterday and the account's own mail are not today's traffic.
    assert sum(h[k] for h in hours for k in ("act_now", "waiting", "sorted", "unclassified")) == 5


def test_by_hour_follows_the_callers_time_zone():
    madrid = ZoneInfo("Europe/Madrid")  # UTC+2 in September
    hours = home.build([item(1, "reply", hours_ago=10)], NOW, now=NOW, tz=madrid)["brief"]["by_hour"]
    assert hours[1]["waiting"] == 1  # 23:00 UTC yesterday is 01:00 today in Madrid


def test_cards_carry_the_account_profile_and_brief_carries_sorted_at():
    card = Item(message_id=1, account=ME, from_email="a@example.com", from_name=None, subject="s", sent_at=NOW,
                unread=True, action_bucket="act_now", profile="work")
    out = home.build([card], NOW, now=NOW, sorted_at=NOW)
    assert out["act_now"][0]["profile"] == "work"
    assert out["brief"]["sorted_at"] == NOW
