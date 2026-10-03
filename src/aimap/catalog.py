"""What ingestion writes to Postgres: accounts, checkpoints, message metadata and jobs.

`record_batch` is the hand-off to later stages. In one transaction it upserts the
message rows for a batch that is already in S3, enqueues a `classify` job for each
new message, and moves the mailbox checkpoint. If the process dies before the
commit, the next pass re-uploads the same S3 keys and these upserts do nothing
the second time.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from typing import Protocol

from psycopg import Connection
from psycopg_pool import ConnectionPool

from aimap.parse import MessageMeta, derive_thread_id
from aimap.storage import Checkpoint

DEFAULT_PROFILE = "default"
STAGES = ("classify",)  # enqueued for every new message; "embed" goes here later


@dataclass(frozen=True)
class StoredMessage:
    uid: int
    s3_key: str
    meta: MessageMeta
    flags: list[str] = field(default_factory=list)
    internal_date: datetime | None = None


class Catalog(Protocol):
    def sync_accounts(self, addresses: list[str]) -> None: ...

    def get_checkpoint(self, account: str, mailbox: str) -> Checkpoint | None: ...

    def put_checkpoint(self, account: str, mailbox: str, cp: Checkpoint) -> None: ...

    def record_batch(self, account: str, mailbox: str, uidvalidity: int, messages: list[StoredMessage],
                     checkpoint: Checkpoint | None) -> int:
        """Store metadata and enqueue jobs; advance the checkpoint if given. Returns new messages."""


class PgCatalog:
    def __init__(self, pool: ConnectionPool) -> None:
        self.pool = pool
        self._ids: dict[str, int] = {}

    def _account_id(self, conn: Connection, address: str) -> int:
        if (cached := self._ids.get(address)) is not None:
            return cached
        row = conn.execute("""
            INSERT INTO accounts (address, profile_id)
            VALUES (%s, (SELECT id FROM profiles WHERE name = %s))
            ON CONFLICT (address) DO UPDATE SET address = EXCLUDED.address
            RETURNING id""", (address, DEFAULT_PROFILE)).fetchone()
        self._ids[address] = row[0]
        return row[0]

    def account_id(self, address: str) -> int:
        with self.pool.connection() as conn:
            return self._account_id(conn, address)

    def sync_accounts(self, addresses: list[str]) -> None:
        with self.pool.connection() as conn:
            for a in addresses:
                self._account_id(conn, a)

    def get_checkpoint(self, account: str, mailbox: str) -> Checkpoint | None:
        with self.pool.connection() as conn:
            row = conn.execute("""
                SELECT s.uidvalidity, s.last_uid FROM mailbox_state s
                JOIN accounts a ON a.id = s.account_id
                WHERE a.address = %s AND s.mailbox = %s""", (account, mailbox)).fetchone()
        return Checkpoint(uidvalidity=row[0], last_uid=row[1]) if row else None

    def put_checkpoint(self, account: str, mailbox: str, cp: Checkpoint) -> None:
        with self.pool.connection() as conn:
            self._put_checkpoint(conn, self._account_id(conn, account), mailbox, cp)

    @staticmethod
    def _put_checkpoint(conn: Connection, account_id: int, mailbox: str, cp: Checkpoint) -> None:
        conn.execute("""
            INSERT INTO mailbox_state (account_id, mailbox, uidvalidity, last_uid, updated_at)
            VALUES (%s, %s, %s, %s, now())
            ON CONFLICT (account_id, mailbox) DO UPDATE
            SET uidvalidity = EXCLUDED.uidvalidity, last_uid = EXCLUDED.last_uid, updated_at = now()""",
                     (account_id, mailbox, cp.uidvalidity, cp.last_uid))

    def record_batch(self, account: str, mailbox: str, uidvalidity: int, messages: list[StoredMessage],
                     checkpoint: Checkpoint | None) -> int:
        new = 0
        with self.pool.connection() as conn, conn.transaction():
            account_id = self._account_id(conn, account)
            for sm in messages:
                new += self._record_one(conn, account_id, mailbox, uidvalidity, sm)
            if checkpoint is not None:
                self._put_checkpoint(conn, account_id, mailbox, checkpoint)
        return new

    def _record_one(self, conn: Connection, account_id: int, mailbox: str, uidvalidity: int, sm: StoredMessage) -> int:
        m = sm.meta
        # Get the account address for thread_id derivation
        account_addr = conn.execute("SELECT address FROM accounts WHERE id = %s", (account_id,)).fetchone()[0]
        participants = [account_addr, m.from_email] if m.from_email else [account_addr]
        thread_id = derive_thread_id(m.references, m.in_reply_to, m.subject, participants, m.rfc822_message_id)

        # xmax = 0 means the row was inserted, not updated: a message we had not seen.
        message_id, inserted = conn.execute("""
            INSERT INTO messages (account_id, rfc822_message_id, from_email, from_name, subject, sent_at,
                                  in_reply_to, message_references, thread_id, has_list_headers)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
            ON CONFLICT (account_id, rfc822_message_id) DO UPDATE SET account_id = EXCLUDED.account_id
            RETURNING id, (xmax = 0)""",
            (account_id, m.rfc822_message_id, m.from_email, m.from_name, m.subject, m.sent_at,
             m.in_reply_to, m.references, thread_id, m.has_list_headers)).fetchone()
        conn.execute("""
            INSERT INTO message_locations (message_id, account_id, mailbox, uidvalidity, uid, s3_key,
                                           flags, internal_date)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
            ON CONFLICT DO NOTHING""",
            (message_id, account_id, mailbox, uidvalidity, sm.uid, sm.s3_key, sm.flags, sm.internal_date))
        for stage in STAGES:
            conn.execute("INSERT INTO jobs (message_id, stage) VALUES (%s, %s) ON CONFLICT DO NOTHING",
                         (message_id, stage))
        return int(inserted)

    def known_keys(self, keys: list[str]) -> set[str]:
        with self.pool.connection() as conn:
            rows = conn.execute("SELECT s3_key FROM message_locations WHERE s3_key = ANY(%s)", (keys,))
            return {r[0] for r in rows}
