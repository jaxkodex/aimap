from conftest import BUCKET, email_bytes

from aimap.backfill import backfill


def put(s3, key, raw, **meta):
    s3.put_object(Bucket=BUCKET, Key=key, Body=raw, Metadata=meta)


def test_backfill_records_unknown_keys_and_is_idempotent(store, pg_catalog, pool, s3):
    put(s3, "t/raw/me@x.com/%5BGmail%5D%2FAll%20Mail/3/10.eml", email_bytes(10),
        **{"imap-flags": "\\Seen \\Flagged", "imap-internaldate": "01-Jan-2026 10:00:00 +0000"})
    put(s3, "t/raw/me@x.com/INBOX/3/11.eml", email_bytes(11))
    put(s3, "t/raw/me@x.com/INBOX/3/12.eml", email_bytes(10))  # same Message-ID as uid 10
    put(s3, "t/raw/not-a-message.txt", b"x")
    res = backfill(store, pg_catalog, chunk=2)
    assert (res.scanned, res.recorded, res.new_messages, res.skipped) == (4, 3, 2, 1)
    with pool.connection() as conn:
        assert conn.execute("SELECT mailbox, uid, flags FROM message_locations ORDER BY uid").fetchall() == [
            ("[Gmail]/All Mail", 10, ["\\Seen", "\\Flagged"]), ("INBOX", 11, []), ("INBOX", 12, [])]
        assert conn.execute("SELECT count(*) FROM jobs").fetchone() == (2,)
        assert conn.execute("SELECT count(*) FROM mailbox_state").fetchone() == (0,)
    again = backfill(store, pg_catalog)
    assert (again.scanned, again.recorded) == (4, 0)
