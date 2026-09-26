"""S3 layout and checkpoint storage.

Keys (all under S3_PREFIX):

    raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml    original RFC 822 bytes
    state/<account>/<mailbox>.json                    legacy checkpoints, read once to import into Postgres

IMAP UIDs are only unique within one UIDVALIDITY, so it is part of the key.
Uploads use the same key for the same message, so re-running a batch after a
crash overwrites instead of duplicating.
"""

from __future__ import annotations

import json
import urllib.parse
from collections.abc import Iterator
from dataclasses import dataclass
from typing import Any

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError

from aimap.config import S3Config


def _seg(value: str) -> str:
    """Make an account or mailbox name safe as one key segment ("[Gmail]/All Mail" has a slash)."""
    return urllib.parse.quote(value, safe="@.-_+")


class ConflictError(RuntimeError):
    """A conditional write lost: someone else changed the object since it was read."""


@dataclass
class Checkpoint:
    uidvalidity: int
    last_uid: int


@dataclass(frozen=True)
class RawKey:
    account: str
    mailbox: str
    uidvalidity: int
    uid: int


class Store:
    def __init__(self, cfg: S3Config, client: Any | None = None) -> None:
        self.cfg = cfg
        self.bucket = cfg.bucket
        self.prefix = cfg.prefix
        self.s3 = client or boto3.client(
            "s3",
            endpoint_url=cfg.endpoint_url,
            region_name=cfg.region,
            aws_access_key_id=cfg.access_key_id,
            aws_secret_access_key=cfg.secret_access_key,
            config=Config(
                retries={"max_attempts": 5, "mode": "standard"},
                s3={"addressing_style": "path" if cfg.force_path_style else "auto"},
            ),
        )

    def key(self, path: str) -> str:
        return f"{self.prefix}{path.lstrip('/')}"

    def head_etag(self, key: str) -> str | None:
        try:
            return self.s3.head_object(Bucket=self.bucket, Key=key)["ETag"]
        except ClientError as e:
            if _missing(e):
                return None
            raise

    def get_bytes(self, key: str) -> tuple[bytes, str] | None:
        """(body, etag), or None if the object does not exist."""
        try:
            obj = self.s3.get_object(Bucket=self.bucket, Key=key)
        except ClientError as e:
            if _missing(e):
                return None
            raise
        return obj["Body"].read(), obj["ETag"]

    def put_bytes(self, key: str, body: bytes, content_type: str, *, expect_etag: str | None) -> str:
        """Write only if the object still has expect_etag (None: only if it does not exist yet)."""
        cond = {"IfMatch": expect_etag} if expect_etag else {"IfNoneMatch": "*"}
        try:
            resp = self.s3.put_object(Bucket=self.bucket, Key=key, Body=body, ContentType=content_type, **cond)
        except ClientError as e:
            if e.response.get("Error", {}).get("Code") in {"PreconditionFailed", "ConditionalRequestConflict", "412"}:
                raise ConflictError(key) from e
            raise
        return resp["ETag"]

    def raw_key(self, account: str, mailbox: str, uidvalidity: int, uid: int) -> str:
        return f"{self.prefix}raw/{_seg(account)}/{_seg(mailbox)}/{uidvalidity}/{uid}.eml"

    def state_key(self, account: str, mailbox: str) -> str:
        return f"{self.prefix}state/{_seg(account)}/{_seg(mailbox)}.json"

    def check(self) -> None:
        """Fail fast at startup if the bucket is unreachable or credentials are wrong."""
        self.s3.head_bucket(Bucket=self.bucket)

    def put_raw(self, account: str, mailbox: str, uidvalidity: int, uid: int, raw: bytes,
                metadata: dict[str, str] | None = None) -> str:
        key = self.raw_key(account, mailbox, uidvalidity, uid)
        self.s3.put_object(
            Bucket=self.bucket, Key=key, Body=raw, ContentType="message/rfc822",
            Metadata={k: _ascii(v) for k, v in (metadata or {}).items()},
        )
        return key

    def get_raw(self, key: str) -> tuple[bytes, dict[str, str]] | None:
        """(body, user metadata) of a stored message, or None if it is gone."""
        try:
            obj = self.s3.get_object(Bucket=self.bucket, Key=key)
        except ClientError as e:
            if _missing(e):
                return None
            raise
        return obj["Body"].read(), obj.get("Metadata", {})

    def list_raw(self) -> Iterator[str]:
        """Every key under raw/, in S3 listing order."""
        paginator = self.s3.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=self.bucket, Prefix=f"{self.prefix}raw/"):
            for obj in page.get("Contents", []):
                yield obj["Key"]

    def parse_raw_key(self, key: str) -> RawKey | None:
        rest = key.removeprefix(f"{self.prefix}raw/")
        parts = rest.split("/")
        if rest == key or len(parts) != 4 or not parts[3].endswith(".eml"):
            return None
        try:
            return RawKey(urllib.parse.unquote(parts[0]), urllib.parse.unquote(parts[1]),
                          int(parts[2]), int(parts[3].removesuffix(".eml")))
        except ValueError:
            return None

    def legacy_checkpoint(self, account: str, mailbox: str) -> Checkpoint | None:
        """Checkpoint written to S3 by versions before Postgres. Only read, to import it once."""
        got = self.get_bytes(self.state_key(account, mailbox))
        if got is None:
            return None
        data = json.loads(got[0])
        return Checkpoint(uidvalidity=int(data["uidvalidity"]), last_uid=int(data["last_uid"]))


def _missing(e: ClientError) -> bool:
    return e.response.get("Error", {}).get("Code") in {"NoSuchKey", "404", "NotFound"}


def _ascii(value: str) -> str:
    """S3 user metadata must be ASCII."""
    return value.encode("ascii", "backslashreplace").decode("ascii")[:1024]
