"""Postgres connection pool and schema migrations.

Migrations are numbered SQL files in aimap/migrations (0001_init.sql, 0002_...).
`aimap migrate` applies the missing ones in order, each in its own transaction,
under an advisory lock so two deploys cannot run them at once. Applied versions
are recorded in schema_migrations. `run` and `classify` refuse to start if any
migration is missing.
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass
from importlib import resources

import psycopg
from psycopg_pool import ConnectionPool

log = logging.getLogger(__name__)

_LOCK_ID = 0x61696D6170  # "aimap"
_FILE = re.compile(r"^(\d{4})_[\w-]+\.sql$")


class SchemaError(RuntimeError):
    pass


@dataclass(frozen=True)
class Migration:
    version: int
    name: str
    sql: str


def migrations() -> list[Migration]:
    found = []
    for entry in resources.files("aimap.migrations").iterdir():
        m = _FILE.match(entry.name)
        if m:
            found.append(Migration(int(m.group(1)), entry.name, entry.read_text(encoding="utf-8")))
    found.sort(key=lambda mg: mg.version)
    versions = [mg.version for mg in found]
    if len(set(versions)) != len(versions):
        raise SchemaError(f"duplicate migration versions: {versions}")
    return found


def _ensure_table(conn: psycopg.Connection) -> None:
    conn.execute("""
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version     INT PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )""")


def applied_versions(conn: psycopg.Connection) -> set[int]:
    exists = conn.execute("SELECT to_regclass('schema_migrations') IS NOT NULL").fetchone()[0]
    if not exists:
        return set()
    return {r[0] for r in conn.execute("SELECT version FROM schema_migrations")}


def pending(conn: psycopg.Connection) -> list[Migration]:
    done = applied_versions(conn)
    return [m for m in migrations() if m.version not in done]


def migrate(dsn: str) -> list[str]:
    """Apply missing migrations. Returns the names of the ones applied."""
    applied = []
    with psycopg.connect(dsn, autocommit=True) as conn:
        conn.execute("SELECT pg_advisory_lock(%s)", (_LOCK_ID,))
        try:
            _ensure_table(conn)
            for m in pending(conn):  # re-read under the lock
                with conn.transaction():
                    conn.execute(m.sql)
                    conn.execute("INSERT INTO schema_migrations (version, name) VALUES (%s, %s)",
                                 (m.version, m.name))
                log.info("migration applied", extra={"migration": m.name})
                applied.append(m.name)
        finally:
            conn.execute("SELECT pg_advisory_unlock(%s)", (_LOCK_ID,))
    return applied


def require_current(pool: ConnectionPool) -> None:
    with pool.connection() as conn:
        missing = pending(conn)
    if missing:
        raise SchemaError("database schema is behind, run `aimap migrate` (missing: "
                          + ", ".join(m.name for m in missing) + ")")


def open_pool(dsn: str, max_size: int = 10) -> ConnectionPool:
    pool = ConnectionPool(dsn, min_size=1, max_size=max_size, open=True, name="aimap")
    pool.wait(timeout=30)
    return pool
