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
