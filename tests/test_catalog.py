import pytest
from conftest import FakeMailbox, email_bytes

from aimap.catalog import StoredMessage
from aimap.ingest import sync_mailbox
from aimap.parse import parse_meta
from aimap.storage import Checkpoint


def rows(pool, sql, *args):
    with pool.connection() as conn:
        return conn.execute(sql, args).fetchall()


def test_ingest_records_messages_jobs_and_checkpoint(store, pg_catalog, pool):
    box = FakeMailbox({"INBOX": (7, {u: email_bytes(u) for u in range(1, 4)})})
    sync_mailbox(box, store, pg_catalog, "me@example.com", "INBOX", initial_fetch_count=0, batch_size=2)
    assert rows(pool, "SELECT address, (SELECT name FROM profiles WHERE id = profile_id) FROM accounts") == [
        ("me@example.com", "default")]
    assert rows(pool, "SELECT rfc822_message_id, from_email, subject FROM messages ORDER BY id") == [
        (f"<m{u}@example.com>", "deals@shop.example", f"m{u}") for u in (1, 2, 3)]
    assert rows(pool, "SELECT s3_key, flags FROM message_locations ORDER BY id")[0] == (
        "t/raw/me@example.com/INBOX/7/1.eml", ["\\Seen"])
    assert rows(pool, "SELECT stage, status FROM jobs") == [("classify", "pending")] * 3
    assert pg_catalog.get_checkpoint("me@example.com", "INBOX") == Checkpoint(7, 3)


def test_same_message_in_two_mailboxes_is_one_message_and_one_job(store, pg_catalog, pool):
    same = email_bytes(1, message_id="<shared@x>")
    box = FakeMailbox({"INBOX": (7, {1: same}), "[Gmail]/All Mail": (9, {40: same})})
    for mb in ("INBOX", "[Gmail]/All Mail"):
        sync_mailbox(box, store, pg_catalog, "me", mb, initial_fetch_count=0)
    assert rows(pool, "SELECT count(*) FROM messages") == [(1,)]
    assert rows(pool, "SELECT mailbox, uid FROM message_locations ORDER BY id") == [
        ("INBOX", 1), ("[Gmail]/All Mail", 40)]
    assert rows(pool, "SELECT count(*) FROM jobs") == [(1,)]


def test_same_message_id_in_two_accounts_is_two_messages(store, pg_catalog, pool):
    same = email_bytes(1, message_id="<shared@x>")
    for acct in ("a@x", "b@x"):
        sync_mailbox(FakeMailbox({"INBOX": (1, {1: same})}), store, pg_catalog, acct, "INBOX")
    assert rows(pool, "SELECT count(*) FROM messages") == [(2,)]


def test_record_batch_is_idempotent(pg_catalog, pool):
    sm = StoredMessage(uid=5, s3_key="raw/me/INBOX/1/5.eml", meta=parse_meta(email_bytes(5)))
    assert pg_catalog.record_batch("me", "INBOX", 1, [sm], Checkpoint(1, 5)) == 1
    assert pg_catalog.record_batch("me", "INBOX", 1, [sm], Checkpoint(1, 5)) == 0
    assert rows(pool, "SELECT count(*) FROM message_locations") == [(1,)]


def test_batch_and_checkpoint_commit_together(pg_catalog, pool, monkeypatch):
    good = StoredMessage(uid=1, s3_key="k1", meta=parse_meta(email_bytes(1)))
    bad = StoredMessage(uid=2, s3_key="k2", meta=parse_meta(email_bytes(2)))
    orig = pg_catalog._record_one

    def boom(conn, account_id, mailbox, uidvalidity, sm):
        if sm.uid == 2:
            raise RuntimeError("db went away")
        return orig(conn, account_id, mailbox, uidvalidity, sm)

    monkeypatch.setattr(pg_catalog, "_record_one", boom)
    with pytest.raises(RuntimeError):
        pg_catalog.record_batch("me", "INBOX", 1, [good, bad], Checkpoint(1, 2))
    assert rows(pool, "SELECT count(*) FROM messages") == [(0,)]
    assert pg_catalog.get_checkpoint("me", "INBOX") is None


def test_uids_above_int32_fit(pg_catalog):
    pg_catalog.put_checkpoint("me", "INBOX", Checkpoint(4_000_000_000, 4_294_967_295))
    assert pg_catalog.get_checkpoint("me", "INBOX") == Checkpoint(4_000_000_000, 4_294_967_295)
