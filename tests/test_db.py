import psycopg
import pytest

from aimap import db


def test_migrations_are_ordered_and_numbered():
    ms = db.migrations()
    assert ms and [m.version for m in ms] == sorted(m.version for m in ms)
    assert ms[0].name == "0001_init.sql"


def test_migrate_is_idempotent(pg_dsn):
    assert db.migrate(pg_dsn) == []  # the session fixture already applied everything
    with psycopg.connect(pg_dsn) as conn:
        assert db.pending(conn) == []
        assert db.applied_versions(conn) == {m.version for m in db.migrations()}


def test_message_state_keeps_one_row_per_message_and_follows_it(pool):
    with pool.connection() as conn:
        conn.execute("INSERT INTO accounts (address, profile_id) SELECT 'me@x.com', id FROM profiles LIMIT 1")
        mid = conn.execute("""
            INSERT INTO messages (account_id, rfc822_message_id, thread_id)
            SELECT id, '<m1@x>', 'thread:test' FROM accounts
            RETURNING id""").fetchone()[0]
        conn.execute("INSERT INTO message_state (message_id, state, changed_by) VALUES (%s, 'later', 'me@x.com')",
                     (mid,))
    with pool.connection() as conn, pytest.raises(psycopg.errors.UniqueViolation):  # one state per message
        conn.execute("INSERT INTO message_state (message_id, state, changed_by) VALUES (%s, 'handled', 'x')",
                     (mid,))
    with pool.connection() as conn, pytest.raises(psycopg.errors.CheckViolation):  # only handled and later
        conn.execute("UPDATE message_state SET state = 'archived' WHERE message_id = %s", (mid,))
    with pool.connection() as conn:
        assert conn.execute("SELECT state FROM message_state").fetchall() == [("later",)]
        conn.execute("DELETE FROM messages WHERE id = %s", (mid,))  # the state goes with the message
        assert conn.execute("SELECT count(*) FROM message_state").fetchone()[0] == 0


def test_require_current_fails_on_a_behind_schema(pool, monkeypatch):
    db.require_current(pool)
    real = db.migrations()
    monkeypatch.setattr(db, "migrations", lambda: [*real, db.Migration(9999, "9999_future.sql", "SELECT 1")])
    with pytest.raises(db.SchemaError, match="9999_future"):
        db.require_current(pool)
