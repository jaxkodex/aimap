"""S3 layout and checkpoint storage.

Keys (all under S3_PREFIX):

    raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml    original RFC 822 bytes
    state/<account>/<mailbox>.json                    {"uidvalidity", "last_uid", "updated_at"}

IMAP UIDs are only unique within one UIDVALIDITY, so it is part of the key.
Uploads use the same key for the same message, so re-running a batch after a
crash overwrites instead of duplicating.
"""

from __future__ import annotations

import json
import urllib.parse
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError

from aimap.config import S3Config


def _seg(value: str) -> str:
    """Make an account or mailbox name safe as one key segment ("[Gmail]/All Mail" has a slash)."""
    return urllib.parse.quote(value, safe="@.-_+")


@dataclass
class Checkpoint:
    uidvalidity: int
    last_uid: int


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

    def get_checkpoint(self, account: str, mailbox: str) -> Checkpoint | None:
        try:
            obj = self.s3.get_object(Bucket=self.bucket, Key=self.state_key(account, mailbox))
        except ClientError as e:
            if e.response.get("Error", {}).get("Code") in {"NoSuchKey", "404"}:
                return None
            raise
        data = json.loads(obj["Body"].read())
        return Checkpoint(uidvalidity=int(data["uidvalidity"]), last_uid=int(data["last_uid"]))

    def put_checkpoint(self, account: str, mailbox: str, cp: Checkpoint) -> None:
        body = json.dumps({
            "uidvalidity": cp.uidvalidity,
            "last_uid": cp.last_uid,
            "updated_at": datetime.now(UTC).isoformat(),
        })
        self.s3.put_object(Bucket=self.bucket, Key=self.state_key(account, mailbox),
                           Body=body.encode(), ContentType="application/json")


def _ascii(value: str) -> str:
    """S3 user metadata must be ASCII."""
    return value.encode("ascii", "backslashreplace").decode("ascii")[:1024]
