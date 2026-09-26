"""Main loop: reload accounts, poll each one, back off per account on failure, exit cleanly on SIGTERM."""

from __future__ import annotations

import logging
import signal
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field

from aimap.accounts import Account, AccountSource
from aimap.catalog import Catalog
from aimap.config import Settings
from aimap.imap import ImapSource, MailSource
from aimap.ingest import sync_mailbox
from aimap.storage import Store

log = logging.getLogger(__name__)

SourceFactory = Callable[[Account], MailSource]


@dataclass
class _AccountState:
    account: Account
    failures: int = 0
    next_try: float = 0.0


@dataclass
class Worker:
    settings: Settings
    store: Store
    catalog: Catalog
    accounts: AccountSource
    source_factory: SourceFactory | None = None
    stop: threading.Event = field(default_factory=threading.Event)
    clock: Callable[[], float] = time.monotonic
    failed_accounts: list[str] = field(default_factory=list)  # from the last run_once
    _state: dict[str, _AccountState] = field(default_factory=dict)
    _warned_empty: bool = False
    _synced: set[str] = field(default_factory=set)

    def __post_init__(self) -> None:
        if self.source_factory is None:
            timeout = self.settings.imap_timeout
            self.source_factory = lambda acct: ImapSource(acct, timeout=timeout)

    def _refresh(self) -> list[Account]:
        current = {a.user: a for a in self.accounts.load()}
        if current.keys() - self._synced:
            try:
                self.catalog.sync_accounts(sorted(current))
                self._synced = set(current)
            except Exception as e:  # each account's sync will fail and back off on its own
                log.error("cannot register accounts in the database", extra={"error": str(e)})
        for user in self._state.keys() - current.keys():
            log.info("account removed or disabled", extra={"account": user})
            del self._state[user]
        for user, acct in current.items():
            st = self._state.get(user)
            if st is None:
                log.info("account added", extra={"account": user})
                self._state[user] = _AccountState(acct)
            elif st.account != acct:
                # New password, host or mailboxes: forget old failures and try right away.
                log.info("account changed", extra={"account": user})
                self._state[user] = _AccountState(acct)
        if not current and not self._warned_empty:
            log.warning("no accounts configured; waiting (add one with `aimap accounts add`)")
        self._warned_empty = not current
        return list(current.values())

    def run_once(self) -> int:
        """One pass over every account. Returns the number of messages stored."""
        total = 0
        self.failed_accounts = []
        for acct in self._refresh():
            if self.stop.is_set():
                break
            st = self._state[acct.user]
            if self.clock() < st.next_try:
                continue
            try:
                total += self._sync_account(acct)
                if st.failures:
                    log.info("account recovered", extra={"account": acct.user})
                st.failures, st.next_try = 0, 0.0
            except Exception as e:  # one broken account must not stop the others
                st.failures += 1
                self.failed_accounts.append(acct.user)
                delay = min(self.settings.max_backoff, self.settings.poll_interval * 2 ** (st.failures - 1))
                st.next_try = self.clock() + delay
                log.error("account sync failed", exc_info=not isinstance(e, OSError),
                          extra={"account": acct.user, "error": str(e), "failures": st.failures,
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
                    source, self.store, self.catalog, acct.user, mailbox,
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
            "accounts_source": self.settings.accounts_source,
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
