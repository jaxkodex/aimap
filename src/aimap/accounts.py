"""Where the worker gets its list of IMAP accounts.

Two sources, chosen with ACCOUNTS_SOURCE:

bucket  (default when IMAP_USER is not set)
    A JSON file in the bucket, `<S3_PREFIX>config/accounts.json` by default.
    Passwords are encrypted with AIMAP_SECRET_KEY; everything else is plain so
    the file can be inspected. The worker re-checks the file's ETag every poll,
    so `aimap accounts add/remove/...` takes effect without a restart.

env     (default when IMAP_USER is set)
    IMAP_USER / IMAP_PASSWORD, plus IMAP_USER2 / IMAP_PASSWORD2 and so on.
    Fixed at startup. Meant for a quick single-mailbox setup.

File format:

    {"version": 1, "accounts": [{
        "user": "me@example.com", "host": "imap.gmail.com", "port": 993,
        "mailboxes": ["INBOX"], "enabled": true,
        "credentials": "<Fernet token>",
        "created_at": "...", "updated_at": "..."}]}
"""

from __future__ import annotations

import json
import logging
import re
from collections.abc import Callable, Mapping
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from typing import Protocol

from aimap.crypto import Cipher, CryptoError
from aimap.storage import ConflictError, Store

log = logging.getLogger(__name__)

FILE_VERSION = 1


class AccountsError(ValueError):
    pass


@dataclass(frozen=True)
class Account:
    """A mailbox the worker can log in to. The password is decrypted and kept in memory only."""

    user: str
    password: str = field(repr=False)
    host: str
    port: int = 993
    mailboxes: tuple[str, ...] = ("INBOX",)


class AccountSource(Protocol):
    def load(self) -> list[Account]:
        """Current enabled accounts. Called at the start of every poll."""


# --------------------------------------------------------------------------------------
# env source
# --------------------------------------------------------------------------------------

_USER_KEY = re.compile(r"^IMAP_USER(\d*)$")


def parse_env_accounts(env: Mapping[str, str]) -> list[Account]:
    suffixes = sorted(
        (m.group(1) for k in env if (m := _USER_KEY.match(k)) and env[k].strip()),
        key=lambda s: int(s or 1),
    )
    accounts = []
    for sfx in suffixes:

        def get(name: str, default: str | None = None, fallback: bool = True, _sfx: str = sfx) -> str | None:
            value = env.get(name + _sfx, "").strip()
            if not value and fallback:
                value = env.get(name, "").strip()
            return value or default

        password = get("IMAP_PASSWORD", fallback=False)
        host = get("IMAP_HOST")
        port = get("IMAP_PORT", "993")
        if not password:
            raise AccountsError(f"IMAP_PASSWORD{sfx} is required when IMAP_USER{sfx} is set")
        if not host:
            raise AccountsError(f"IMAP_HOST{sfx} (or IMAP_HOST) is required")
        if not port.isdigit():
            raise AccountsError(f"IMAP_PORT{sfx} must be an integer, got {port!r}")
        accounts.append(Account(
            user=get("IMAP_USER", fallback=False),
            password=clean_password(password),
            host=host,
            port=int(port),
            mailboxes=parse_mailboxes(get("IMAP_MAILBOX", "INBOX")),
        ))
    return accounts


class EnvAccounts:
    def __init__(self, env: Mapping[str, str]) -> None:
        self.accounts = parse_env_accounts(env)
        if not self.accounts:
            raise AccountsError("ACCOUNTS_SOURCE=env but IMAP_USER / IMAP_PASSWORD are not set")

    def load(self) -> list[Account]:
        return list(self.accounts)


def clean_password(password: str) -> str:
    return password.replace(" ", "")  # app passwords are often pasted with spaces


def parse_mailboxes(value: str | list[str] | None) -> tuple[str, ...]:
    items = value.split(",") if isinstance(value, str) else (value or [])
    return tuple(m.strip() for m in items if m.strip()) or ("INBOX",)


# --------------------------------------------------------------------------------------
# bucket source
# --------------------------------------------------------------------------------------


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


@dataclass
class AccountRecord:
    """An account as stored in the file. `credentials` is ciphertext."""

    user: str
    host: str
    credentials: str
    port: int = 993
    mailboxes: list[str] = field(default_factory=lambda: ["INBOX"])
    enabled: bool = True
    created_at: str = field(default_factory=_now)
    updated_at: str = field(default_factory=_now)

    @classmethod
    def from_dict(cls, d: dict) -> AccountRecord:
        try:
            return cls(
                user=str(d["user"]), host=str(d["host"]), credentials=str(d["credentials"]),
                port=int(d.get("port", 993)), mailboxes=list(parse_mailboxes(d.get("mailboxes"))),
                enabled=bool(d.get("enabled", True)),
                created_at=str(d.get("created_at", "")), updated_at=str(d.get("updated_at", "")),
            )
        except (KeyError, TypeError, ValueError) as e:
            raise AccountsError(f"invalid account record {d.get('user', '?')!r}: {e}") from e

    def update(self, **changes) -> None:
        for k, v in changes.items():
            if not hasattr(self, k):
                raise AttributeError(k)
            setattr(self, k, v)
        self.updated_at = _now()

    def to_account(self, cipher: Cipher) -> Account:
        return Account(user=self.user, password=cipher.decrypt(self.user, self.credentials),
                       host=self.host, port=self.port, mailboxes=tuple(self.mailboxes))


@dataclass
class AccountsDoc:
    records: list[AccountRecord] = field(default_factory=list)
    etag: str | None = None  # None: file does not exist yet

    @classmethod
    def parse(cls, body: bytes, etag: str | None) -> AccountsDoc:
        try:
            data = json.loads(body)
        except ValueError as e:
            raise AccountsError(f"accounts file is not valid JSON: {e}") from e
        if not isinstance(data, dict) or not isinstance(data.get("accounts"), list):
            raise AccountsError('accounts file must be {"version": 1, "accounts": [...]}')
        if data.get("version", FILE_VERSION) != FILE_VERSION:
            raise AccountsError(f"unsupported accounts file version {data.get('version')!r}")
        records = [AccountRecord.from_dict(d) for d in data["accounts"]]
        seen = set()
        for r in records:
            if r.user in seen:
                raise AccountsError(f"duplicate account {r.user!r} in accounts file")
            seen.add(r.user)
        return cls(records, etag)

    def dump(self) -> bytes:
        return json.dumps({"version": FILE_VERSION, "accounts": [asdict(r) for r in self.records]},
                          indent=2, ensure_ascii=False).encode() + b"\n"

    def get(self, user: str) -> AccountRecord | None:
        return next((r for r in self.records if r.user == user), None)

    def require(self, user: str) -> AccountRecord:
        rec = self.get(user)
        if rec is None:
            raise AccountsError(f"no account {user!r}")
        return rec


class AccountsRepo:
    """Read and change the accounts file. Writes are conditional on the ETag, so two
    concurrent CLI runs cannot silently overwrite each other."""

    def __init__(self, store: Store, path: str = "config/accounts.json") -> None:
        self.store = store
        self.key = store.key(path)

    def etag(self) -> str | None:
        return self.store.head_etag(self.key)

    def read(self) -> AccountsDoc:
        got = self.store.get_bytes(self.key)
        return AccountsDoc() if got is None else AccountsDoc.parse(*got)

    def modify(self, change: Callable[[AccountsDoc], None], attempts: int = 3) -> AccountsDoc:
        """Apply change() to a fresh copy and write it back, retrying if someone else wrote first."""
        for i in range(attempts):
            doc = self.read()
            change(doc)
            try:
                doc.etag = self.store.put_bytes(self.key, doc.dump(), "application/json", expect_etag=doc.etag)
                return doc
            except ConflictError:
                if i == attempts - 1:
                    raise
                log.info("accounts file changed during update, retrying")
        raise AssertionError("unreachable")


class BucketAccounts:
    """Account source backed by the accounts file. Re-reads it only when its ETag changes.
    If the file becomes unreadable, keeps serving the last good list."""

    def __init__(self, repo: AccountsRepo, cipher: Cipher) -> None:
        self.repo = repo
        self.cipher = cipher
        self._etag: str | None = "<never loaded>"
        self._accounts: list[Account] = []

    def load(self) -> list[Account]:
        try:
            etag = self.repo.etag()
            if etag == self._etag:
                return list(self._accounts)
            doc = self.repo.read()
        except Exception as e:
            log.error("cannot read accounts file, using last good copy",
                      extra={"key": self.repo.key, "error": str(e), "accounts": len(self._accounts)})
            return list(self._accounts)

        accounts = []
        for rec in doc.records:
            if not rec.enabled:
                continue
            try:
                accounts.append(rec.to_account(self.cipher))
            except CryptoError as e:
                log.error("skipping account", extra={"account": rec.user, "error": str(e)})
        self._etag, self._accounts = doc.etag, accounts
        log.info("accounts loaded", extra={
            "key": self.repo.key, "enabled": [a.user for a in accounts],
            "disabled": [r.user for r in doc.records if not r.enabled],
        })
        return list(accounts)

