"""Copy new messages from one mailbox to S3 and advance its checkpoint."""

from __future__ import annotations

import logging
import threading
from dataclasses import dataclass

from aimap.imap import MailSource
from aimap.storage import Checkpoint, Store

log = logging.getLogger(__name__)


@dataclass
class SyncResult:
    account: str
    mailbox: str
    uidvalidity: int
    stored: int
    last_uid: int
    interrupted: bool = False


def sync_mailbox(
    source: MailSource,
    store: Store,
    account: str,
    mailbox: str,
    *,
    initial_fetch_count: int = 100,
    batch_size: int = 25,
    stop: threading.Event | None = None,
) -> SyncResult:
    ctx = {"account": account, "mailbox": mailbox}
    uidvalidity = source.select(mailbox)
    cp = store.get_checkpoint(account, mailbox)

    if cp is not None and cp.uidvalidity != uidvalidity:
        # The server renumbered the mailbox. Old UIDs mean nothing now; start over
        # under the new UIDVALIDITY. Old objects stay where they are.
        log.warning("uidvalidity changed, resyncing",
                    extra=ctx | {"old_uidvalidity": cp.uidvalidity, "uidvalidity": uidvalidity})
        cp = None

    if cp is None:
        uids = source.uids_after(0)
        if initial_fetch_count:
            uids = uids[-initial_fetch_count:]
        cp = Checkpoint(uidvalidity=uidvalidity, last_uid=0)
        log.info("first sync", extra=ctx | {"uidvalidity": uidvalidity, "to_fetch": len(uids)})
        if not uids:
            store.put_checkpoint(account, mailbox, cp)
    else:
        uids = source.uids_after(cp.last_uid)

    stored = 0
    for start in range(0, len(uids), batch_size):
        if stop is not None and stop.is_set():
            return SyncResult(account, mailbox, uidvalidity, stored, cp.last_uid, interrupted=True)
        for uid in uids[start:start + batch_size]:
            msg = source.fetch(uid)
            if msg is None:
                log.info("message vanished before fetch", extra=ctx | {"uid": uid})
                continue
            store.put_raw(account, mailbox, uidvalidity, uid, msg.raw, metadata={
                "imap-flags": " ".join(msg.flags),
                "imap-internaldate": msg.internal_date,
            })
            stored += 1
        # Advance only after the whole batch is in S3. A crash mid-batch re-uploads
        # the same keys next time, which is harmless.
        cp = Checkpoint(uidvalidity=uidvalidity, last_uid=uids[min(start + batch_size, len(uids)) - 1])
        store.put_checkpoint(account, mailbox, cp)

    if stored:
        log.info("stored messages", extra=ctx | {"stored": stored, "last_uid": cp.last_uid})
    return SyncResult(account, mailbox, uidvalidity, stored, cp.last_uid)
