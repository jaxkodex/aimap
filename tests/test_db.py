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


def test_require_current_fails_on_a_behind_schema(pool, monkeypatch):
    db.require_current(pool)
    real = db.migrations()
    monkeypatch.setattr(db, "migrations", lambda: [*real, db.Migration(9999, "9999_future.sql", "SELECT 1")])
    with pytest.raises(db.SchemaError, match="9999_future"):
        db.require_current(pool)
