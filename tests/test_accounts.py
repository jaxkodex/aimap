import json

import pytest
from conftest import BUCKET

from aimap.accounts import AccountRecord, AccountsDoc, AccountsError, AccountsRepo, BucketAccounts
from aimap.crypto import Cipher, generate_key
from aimap.storage import ConflictError


@pytest.fixture
def cipher():
    return Cipher(generate_key())


@pytest.fixture
def repo(store):
    return AccountsRepo(store)


def record(cipher, user, **kw):
    return AccountRecord(user=user, host="imap.x.com", credentials=cipher.encrypt(user, f"pw-{user}"), **kw)


def test_missing_file_is_empty(repo, cipher):
    assert repo.read().records == [] and repo.read().etag is None
    assert BucketAccounts(repo, cipher).load() == []


def test_file_has_no_plaintext_password(repo, cipher, s3):
    repo.modify(lambda d: d.records.append(record(cipher, "a@x.com")))
    body = s3.get_object(Bucket=BUCKET, Key="t/config/accounts.json")["Body"].read().decode()
    assert "pw-a@x.com" not in body
    assert json.loads(body)["accounts"][0]["user"] == "a@x.com"


def test_source_is_live_and_skips_disabled(repo, cipher):
    src = BucketAccounts(repo, cipher)
    repo.modify(lambda d: d.records.append(record(cipher, "a")))
    assert [(a.user, a.password) for a in src.load()] == [("a", "pw-a")]
    repo.modify(lambda d: d.records.append(record(cipher, "b", enabled=False)))
    assert [a.user for a in src.load()] == ["a"]
    repo.modify(lambda d: d.require("b").update(enabled=True))
    assert [a.user for a in src.load()] == ["a", "b"]


def test_source_only_rereads_when_etag_changes(repo, cipher, monkeypatch):
    repo.modify(lambda d: d.records.append(record(cipher, "a")))
    src = BucketAccounts(repo, cipher)
    src.load()
    reads = []
    orig = repo.read
    monkeypatch.setattr(repo, "read", lambda: reads.append(1) or orig())
    src.load()
    src.load()
    assert reads == []


def test_undecryptable_account_skipped_others_kept(repo, cipher):
    other = Cipher(generate_key())
    repo.modify(lambda d: d.records.extend([record(cipher, "good"), record(other, "bad")]))
    assert [a.user for a in BucketAccounts(repo, cipher).load()] == ["good"]


def test_broken_file_keeps_last_good(repo, cipher, store):
    repo.modify(lambda d: d.records.append(record(cipher, "a")))
    src = BucketAccounts(repo, cipher)
    assert len(src.load()) == 1
    store.s3.put_object(Bucket=BUCKET, Key=repo.key, Body=b"{not json")
    assert [a.user for a in src.load()] == ["a"]


def test_conditional_write_detects_concurrent_change(repo, cipher, store):
    repo.modify(lambda d: d.records.append(record(cipher, "a")))
    stale = repo.read()
    repo.modify(lambda d: d.records.append(record(cipher, "b")))
    with pytest.raises(ConflictError):
        store.put_bytes(repo.key, stale.dump(), "application/json", expect_etag=stale.etag)


def test_modify_retries_after_conflict(repo, cipher):
    repo.modify(lambda d: d.records.append(record(cipher, "a")))
    raced = []

    def change(doc):
        if not raced:  # another writer sneaks in between our read and write
            raced.append(1)
            repo.modify(lambda d: d.records.append(record(cipher, "b")))
        doc.records.append(record(cipher, "c"))

    repo.modify(change)
    assert [r.user for r in repo.read().records] == ["a", "b", "c"]


@pytest.mark.parametrize("body, msg", [
    (b"[]", "must be"),
    (b'{"version": 2, "accounts": []}', "version"),
    (b'{"accounts": [{"user": "a"}]}', "invalid account"),
    (b'{"accounts": [{"user":"a","host":"h","credentials":"x"},{"user":"a","host":"h","credentials":"x"}]}',
     "duplicate"),
])
def test_invalid_files(body, msg):
    with pytest.raises(AccountsError, match=msg):
        AccountsDoc.parse(body, "etag")
