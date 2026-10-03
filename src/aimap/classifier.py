"""The `aimap classify` process: take classify jobs, ask Jev, store the labels.

For each job:

1. Load the message metadata, its account's profile and the profile's patterns.
2. Build the questions and the classifier key. If this message already has a
   classification with that key, mark the job done without calling Jev.
3. Read the raw message from S3 and build the Jev state (header facts plus a
   trimmed body, in memory only).
4. Call Jev, run `decide`, then insert the classification and mark the job done
   in one transaction.

`concurrency` threads each claim their own jobs. The main thread puts back jobs
left `running` by a worker that died, and stops everything on SIGTERM.
"""

from __future__ import annotations

import logging
import signal
import threading
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, Protocol

from psycopg.types.json import Jsonb
from psycopg_pool import ConnectionPool

from aimap import jev, profiles, queue
from aimap.config import ClassifierSettings
from aimap.parse import MessageMeta
from aimap.storage import Store

log = logging.getLogger(__name__)

STAGE = "classify"


@dataclass(frozen=True)
class JevResult:
    model: str
    answers: dict[str, dict]
    usage: dict[str, Any] | None = None


class Asker(Protocol):
    def ask(self, state: dict, questions: dict) -> JevResult: ...


class TypeSafeAsker:
    """Jev through the TypeSafe SDK. One per worker thread."""

    def __init__(self, api_key: str, model: str, timeout: float = 120.0) -> None:
        from typesafe_sdk import TypeSafeClient

        self.client = TypeSafeClient(api_key=api_key, model=model, timeout=timeout)

    def ask(self, state: dict, questions: dict) -> JevResult:
        resp = self.client.system_one(state, questions)
        return JevResult(model=resp.model, usage=resp.usage.model_dump() if resp.usage else None,
                         answers={k: v.model_dump() for k, v in resp.answers.items()})


class PermanentError(RuntimeError):
    """Retrying will not help, for example the raw message is gone from S3."""


@dataclass
class Classifier:
    settings: ClassifierSettings
    pool: ConnectionPool
    store: Store
    asker_factory: Callable[[], Asker]
    stop: threading.Event = field(default_factory=threading.Event)
    idle_wait: float = 5.0
    reclaim_every: float = 60.0

    # -- one job ----------------------------------------------------------------------

    def _context(self, message_id: int) -> dict | None:
        with self.pool.connection() as conn:
            row = conn.execute("""
                SELECT m.id, a.address, m.rfc822_message_id, m.from_email, m.from_name, m.subject, m.sent_at,
                       m.in_reply_to, m.message_references, m.has_list_headers, p.id, p.profile,
                       (SELECT l.s3_key FROM message_locations l WHERE l.message_id = m.id
                        ORDER BY l.ingested_at DESC, l.id DESC LIMIT 1)
                FROM messages m
                JOIN accounts a ON a.id = m.account_id
                JOIN profiles p ON p.id = a.profile_id
                WHERE m.id = %s""", (message_id,)).fetchone()
            if row is None:
                return None
            pats = profiles.patterns(conn, row[10])
        return {
            "message_id": row[0], "account": row[1],
            "meta": MessageMeta(rfc822_message_id=row[2], from_email=row[3], from_name=row[4], subject=row[5],
                                sent_at=row[6], in_reply_to=row[7], references=row[8], has_list_headers=row[9]),
            "profile_id": row[10], "profile": row[11], "patterns": pats, "s3_key": row[12],
        }

    def _exists(self, message_id: int, key: str) -> bool:
        with self.pool.connection() as conn:
            return conn.execute("SELECT 1 FROM classifications WHERE message_id = %s AND classifier_key = %s",
                                (message_id, key)).fetchone() is not None

    def process(self, job: queue.Job, asker: Asker) -> str:
        """Returns 'classified' or 'skipped'. Raises on failure."""
        ctx = self._context(job.message_id)
        if ctx is None:
            raise PermanentError("message row is gone")
        questions = jev.build_questions(ctx["patterns"])
        key = jev.classifier_key(questions, self.settings.model, ctx["profile"])
        if self._exists(job.message_id, key):
            with self.pool.connection() as conn:
                queue.complete(conn, job.id)
            return "skipped"
        if not ctx["s3_key"]:
            raise PermanentError("message has no stored location")
        got = self.store.get_raw(ctx["s3_key"])
        if got is None:
            raise PermanentError(f"raw message missing from S3: {ctx['s3_key']}")

        state = jev.build_state(ctx["profile"], ctx["account"], got[0], ctx["meta"], self.settings.body_chars)
        result = asker.ask(state, questions)
        d = jev.decide(result.answers, ctx["patterns"], self.settings.thresholds)

        with self.pool.connection() as conn, conn.transaction():
            conn.execute("""
                INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers, usage,
                    importance, action_bucket, tags, insight, source, needs_review, priority, signals)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON CONFLICT (message_id, classifier_key) DO NOTHING""",
                (job.message_id, ctx["profile_id"], key, result.model, Jsonb(result.answers),
                 Jsonb(result.usage) if result.usage is not None else None,
                 d.importance, d.action_bucket, d.tags, d.insight, d.source, d.needs_review, d.priority,
                 Jsonb(d.signals)))
            queue.complete(conn, job.id)
        log.info("classified", extra={"message_id": job.message_id, "account": ctx["account"],
                                      "importance": d.importance, "action": d.action_bucket, "source": d.source,
                                      "needs_review": d.needs_review})
        return "classified"

    def _run_job(self, job: queue.Job, asker: Asker) -> None:
        try:
            self.process(job, asker)
        except Exception as e:
            permanent = isinstance(e, PermanentError)
            with self.pool.connection() as conn:
                status = queue.fail(conn, job, f"{type(e).__name__}: {e}", max_attempts=self.settings.max_attempts,
                                    base_delay=self.settings.retry_delay, permanent=permanent)
            log.error("classify failed", exc_info=not permanent and not _is_api_error(e), extra={
                "message_id": job.message_id, "attempt": job.attempts, "status": status, "error": str(e)})

    def _claim(self) -> queue.Job | None:
        with self.pool.connection() as conn:
            return queue.claim(conn, STAGE)

    # -- loops ------------------------------------------------------------------------

    def _loop(self, keep_going: bool, done: list[int], lock: threading.Lock) -> None:
        asker = self.asker_factory()
        while not self.stop.is_set():
            try:
                job = self._claim()
            except Exception as e:
                log.error("cannot claim job", extra={"error": str(e)})
                job = None
                if not keep_going:
                    return
            if job is None:
                if not keep_going:
                    return
                self.stop.wait(self.idle_wait)
                continue
            self._run_job(job, asker)
            with lock:
                done[0] += 1

    def reclaim(self) -> int:
        with self.pool.connection() as conn:
            n = queue.reclaim_stuck(conn, STAGE, self.settings.job_timeout, self.settings.max_attempts)
        if n:
            log.warning("reclaimed stuck jobs", extra={"count": n})
        return n

    def _run(self, keep_going: bool) -> int:
        self.reclaim()
        done, lock = [0], threading.Lock()
        threads = [threading.Thread(target=self._loop, args=(keep_going, done, lock), name=f"classify-{i}",
                                    daemon=True) for i in range(self.settings.concurrency)]
        for t in threads:
            t.start()
        if keep_going:
            while not self.stop.wait(self.reclaim_every):
                try:
                    self.reclaim()
                except Exception as e:
                    log.error("cannot reclaim stuck jobs", extra={"error": str(e)})
        for t in threads:  # each finishes its current job, then exits
            t.join()
        return done[0]

    def drain(self) -> int:
        """Process jobs until none are due, then return how many were processed."""
        return self._run(keep_going=False)

    def run_forever(self) -> None:
        log.info("classifier started", extra={"model": self.settings.model, "concurrency": self.settings.concurrency})
        self._run(keep_going=True)
        log.info("classifier stopped")

    def install_signal_handlers(self) -> None:
        def handle(signum, _frame):
            log.info("shutdown requested", extra={"signal": signal.Signals(signum).name})
            self.stop.set()

        signal.signal(signal.SIGTERM, handle)
        signal.signal(signal.SIGINT, handle)


def _is_api_error(e: Exception) -> bool:
    try:
        from typesafe_sdk import TypeSafeError
    except ImportError:  # pragma: no cover
        return False
    return isinstance(e, TypeSafeError | OSError)
