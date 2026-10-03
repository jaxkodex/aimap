from __future__ import annotations

import os

import boto3
import pytest
from moto import mock_aws

from aimap.config import S3Config
from aimap.imap import FetchedMessage
from aimap.storage import Store

BUCKET = "aimap-test"


class FakeMailbox:
    """In-memory MailSource. mailboxes: name -> (uidvalidity, {uid: raw})."""

    def __init__(self, mailboxes: dict[str, tuple[int, dict[int, bytes]]]) -> None:
        self.mailboxes = mailboxes
        self.current: str | None = None
        self.fetched: list[int] = []
        self.closed = False

    def select(self, mailbox: str) -> int:
        if mailbox not in self.mailboxes:
            raise RuntimeError(f"no mailbox {mailbox}")
        self.current = mailbox
        return self.mailboxes[mailbox][0]

    def uids_after(self, last_uid: int) -> list[int]:
        return sorted(u for u in self.mailboxes[self.current][1] if u > last_uid)

    def fetch(self, uid: int) -> FetchedMessage | None:
        raw = self.mailboxes[self.current][1].get(uid)
        if raw is None:
            return None
        self.fetched.append(uid)
        return FetchedMessage(uid=uid, raw=raw, flags=["\\Seen"], internal_date="01-Jan-2026 10:00:00 +0000")

    def close(self) -> None:
        self.closed = True


def msg(n: int) -> bytes:
    return f"From: a@example.com\r\nSubject: m{n}\r\n\r\nbody {n}\r\n".encode()


@pytest.fixture
def s3():
    with mock_aws():
        client = boto3.client("s3", region_name="us-east-1")
        client.create_bucket(Bucket=BUCKET)
        yield client


@pytest.fixture
def store(s3):
    return Store(S3Config(bucket=BUCKET, prefix="t/"), client=s3)


def keys(s3, prefix: str = "") -> list[str]:
    resp = s3.list_objects_v2(Bucket=BUCKET, Prefix=prefix)
    return sorted(o["Key"] for o in resp.get("Contents", []))


# --------------------------------------------------------------------------------------
# Catalog: an in-memory fake for ingest/worker logic, real Postgres for the rest
# --------------------------------------------------------------------------------------


class FakeCatalog:
    """In-memory Catalog. Keeps what PgCatalog would write, without transactions."""

    def __init__(self) -> None:
        self.checkpoints: dict[tuple[str, str], object] = {}
        self.messages: dict[tuple[str, str], list] = {}  # (account, rfc822 id) -> locations
        self.accounts: set[str] = set()
        self.batches = 0

    def sync_accounts(self, addresses):
        self.accounts.update(addresses)

    def get_checkpoint(self, account, mailbox):
        return self.checkpoints.get((account, mailbox))

    def put_checkpoint(self, account, mailbox, cp):
        self.checkpoints[(account, mailbox)] = cp

    def record_batch(self, account, mailbox, uidvalidity, messages, checkpoint):
        self.batches += 1
        new = 0
        for sm in messages:
            locs = self.messages.setdefault((account, sm.meta.rfc822_message_id), [])
            new += not locs
            if sm.s3_key not in locs:
                locs.append(sm.s3_key)
        if checkpoint is not None:
            self.checkpoints[(account, mailbox)] = checkpoint
        return new


@pytest.fixture
def catalog():
    return FakeCatalog()


_TABLES = ("classifications, jobs, patterns, message_state, message_locations, messages, mailbox_state, "
           "accounts, profiles")


@pytest.fixture(scope="session")
def pg_dsn():
    """A fresh database with every migration applied. Needs TEST_DATABASE_URL; in CI a missing one fails."""
    base = os.environ.get("TEST_DATABASE_URL")
    if not base:
        if os.environ.get("CI"):
            pytest.fail("TEST_DATABASE_URL must be set in CI")
        pytest.skip("TEST_DATABASE_URL not set")
    import psycopg
    from psycopg.conninfo import conninfo_to_dict, make_conninfo

    name = f"aimap_test_{os.getpid()}"
    with psycopg.connect(base, autocommit=True) as conn:
        conn.execute(f"DROP DATABASE IF EXISTS {name}")
        conn.execute(f"CREATE DATABASE {name}")
    dsn = make_conninfo(**{**conninfo_to_dict(base), "dbname": name})
    from aimap import db

    db.migrate(dsn)
    yield dsn
    with psycopg.connect(base, autocommit=True) as conn:
        conn.execute(f"DROP DATABASE IF EXISTS {name} WITH (FORCE)")


@pytest.fixture
def pool(pg_dsn):
    from aimap import db

    p = db.open_pool(pg_dsn, max_size=6)
    with p.connection() as conn:
        conn.execute(f"TRUNCATE {_TABLES} RESTART IDENTITY CASCADE")
        conn.execute("INSERT INTO profiles (name) VALUES ('default')")
    yield p
    p.close()


@pytest.fixture
def pg_catalog(pool):
    from aimap.catalog import PgCatalog

    return PgCatalog(pool)


def email_bytes(n: int, *, message_id: str | None = None, sender: str = "Shop <deals@shop.example>",
                subject: str | None = None, body: str | None = None, extra: str = "") -> bytes:
    mid = message_id if message_id is not None else f"<m{n}@example.com>"
    headers = f"From: {sender}\r\nTo: me@example.com\r\nSubject: {subject or f'm{n}'}\r\n"
    headers += "Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n"
    if mid:
        headers += f"Message-ID: {mid}\r\n"
    return (headers + extra + "\r\n" + (body or f"body {n}") + "\r\n").encode()
