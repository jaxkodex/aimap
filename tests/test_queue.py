import threading

from conftest import email_bytes

from aimap import queue
from aimap.catalog import StoredMessage
from aimap.parse import parse_meta


def seed(pg_catalog, n):
    items = [StoredMessage(uid=u, s3_key=f"k{u}", meta=parse_meta(email_bytes(u))) for u in range(1, n + 1)]
    pg_catalog.record_batch("me", "INBOX", 1, items, None)


def status(pool):
    with pool.connection() as conn:
        return conn.execute("SELECT status, attempts, last_error FROM jobs ORDER BY id").fetchall()


def test_two_claimers_never_get_the_same_job(pool, pg_catalog):
    seed(pg_catalog, 20)
    got, lock, barrier = [], threading.Lock(), threading.Barrier(4)

    def claimer():
        barrier.wait()
        while True:
            with pool.connection() as conn:
                job = queue.claim(conn, "classify")
            if job is None:
                return
            with lock:
                got.append(job.id)

    threads = [threading.Thread(target=claimer) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert sorted(got) == list(range(1, 21))


def test_claim_holds_row_lock_until_commit(pool, pg_catalog):
    seed(pg_catalog, 1)
    with pool.connection() as a, pool.connection() as b:
        assert queue.claim(a, "classify") is not None  # a's transaction still open
        assert queue.claim(b, "classify") is None      # skipped, not blocked


def test_retry_backoff_then_failed(pool, pg_catalog):
    seed(pg_catalog, 1)
    for attempt in (1, 2):
        with pool.connection() as conn:
            conn.execute("UPDATE jobs SET run_after = now()")
            job = queue.claim(conn, "classify")
            assert job.attempts == attempt
            assert queue.fail(conn, job, "boom", max_attempts=2, base_delay=60) == (
                "pending" if attempt == 1 else "failed")
        if attempt == 1:
            with pool.connection() as conn:
                assert queue.claim(conn, "classify") is None  # waiting out the backoff
    assert status(pool) == [("failed", 2, "boom")]
    with pool.connection() as conn:
        assert queue.requeue(conn, "classify", ("failed",)) == 1
    assert status(pool) == [("pending", 0, None)]


def test_permanent_failure_skips_retries(pool, pg_catalog):
    seed(pg_catalog, 1)
    with pool.connection() as conn:
        job = queue.claim(conn, "classify")
        assert queue.fail(conn, job, "gone", max_attempts=5, base_delay=1, permanent=True) == "failed"


def test_stuck_jobs_are_reclaimed(pool, pg_catalog):
    seed(pg_catalog, 2)
    with pool.connection() as conn:
        queue.claim(conn, "classify")
        queue.claim(conn, "classify")
        conn.execute("UPDATE jobs SET locked_at = now() - interval '1 hour' WHERE id = 1")
    with pool.connection() as conn:
        assert queue.reclaim_stuck(conn, "classify", timeout=600, max_attempts=5) == 1
    assert [s[0] for s in status(pool)] == ["pending", "running"]
    with pool.connection() as conn:
        assert dict((s, n) for _, s, n in queue.counts(conn)) == {"pending": 1, "running": 1}
