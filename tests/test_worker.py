import logging

from conftest import FakeMailbox, msg

from aimap.accounts import Account
from aimap.config import S3Config, Settings
from aimap.worker import Worker


class ListSource:
    def __init__(self, *accounts):
        self.accounts = list(accounts)

    def load(self):
        return list(self.accounts)


SETTINGS = Settings(s3=S3Config(bucket="b"), poll_interval=10, max_backoff=60)


def test_failing_account_backs_off_without_blocking_others(store):
    good = Account(user="good", password="p", host="h")
    bad = Account(user="bad", password="p", host="h")
    now = [0.0]
    boxes = {"good": FakeMailbox({"INBOX": (1, {1: msg(1)})})}
    calls = []

    def factory(acct):
        calls.append(acct.user)
        if acct.user == "bad":
            raise OSError("connection refused")
        return boxes[acct.user]

    w = Worker(SETTINGS, store, ListSource(bad, good), source_factory=factory, clock=lambda: now[0])
    assert w.run_once() == 1 and w.failed_accounts == ["bad"]
    assert boxes["good"].closed

    for t, expected in ((5, ["good"]), (11, ["bad", "good"]), (25, ["good"]), (32, ["bad", "good"])):
        now[0] = t  # backoff 10s, then 20s after the second failure
        calls.clear()
        w.run_once()
        assert calls == expected, t


def test_accounts_added_and_removed_live(store):
    src = ListSource()
    box = FakeMailbox({"INBOX": (1, {1: msg(1)})})
    w = Worker(SETTINGS, store, src, source_factory=lambda a: box)
    assert w.run_once() == 0
    src.accounts.append(Account(user="new", password="p", host="h"))
    assert w.run_once() == 1
    src.accounts.clear()
    w.run_once()
    assert w._state == {}


def test_changed_account_resets_backoff(store):
    now = [0.0]
    src = ListSource(Account(user="u", password="wrong", host="h"))
    box = FakeMailbox({"INBOX": (1, {1: msg(1)})})

    def factory(acct):
        if acct.password == "wrong":
            raise OSError("auth failed")
        return box

    w = Worker(SETTINGS, store, src, source_factory=factory, clock=lambda: now[0])
    w.run_once()
    assert w.failed_accounts == ["u"]
    src.accounts = [Account(user="u", password="right", host="h")]  # fixed via set-password
    now[0] = 1  # still inside the backoff window, but the account changed
    assert w.run_once() == 1


def test_empty_warning_logged_once(store, caplog):
    w = Worker(SETTINGS, store, ListSource(), source_factory=lambda a: None)
    with caplog.at_level(logging.WARNING):
        w.run_once()
        w.run_once()
    assert sum("no accounts" in r.message for r in caplog.records) == 1


def test_run_forever_exits_when_stopped(store):
    w = Worker(SETTINGS, store, ListSource(), source_factory=lambda a: None)
    w.stop.set()
    w.run_forever()
