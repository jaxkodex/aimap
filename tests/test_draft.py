from datetime import UTC, datetime, timedelta
from unittest.mock import MagicMock, patch

import pytest
import requests
from conftest import FakeMailbox, email_bytes
from fastapi.testclient import TestClient
from psycopg.types.json import Jsonb

from aimap.api import create_app
from aimap.auth import User
from aimap.draft import DraftConfig
from aimap.ingest import sync_mailbox

ME = "me@example.com"
AUTH = {"Authorization": "Bearer good"}


class FakeVerifier:
    def verify(self, token):
        if token == "good":
            return User(uid="u1", email=ME)
        raise Exception("invalid token")


def _date(dt: datetime) -> str:
    return dt.strftime("%a, %d %b %Y %H:%M:%S +0000")


@pytest.fixture
def seeded(store, pg_catalog, pool):
    """A message needing a reply."""
    now = datetime.now(UTC).replace(microsecond=0)
    raw = email_bytes(1, sender="Bob <bob@example.com>", subject="Interview slot",
                      body="Can you do Tuesday at 3pm?",
                      extra=f"Date: {_date(now - timedelta(hours=1))}\r\n").replace(
        b"Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n", b"", 1)
    sync_mailbox(FakeMailbox({"INBOX": (1, {1: raw})}), store, pg_catalog, ME, "INBOX", initial_fetch_count=0)
    with pool.connection() as conn:
        mid = conn.execute("SELECT id FROM messages WHERE subject = 'Interview slot'").fetchone()[0]
        pid = conn.execute("SELECT id FROM profiles WHERE name = 'default'").fetchone()[0]
        conn.execute("""
            INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
                importance, action_bucket, tags, insight, source, needs_review, priority, signals)
            VALUES (%s, %s, 'k', 'jev-test', '{}', 'High', 'reply', '{}', NULL, 'judgment', false, 5.0, %s)""",
                     (mid, pid, Jsonb({"real_person": 0.95})))
    return mid


@pytest.fixture
def draft_config():
    return DraftConfig(
        base_url="https://api.example.com/v1",
        api_key="test-key",
        model="test-model",
        timeout=30.0,
        max_tokens=500,
    )


@pytest.fixture
def client(pool, store, draft_config):
    return TestClient(create_app(pool, store, FakeVerifier(), draft_config))


def test_draft_happy_path(client, seeded):
    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_response.json.return_value = {
        "choices": [{"message": {"content": "Hi Bob,\n\nTuesday at 3pm works for me.\n\nThanks!"}}]
    }
    
    with patch("requests.post", return_value=mock_response) as mock_post:
        resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
        assert resp.status_code == 200
        data = resp.json()
        assert data["message_id"] == seeded
        assert data["needs_reply"] is True
        assert data["to"] == ["bob@example.com"]
        assert data["cc"] == []
        assert data["subject"] == "RE: Interview slot"
        assert "Tuesday at 3pm works" in data["body"]
        assert data["model"] == "test-model"
        assert seeded in data["used_message_ids"]
        assert "created_at" in data
        
        # Check the API was called correctly
        assert mock_post.call_count == 1
        call_kwargs = mock_post.call_args.kwargs
        assert "test-key" in call_kwargs["headers"]["Authorization"]
        assert call_kwargs["json"]["model"] == "test-model"


def test_draft_with_instructions(client, seeded):
    mock_response = MagicMock()
    mock_response.json.return_value = {
        "choices": [{"message": {"content": "Wednesday would work better for me."}}]
    }
    
    with patch("requests.post", return_value=mock_response):
        resp = client.post(f"/messages/{seeded}/draft",
                          json={"instructions": "Say Wednesday works better"},
                          headers=AUTH)
        assert resp.status_code == 200
        assert "Wednesday" in resp.json()["body"]


def test_draft_instructions_too_long(client, seeded):
    resp = client.post(f"/messages/{seeded}/draft",
                      json={"instructions": "x" * 2001},
                      headers=AUTH)
    assert resp.status_code == 422


def test_draft_message_not_found(client):
    resp = client.post("/messages/999999/draft", json={}, headers=AUTH)
    assert resp.status_code == 404


def test_draft_not_configured(pool, store, seeded):
    # No draft config
    client = TestClient(create_app(pool, store, FakeVerifier(), None))
    resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
    assert resp.status_code == 503
    assert resp.json()["detail"] == "Drafting is not configured."


def test_draft_model_timeout(client, seeded):
    with patch("requests.post", side_effect=requests.Timeout):
        resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
        assert resp.status_code == 502
        assert "timed out" in resp.json()["detail"]


def test_draft_model_error(client, seeded):
    with patch("requests.post", side_effect=requests.RequestException("connection failed")):
        resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
        assert resp.status_code == 502
        assert "failed" in resp.json()["detail"]


def test_draft_subject_re_not_doubled(client, pool, seeded):
    # Change subject to already have RE:
    with pool.connection() as conn:
        conn.execute("UPDATE messages SET subject = 'RE: Interview slot' WHERE id = %s", (seeded,))
    
    mock_response = MagicMock()
    mock_response.json.return_value = {
        "choices": [{"message": {"content": "Works for me."}}]
    }
    
    with patch("requests.post", return_value=mock_response):
        resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
        subject = resp.json()["subject"]
        # Should not be "RE: RE: Interview slot"
        assert subject.count("RE:") == 1


def test_draft_caps_thread_messages(client, pool, store, pg_catalog, seeded):
    # Add more messages to the thread to test the character cap
    with pool.connection() as conn:
        thread_id = conn.execute("SELECT thread_id FROM messages WHERE id = %s", (seeded,)).fetchone()[0]
        for i in range(10):
            raw = email_bytes(100 + i, sender=f"Person{i} <p{i}@x>", subject="Interview slot",
                            body="x" * 5000)  # long body
            sync_mailbox(FakeMailbox({"INBOX": (1, {100 + i: raw})}), store, pg_catalog, ME, "INBOX",
                        initial_fetch_count=0)
            new_id = conn.execute("SELECT id FROM messages ORDER BY id DESC LIMIT 1").fetchone()[0]
            conn.execute("UPDATE messages SET thread_id = %s WHERE id = %s", (thread_id, new_id))
    
    mock_response = MagicMock()
    mock_response.json.return_value = {
        "choices": [{"message": {"content": "Reply."}}]
    }
    
    with patch("requests.post", return_value=mock_response):
        resp = client.post(f"/messages/{seeded}/draft", json={}, headers=AUTH)
        data = resp.json()
        # Should cap at MAX_THREAD_MESSAGES and MAX_THREAD_CHARS
        assert len(data["used_message_ids"]) <= 6  # MAX_THREAD_MESSAGES
