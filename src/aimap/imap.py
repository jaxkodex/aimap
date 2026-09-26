"""Read-only IMAP access. Messages are fetched with BODY.PEEK so nothing is marked as read."""

from __future__ import annotations

import contextlib
import imaplib
import re
from dataclasses import dataclass
from typing import Protocol

from aimap.accounts import Account


class ImapError(RuntimeError):
    pass


@dataclass
class FetchedMessage:
    uid: int
    raw: bytes
    flags: list[str]
    internal_date: str


class MailSource(Protocol):
    """What the ingester needs from a mailbox. Implemented by ImapSource and by test fakes."""

    def select(self, mailbox: str) -> int:
        """Open mailbox read-only and return its UIDVALIDITY."""

    def uids_after(self, last_uid: int) -> list[int]:
        """Ascending UIDs greater than last_uid. last_uid=0 means all."""

    def fetch(self, uid: int) -> FetchedMessage | None:
        """Full message, or None if it disappeared since the search."""

    def close(self) -> None: ...


_INTERNALDATE = re.compile(rb'INTERNALDATE "([^"]+)"')


class ImapSource:
    def __init__(self, account: Account, timeout: float = 60.0) -> None:
        self.account = account
        try:
            self.conn = imaplib.IMAP4_SSL(account.host, account.port, timeout=timeout)
            self.conn.login(account.user, account.password)
        except (imaplib.IMAP4.error, OSError) as e:
            raise ImapError(f"connect/login to {account.host}:{account.port} failed: {e}") from e

    def select(self, mailbox: str) -> int:
        status, _ = self.conn.select(_quote(mailbox), readonly=True)
        if status != "OK":
            raise ImapError(f"cannot open mailbox {mailbox!r}")
        _, data = self.conn.response("UIDVALIDITY")
        if not data or data[0] is None:
            raise ImapError(f"server did not report UIDVALIDITY for {mailbox!r}")
        return int(data[0])

    def uids_after(self, last_uid: int) -> list[int]:
        criteria = "ALL" if last_uid <= 0 else f"UID {last_uid + 1}:*"
        status, data = self.conn.uid("SEARCH", None, criteria)
        if status != "OK":
            raise ImapError(f"UID SEARCH {criteria} failed")
        # "N:*" always matches the highest UID, even when it is below N, so filter.
        return sorted(u for u in (int(x) for x in (data[0] or b"").split()) if u > last_uid)

    def fetch(self, uid: int) -> FetchedMessage | None:
        status, resp = self.conn.uid("FETCH", str(uid), "(FLAGS INTERNALDATE BODY.PEEK[])")
        if status != "OK":
            raise ImapError(f"UID FETCH {uid} failed")
        part = next((p for p in resp or [] if isinstance(p, tuple)), None)
        if part is None:
            return None
        meta, raw = part
        m = _INTERNALDATE.search(meta)
        return FetchedMessage(
            uid=uid,
            raw=raw,
            flags=[f.decode() for f in imaplib.ParseFlags(meta)],
            internal_date=m.group(1).decode() if m else "",
        )

    def close(self) -> None:
        with contextlib.suppress(imaplib.IMAP4.error, OSError):
            self.conn.logout()


def _quote(mailbox: str) -> str:
    return '"' + mailbox.replace("\\", "\\\\").replace('"', '\\"') + '"'
