"""Main loop: poll every account, back off per account on failure, exit cleanly on SIGTERM."""

from __future__ import annotations

import logging
import signal
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field

from aimap.config import Account, Settings
from aimap.imap import ImapSource, MailSource
from aimap.ingest import sync_mailbox
from aimap.storage import Store

log = logging.getLogger(__name__)

SourceFactory = Callable[[Account], MailSource]


@dataclass
class _Backoff:
    failures: int = 0
    next_try: float = 0.0


@dataclass
class Worker:
    settings: Settings
    store: Store
    source_factory: SourceFactory | None = None
    stop: threading.Event = field(default_factory=threading.Event)
    clock: Callable[[], float] = time.monotonic
    _backoff: dict[str, _Backoff] = field(default_factory=dict)
    failed_accounts: list[str] = field(default_factory=list)  # from the last run_once

    def __post_init__(self) -> None:
        if self.source_factory is None:
            timeout = self.settings.imap_timeout
            self.source_factory = lambda acct: ImapSource(acct, timeout=timeout)

    def run_once(self) -> int:
        """One pass over every account. Returns the number of messages stored."""
        total = 0
        self.failed_accounts = []
        for acct in self.settings.accounts:
            if self.stop.is_set():
                break
            state = self._backoff.setdefault(acct.user, _Backoff())
            if self.clock() < state.next_try:
                continue
            try:
                total += self._sync_account(acct)
                if state.failures:
                    log.info("account recovered", extra={"account": acct.user})
                state.failures, state.next_try = 0, 0.0
            except Exception as e:  # one broken account must not stop the others
                state.failures += 1
                self.failed_accounts.append(acct.user)
                delay = min(self.settings.max_backoff, self.settings.poll_interval * 2 ** (state.failures - 1))
                state.next_try = self.clock() + delay
                log.error("account sync failed", exc_info=not isinstance(e, OSError),
                          extra={"account": acct.user, "error": str(e), "failures": state.failures,
                                 "retry_in_s": round(delay)})
        return total

    def _sync_account(self, acct: Account) -> int:
        source = self.source_factory(acct)
        try:
            stored = 0
            for mailbox in acct.mailboxes:
                if self.stop.is_set():
                    break
                result = sync_mailbox(
                    source, self.store, acct.user, mailbox,
                    initial_fetch_count=self.settings.initial_fetch_count,
                    batch_size=self.settings.batch_size,
                    stop=self.stop,
                )
                stored += result.stored
            return stored
        finally:
            source.close()

    def run_forever(self) -> None:
        log.info("worker started", extra={
            "accounts": [a.user for a in self.settings.accounts],
            "poll_interval_s": self.settings.poll_interval,
            "bucket": self.store.bucket,
        })
        while not self.stop.is_set():
            started = self.clock()
            self.run_once()
            elapsed = self.clock() - started
            self.stop.wait(max(0.0, self.settings.poll_interval - elapsed))
        log.info("worker stopped")

    def install_signal_handlers(self) -> None:
        def handle(signum, _frame):
            log.info("shutdown requested", extra={"signal": signal.Signals(signum).name})
            self.stop.set()

        signal.signal(signal.SIGTERM, handle)
        signal.signal(signal.SIGINT, handle)
