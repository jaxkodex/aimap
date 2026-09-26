"""The jobs table as a work queue.

A worker claims one pending job with `FOR UPDATE SKIP LOCKED`, so two workers
never get the same row, and marks it `running`. When the work is done it marks it
`done`. On error it goes back to `pending` with an exponential delay, or to
`failed` after `max_attempts`. A job left `running` by a worker that died is
put back by `reclaim_stuck`.
"""

from __future__ import annotations

from dataclasses import dataclass

from psycopg import Connection


@dataclass(frozen=True)
class Job:
    id: int
    message_id: int
    stage: str
    attempts: int  # including the current one


def claim(conn: Connection, stage: str) -> Job | None:
    row = conn.execute("""
        UPDATE jobs SET status = 'running', locked_at = now(), attempts = attempts + 1, updated_at = now()
        WHERE id = (
            SELECT id FROM jobs
            WHERE stage = %s AND status = 'pending' AND run_after <= now()
            ORDER BY run_after, id
            FOR UPDATE SKIP LOCKED
            LIMIT 1)
        RETURNING id, message_id, stage, attempts""", (stage,)).fetchone()
    return Job(*row) if row else None


def complete(conn: Connection, job_id: int) -> None:
    conn.execute("""
        UPDATE jobs SET status = 'done', locked_at = NULL, last_error = NULL, updated_at = now()
        WHERE id = %s""", (job_id,))


def fail(conn: Connection, job: Job, error: str, *, max_attempts: int, base_delay: float,
         max_delay: float = 3600.0, permanent: bool = False) -> str:
    """Record a failure. Returns the new status: 'pending' (will retry) or 'failed'."""
    if permanent or job.attempts >= max_attempts:
        status, delay = "failed", 0.0
    else:
        status, delay = "pending", min(max_delay, base_delay * 2 ** (job.attempts - 1))
    conn.execute("""
        UPDATE jobs SET status = %s, locked_at = NULL, last_error = %s, updated_at = now(),
                        run_after = now() + make_interval(secs => %s)
        WHERE id = %s""", (status, error[:2000], delay, job.id))
    return status


def reclaim_stuck(conn: Connection, stage: str, timeout: float, max_attempts: int) -> int:
    """Put back jobs that have been `running` longer than timeout seconds. Returns how many."""
    cur = conn.execute("""
        UPDATE jobs SET status = CASE WHEN attempts >= %s THEN 'failed' ELSE 'pending' END,
                        locked_at = NULL, updated_at = now(),
                        last_error = coalesce(last_error, 'worker stopped while running the job')
        WHERE stage = %s AND status = 'running' AND locked_at < now() - make_interval(secs => %s)""",
                       (max_attempts, stage, timeout))
    return cur.rowcount


def requeue(conn: Connection, stage: str, statuses: tuple[str, ...]) -> int:
    """Send jobs in the given statuses back to pending with a fresh attempt count."""
    cur = conn.execute("""
        UPDATE jobs SET status = 'pending', attempts = 0, run_after = now(), locked_at = NULL,
                        last_error = NULL, updated_at = now()
        WHERE stage = %s AND status = ANY(%s)""", (stage, list(statuses)))
    return cur.rowcount


def counts(conn: Connection) -> list[tuple[str, str, int]]:
    return conn.execute("SELECT stage, status, count(*) FROM jobs GROUP BY 1, 2 ORDER BY 1, 2").fetchall()
