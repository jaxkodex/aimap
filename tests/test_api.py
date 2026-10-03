from datetime import UTC, datetime, timedelta

import pytest
from conftest import FakeMailbox, email_bytes
from fastapi.testclient import TestClient
from psycopg.types.json import Jsonb

from aimap.api import create_app
from aimap.auth import AuthError, Forbidden, User
from aimap.ingest import sync_mailbox

ME = "me@example.com"
AUTH = {"Authorization": "Bearer good"}


class FakeVerifier:
    def verify(self, token):
        if token == "good":
            return User(uid="u1", email=ME)
        if token == "stranger":
            raise Forbidden("email not allowed")
        raise AuthError("invalid token")


def _date(dt: datetime) -> str:
    return dt.strftime("%a, %d %b %Y %H:%M:%S +0000")


@pytest.fixture
def seeded(store, pg_catalog, pool):
    """Four messages from the last day: act_now, reply, skim (read), and one not classified yet."""
    now = datetime.now(UTC).replace(microsecond=0)
    specs = [
        (1, "Priya <priya@legal.example>", "Sign-off needed", "act_now", 9.0, {"time_sensitive": 0.9}),
        (2, "Elena <elena@example.com>", "Offer letter", "reply", 4.0, {"real_person": 0.95}),
        (3, "News <news@list.example>", "Weekly digest", "skim", -1.0, {"promotional": 0.2}),
        (4, "Sam <sam@example.com>", "PR notes", None, 0, {}),
    ]
    raws = {n: email_bytes(n, sender=s, subject=subj, body=f"Body of message {n}.",
                           extra=f"Date: {_date(now - timedelta(hours=n))}\r\n").replace(
        b"Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n", b"", 1) for n, s, subj, *_ in specs}
    sync_mailbox(FakeMailbox({"INBOX": (1, raws)}), store, pg_catalog, ME, "INBOX", initial_fetch_count=0)
    with pool.connection() as conn:
        ids = dict(conn.execute("SELECT subject, id FROM messages").fetchall())
        pid = conn.execute("SELECT id FROM profiles WHERE name = 'default'").fetchone()[0]
        # FakeMailbox marks everything \Seen. Leave only the digest read.
        conn.execute("UPDATE message_locations SET flags = '{}' WHERE message_id <> %s", (ids["Weekly digest"],))
        for _, _, subj, bucket, priority, signals in specs:
            if bucket:
                conn.execute("""
                    INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
                        importance, action_bucket, tags, insight, source, needs_review, priority, signals)
                    VALUES (%s, %s, 'k', 'jev-test', '{}', 'Medium', %s, %s, NULL, 'judgment', false, %s, %s)""",
                             (ids[subj], pid, bucket, ["newsletter"] if bucket == "skim" else [], priority,
                              Jsonb(signals)))
    return ids


@pytest.fixture
def client(pool, store):
    return TestClient(create_app(pool, store, FakeVerifier()))


def test_healthz_needs_no_token(client):
    assert client.get("/healthz").json() == {"ok": True}


@pytest.mark.parametrize("headers, status", [
    ({}, 401), ({"Authorization": "Basic x"}, 401), ({"Authorization": "Bearer bad"}, 401),
    ({"Authorization": "Bearer stranger"}, 403),
])
def test_every_data_route_needs_an_allowed_user(client, headers, status):
    for path in ("/me", "/home", "/messages", "/messages/1", "/messages/1/body", "/accounts"):
        assert client.get(path, headers=headers).status_code == status, path


@pytest.mark.parametrize("headers, status", [
    ({}, 401), ({"Authorization": "Bearer bad"}, 401), ({"Authorization": "Bearer stranger"}, 403),
])
def test_actions_need_an_allowed_user(client, headers, status):
    assert client.post("/messages/1/actions", json={"action": "handled"}, headers=headers).status_code == status


def test_me(client):
    assert client.get("/me", headers=AUTH).json() == {"uid": "u1", "email": ME}


def test_home(client, seeded):
    body = client.get("/home", headers=AUTH).json()
    assert [c["subject"] for c in body["act_now"]] == ["Sign-off needed"]
    assert body["act_now"][0]["reasons"] == ["Mentions a deadline"]
    assert [c["subject"] for c in body["waiting"]] == ["Offer letter"]
    assert body["sorted"] == [{"name": "newsletter", "count": 1, "unread": 0, "latest_at": body["sorted"][0]
                               ["latest_at"], "summary": "News"}]
    assert body["brief"]["new"] == 4 and body["brief"]["unclassified"] == 1
    assert body["act_now"][0]["profile"] == "default"
    assert body["brief"]["sorted_at"] is not None
    assert len(body["brief"]["by_hour"]) == 24


def act(client, message_id, action):
    return client.post(f"/messages/{message_id}/actions", json={"action": action}, headers=AUTH)


def test_action_round_trip_is_idempotent(client, seeded, pool):
    mid = seeded["Sign-off needed"]
    first = act(client, mid, "handled").json()
    assert first["message_id"] == mid and first["state"] == "handled" and first["changed_at"]
    assert act(client, mid, "handled").json()["state"] == "handled"
    assert act(client, mid, "later").json()["state"] == "later"
    assert client.get(f"/messages/{mid}", headers=AUTH).json()["state"] == "later"

    assert act(client, mid, "undo").json() == {"message_id": mid, "state": None, "changed_at": None}
    assert act(client, mid, "undo").json()["state"] is None  # undo twice is still fine
    assert client.get(f"/messages/{mid}", headers=AUTH).json()["state"] is None
    with pool.connection() as conn:
        assert conn.execute("SELECT count(*) FROM message_state").fetchone()[0] == 0


def test_action_records_the_signed_in_email(client, seeded, pool):
    act(client, seeded["Offer letter"], "later")
    with pool.connection() as conn:
        assert conn.execute("SELECT changed_by FROM message_state").fetchall() == [(ME,)]


def test_action_rejects_unknown_ids_and_bad_actions(client, seeded):
    assert act(client, 999999, "handled").status_code == 404
    assert act(client, seeded["PR notes"], "archive").status_code == 422
    assert client.post(f"/messages/{seeded['PR notes']}/actions", json={}, headers=AUTH).status_code == 422


def test_home_leaves_handled_messages_out(client, seeded):
    act(client, seeded["Sign-off needed"], "handled")
    act(client, seeded["Offer letter"], "handled")
    body = client.get("/home", headers=AUTH).json()
    assert body["act_now"] == [] and body["waiting"] == []
    assert body["brief"]["act_now"] == 0 and body["brief"]["waiting"] == 0
    assert body["brief"]["handled_today"] == 2
    assert body["brief"]["new"] == 4  # they still arrived
    other = client.get("/home", params={"account": "other@example.com"}, headers=AUTH).json()
    assert other["brief"]["handled_today"] == 0  # handled_today follows the account filter


def test_home_counts_only_todays_handled(client, seeded, pool):
    act(client, seeded["Sign-off needed"], "handled")
    assert client.get("/home", headers=AUTH).json()["brief"]["handled_today"] == 1
    with pool.connection() as conn:  # yesterday's work is not today's count
        conn.execute("UPDATE message_state SET changed_at = now() - interval '2 days'")
    assert client.get("/home", headers=AUTH).json()["brief"]["handled_today"] == 0
    act(client, seeded["Offer letter"], "later")  # later is not handled
    assert client.get("/home", headers=AUTH).json()["brief"]["handled_today"] == 0


def test_home_sorts_later_cards_last(client, seeded, pool):
    with pool.connection() as conn:  # a second act_now card, lower priority than the seeded one
        conn.execute("""
            INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
                importance, action_bucket, tags, source, needs_review, priority, signals)
            SELECT %s, id, 'k2', 'jev-test', '{}', 'Medium', 'act_now', '{}', 'judgment', false, 1.0, '{}'
            FROM profiles WHERE name = 'default'""", (seeded["PR notes"],))
    assert [c["subject"] for c in client.get("/home", headers=AUTH).json()["act_now"]] == [
        "Sign-off needed", "PR notes"]

    act(client, seeded["Sign-off needed"], "later")
    cards = client.get("/home", headers=AUTH).json()["act_now"]
    assert [c["subject"] for c in cards] == ["PR notes", "Sign-off needed"]
    assert [c["state"] for c in cards] == [None, "later"]

    act(client, seeded["PR notes"], "later")  # marked last, so it now sorts last
    assert [c["subject"] for c in client.get("/home", headers=AUTH).json()["act_now"]] == [
        "Sign-off needed", "PR notes"]


def test_home_traffic_uses_the_time_zone(client, seeded):
    body = client.get("/home", params={"tz": "Pacific/Kiritimati"}, headers=AUTH).json()
    assert [h["hour"] for h in body["brief"]["by_hour"]] == list(range(24))
    assert client.get("/home", params={"tz": "Mars/Olympus"}, headers=AUTH).status_code == 400


def test_home_filters_by_account(client, seeded):
    body = client.get("/home", params={"account": "other@example.com"}, headers=AUTH).json()
    assert body["act_now"] == [] and body["brief"]["new"] == 0


def test_messages_pages_newest_first(client, seeded):
    first = client.get("/messages", params={"limit": 3}, headers=AUTH).json()
    assert [m["subject"] for m in first["messages"]] == ["Sign-off needed", "Offer letter", "Weekly digest"]
    assert first["next_cursor"]
    rest = client.get("/messages", params={"limit": 3, "cursor": first["next_cursor"]}, headers=AUTH).json()
    assert [m["subject"] for m in rest["messages"]] == ["PR notes"]
    assert rest["messages"][0]["action_bucket"] is None and rest["next_cursor"] is None


def test_messages_filters(client, seeded):
    unread = client.get("/messages", params={"filter": "unread"}, headers=AUTH).json()["messages"]
    assert "Weekly digest" not in [m["subject"] for m in unread] and len(unread) == 3
    assert client.get("/messages", params={"filter": "bogus"}, headers=AUTH).status_code == 422
    assert client.get("/messages", params={"cursor": "%%%"}, headers=AUTH).status_code == 400


def test_message_detail_and_body(client, seeded):
    mid = seeded["Sign-off needed"]
    m = client.get(f"/messages/{mid}", headers=AUTH).json()
    assert m["sender"] == "Priya" and m["labels"]["action_bucket"] == "act_now"
    assert m["account"] == ME and m["profile"] == "default"  # the same profile name Home cards carry
    assert m["labels"]["reasons"] == ["Mentions a deadline"]
    assert m["mailboxes"] == [{"mailbox": "INBOX", "flags": []}] and "s3_key" not in m
    assert client.get(f"/messages/{mid}/body", headers=AUTH).json()["text"] == "Body of message 1."
    assert client.get("/messages/999999", headers=AUTH).status_code == 404


def test_body_missing_from_bucket_is_404(client, seeded, store, s3):
    mid = seeded["PR notes"]
    for obj in s3.list_objects_v2(Bucket=store.cfg.bucket)["Contents"]:
        s3.delete_object(Bucket=store.cfg.bucket, Key=obj["Key"])
    assert client.get(f"/messages/{mid}/body", headers=AUTH).status_code == 404


def test_accounts(client, seeded):
    assert client.get("/accounts", headers=AUTH).json() == {
        "accounts": [{"address": ME, "profile": "default", "messages": 4, "unread": 3}]}


def test_reply_detection_on_act_now_with_real_person(client, seeded):
    mid = seeded["Sign-off needed"]
    m = client.get(f"/messages/{mid}", headers=AUTH).json()
    assert m["reply"] == {"needed": True, "reason": "Mentions a deadline"}


def test_reply_detection_on_reply_bucket(client, seeded):
    mid = seeded["Offer letter"]
    m = client.get(f"/messages/{mid}", headers=AUTH).json()
    assert m["reply"]["needed"] is True


def test_reply_detection_on_newsletter(client, seeded):
    mid = seeded["Weekly digest"]
    m = client.get(f"/messages/{mid}", headers=AUTH).json()
    assert m["reply"] == {"needed": False, "reason": None}


def test_reply_absent_when_not_classified(client, seeded):
    mid = seeded["PR notes"]
    m = client.get(f"/messages/{mid}", headers=AUTH).json()
    assert "reply" not in m or m["reply"] is None


def test_thread_endpoint(client, seeded, pool):
    mid = seeded["Sign-off needed"]
    resp = client.get(f"/messages/{mid}/thread", headers=AUTH)
    assert resp.status_code == 200
    data = resp.json()
    assert data["message_id"] == mid
    assert "thread_id" in data
    assert len(data["messages"]) >= 1
    msg = data["messages"][0]
    assert msg["message_id"] == mid
    assert msg["sender"] == "Priya"
    assert msg["from_recipient"] is False
    assert "excerpt" in msg


def test_thread_endpoint_404(client):
    assert client.get("/messages/999999/thread", headers=AUTH).status_code == 404
