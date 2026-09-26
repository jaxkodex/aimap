import threading

from conftest import FakeMailbox, email_bytes

from aimap import jev, profiles
from aimap.classifier import Classifier, JevResult
from aimap.config import ClassifierSettings
from aimap.ingest import sync_mailbox

PROFILE = {"who": "Alex, a freelance designer.", "active_priorities": ["Client invoices"]}
PATTERNS = [jev.Pattern("Shop promotions", "Low", "discard", ["promo"],
                        [{"from": "Shop <deals@shop.example>", "subject": "Sale"}])]


class FakeJev:
    def __init__(self, fail_times=0):
        self.calls = []
        self.fail_times = fail_times
        self.lock = threading.Lock()

    def ask(self, state, questions):
        with self.lock:
            self.calls.append((state, questions))
            if self.fail_times:
                self.fail_times -= 1
                raise ConnectionError("jev unreachable")
        a = {
            "importance": {"score": 0.2, "confidence": 0.9},
            "action": {"choice": "discard", "confidence": 0.9},
            "sig:promotional": {"noul": 0.95},
        }
        if "pattern" in questions:
            a["pattern"] = {"choice": "Shop promotions", "confidence": 0.8,
                            "probabilities": {"Shop promotions": 0.8, "none_of_these": 0.2}}
            a["tag:promo"] = {"noul": 0.9}
        return JevResult(model="jev-test", answers=a, usage={"input_tokens": 10, "output_tokens": 2})


def setup(store, pg_catalog, pool, n=3, with_patterns=True):
    box = FakeMailbox({"INBOX": (1, {u: email_bytes(u, body=f"Sale number {u}") for u in range(1, n + 1)})})
    sync_mailbox(box, store, pg_catalog, "me@example.com", "INBOX", initial_fetch_count=0)
    with pool.connection() as conn:
        profiles.put(conn, "default", PROFILE)
        if with_patterns:
            profiles.replace_patterns(conn, profiles.get(conn, "default").id, PATTERNS)


def make(pool, store, fake, **kw):
    settings = ClassifierSettings(**{"model": "jev-test", "concurrency": 2, "retry_delay": 0, **kw})
    return Classifier(settings, pool, store, lambda: fake, idle_wait=0.01)


def q(pool, sql):
    with pool.connection() as conn:
        return conn.execute(sql).fetchall()


def test_classifies_every_queued_message(store, pg_catalog, pool):
    setup(store, pg_catalog, pool)
    fake = FakeJev()
    assert make(pool, store, fake).drain() == 3
    assert len(fake.calls) == 3
    state, questions = fake.calls[0]
    assert state["recipient_profile"] == PROFILE and state["email"]["body"].startswith("Sale number")
    assert "pattern" in questions
    rows = q(pool, "SELECT importance, action_bucket, tags, source, needs_review, model FROM classifications")
    assert rows == [("Low", "discard", ["promo"], "pattern", False, "jev-test")] * 3
    assert q(pool, "SELECT DISTINCT status FROM jobs") == [("done",)]
    assert q(pool, "SELECT count(*) FROM message_labels") == [(3,)]


def test_requeued_job_with_same_inputs_is_skipped(store, pg_catalog, pool):
    setup(store, pg_catalog, pool, n=1)
    fake = FakeJev()
    make(pool, store, fake).drain()
    q_ = "UPDATE jobs SET status = 'pending'"
    with pool.connection() as conn:
        conn.execute(q_)
    make(pool, store, fake).drain()
    assert len(fake.calls) == 1
    # a changed profile gives a new key, so the message is classified again
    with pool.connection() as conn:
        profiles.put(conn, "default", {**PROFILE, "who": "Alex, now a studio owner."})
        conn.execute(q_)
    make(pool, store, fake).drain()
    assert len(fake.calls) == 2
    assert q(pool, "SELECT count(*) FROM classifications") == [(2,)]


def test_profile_per_account(store, pg_catalog, pool):
    setup(store, pg_catalog, pool, n=1)
    with pool.connection() as conn:
        profiles.put(conn, "work", {"who": "Alex at work."})
        profiles.assign(conn, "me@example.com", "work")
    fake = FakeJev()
    make(pool, store, fake).drain()
    state, questions = fake.calls[0]
    assert state["recipient_profile"] == {"who": "Alex at work."} and "pattern" not in questions
    assert q(pool, "SELECT source FROM classifications") == [("judgment",)]


def test_errors_are_retried_then_succeed(store, pg_catalog, pool):
    setup(store, pg_catalog, pool, n=1)
    fake = FakeJev(fail_times=1)
    c = make(pool, store, fake, retry_delay=300)
    c.drain()
    assert q(pool, "SELECT status, attempts, last_error, run_after > now() FROM jobs") == [
        ("pending", 1, "ConnectionError: jev unreachable", True)]
    with pool.connection() as conn:
        conn.execute("UPDATE jobs SET run_after = now()")
    c.drain()
    assert q(pool, "SELECT status, attempts FROM jobs") == [("done", 2)]


def test_missing_raw_object_fails_permanently(store, pg_catalog, pool, s3):
    setup(store, pg_catalog, pool, n=1)
    s3.delete_object(Bucket=store.bucket, Key="t/raw/me@example.com/INBOX/1/1.eml")
    fake = FakeJev()
    make(pool, store, fake).drain()
    [(status, error)] = q(pool, "SELECT status, last_error FROM jobs")
    assert status == "failed" and "missing from S3" in error and not fake.calls


def test_run_forever_stops_on_signal(store, pg_catalog, pool):
    setup(store, pg_catalog, pool, n=2)
    c = make(pool, store, FakeJev())
    t = threading.Thread(target=c.run_forever)
    t.start()
    for _ in range(200):
        if q(pool, "SELECT count(*) FROM classifications") == [(2,)]:
            break
        threading.Event().wait(0.02)
    c.stop.set()
    t.join(timeout=5)
    assert not t.is_alive()
    assert q(pool, "SELECT count(*) FROM classifications") == [(2,)]
