from conftest import FakeMailbox, msg

from aimap.config import Account, S3Config, Settings
from aimap.worker import Worker


def settings(*accounts):
    return Settings(accounts=accounts, s3=S3Config(bucket="b"), poll_interval=10, max_backoff=60)


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

    w = Worker(settings(bad, good), store, source_factory=factory, clock=lambda: now[0])
    assert w.run_once() == 1 and w.failed_accounts == ["bad"]
    assert boxes["good"].closed

    calls.clear()
    now[0] = 5  # inside the 10s backoff: bad is skipped
    w.run_once()
    assert calls == ["good"]

    now[0] = 11  # backoff over: retried, fails again, delay doubles to 20s
    w.run_once()
    now[0] = 25
    calls.clear()
    w.run_once()
    assert calls == ["good"]
    now[0] = 32
    calls.clear()
    w.run_once()
    assert calls == ["bad", "good"]


def test_run_forever_exits_when_stopped(store):
    acct = Account(user="u", password="p", host="h")
    w = Worker(settings(acct), store, source_factory=lambda a: FakeMailbox({"INBOX": (1, {})}))
    w.stop.set()
    w.run_forever()  # returns immediately
