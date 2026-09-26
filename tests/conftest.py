from __future__ import annotations

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
