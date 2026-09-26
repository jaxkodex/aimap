"""`aimap backfill`: record messages that are in S3 but not in Postgres.

Covers mail stored before the database existed, and rebuilding the database from
the bucket. Lists raw/, skips keys already in message_locations, reads the rest,
and records them through the same catalog call ingestion uses, which also
enqueues their classify jobs. Running it twice changes nothing the second time.
Checkpoints are not touched.
"""

from __future__ import annotations

import itertools
import logging
from collections import defaultdict
from dataclasses import dataclass

from aimap.catalog import PgCatalog, StoredMessage
from aimap.parse import parse_internal_date, parse_meta
from aimap.storage import Store

log = logging.getLogger(__name__)


@dataclass
class BackfillResult:
    scanned: int = 0
    recorded: int = 0
    new_messages: int = 0
    skipped: int = 0  # keys that do not look like raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml


def backfill(store: Store, catalog: PgCatalog, chunk: int = 500) -> BackfillResult:
    res = BackfillResult()
    keys = store.list_raw()
    while batch := list(itertools.islice(keys, chunk)):
        res.scanned += len(batch)
        known = catalog.known_keys(batch)
        groups: dict[tuple[str, str, int], list[StoredMessage]] = defaultdict(list)
        for key in batch:
            if key in known:
                continue
            rk = store.parse_raw_key(key)
            if rk is None:
                res.skipped += 1
                continue
            got = store.get_raw(key)
            if got is None:
                continue
            raw, meta = got
            groups[(rk.account, rk.mailbox, rk.uidvalidity)].append(StoredMessage(
                uid=rk.uid, s3_key=key, meta=parse_meta(raw),
                flags=(meta.get("imap-flags") or "").split(),
                internal_date=parse_internal_date(meta.get("imap-internaldate", "")),
            ))
        for (account, mailbox, uidvalidity), items in groups.items():
            res.new_messages += catalog.record_batch(account, mailbox, uidvalidity, items, checkpoint=None)
            res.recorded += len(items)
        log.info("backfill progress", extra={"scanned": res.scanned, "recorded": res.recorded})
    return res
