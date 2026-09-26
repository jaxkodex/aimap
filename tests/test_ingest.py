import threading

from conftest import BUCKET, FakeMailbox, keys, msg

from aimap.ingest import sync_mailbox


def test_first_sync_takes_only_the_most_recent(store, s3):
    box = FakeMailbox({"INBOX": (7, {u: msg(u) for u in range(1, 11)})})
    r = sync_mailbox(box, store, "me@example.com", "INBOX", initial_fetch_count=3)
    assert r.stored == 3 and r.last_uid == 10
    assert keys(s3, "t/raw/") == sorted(f"t/raw/me@example.com/INBOX/7/{u}.eml" for u in (8, 9, 10))
    body = s3.get_object(Bucket=BUCKET, Key="t/raw/me@example.com/INBOX/7/10.eml")
    assert body["Body"].read() == msg(10)
    assert body["ContentType"] == "message/rfc822"
    assert body["Metadata"]["imap-flags"] == "\\Seen"


def test_incremental_sync_fetches_only_new(store):
    data = {u: msg(u) for u in range(1, 4)}
    box = FakeMailbox({"INBOX": (7, data)})
    sync_mailbox(box, store, "me", "INBOX", initial_fetch_count=0)
    data[4], data[5] = msg(4), msg(5)
    box.fetched.clear()
    r = sync_mailbox(box, store, "me", "INBOX")
    assert box.fetched == [4, 5] and r.last_uid == 5
    assert sync_mailbox(box, store, "me", "INBOX").stored == 0


def test_empty_mailbox_writes_checkpoint(store):
    box = FakeMailbox({"INBOX": (7, {})})
    sync_mailbox(box, store, "me", "INBOX")
    cp = store.get_checkpoint("me", "INBOX")
    assert cp.uidvalidity == 7 and cp.last_uid == 0


def test_uidvalidity_change_resyncs(store, s3):
    box = FakeMailbox({"INBOX": (7, {1: msg(1), 2: msg(2)})})
    sync_mailbox(box, store, "me", "INBOX", initial_fetch_count=0)
    box.mailboxes["INBOX"] = (8, {1: msg(1)})
    r = sync_mailbox(box, store, "me", "INBOX", initial_fetch_count=0)
    assert r.stored == 1 and r.uidvalidity == 8
    assert "t/raw/me/INBOX/8/1.eml" in keys(s3)
    assert store.get_checkpoint("me", "INBOX").uidvalidity == 8


def test_checkpoint_advances_per_batch_and_stop_is_honoured(store):
    box = FakeMailbox({"INBOX": (7, {u: msg(u) for u in range(1, 8)})})
    stop = threading.Event()
    orig = box.fetch

    def fetch_then_stop(uid):
        if uid == 2:
            stop.set()
        return orig(uid)

    box.fetch = fetch_then_stop
    r = sync_mailbox(box, store, "me", "INBOX", initial_fetch_count=0, batch_size=3, stop=stop)
    assert r.interrupted and r.last_uid == 3  # first batch finished, then stopped
    assert store.get_checkpoint("me", "INBOX").last_uid == 3


def test_vanished_message_is_skipped(store):
    box = FakeMailbox({"INBOX": (7, {1: msg(1), 2: msg(2)})})
    orig = box.fetch
    box.fetch = lambda uid: None if uid == 1 else orig(uid)
    r = sync_mailbox(box, store, "me", "INBOX", initial_fetch_count=0)
    assert r.stored == 1 and r.last_uid == 2


def test_mailbox_names_with_slashes_are_escaped(store):
    assert store.raw_key("me@x.com", "[Gmail]/All Mail", 1, 5) == "t/raw/me@x.com/%5BGmail%5D%2FAll%20Mail/1/5.eml"
