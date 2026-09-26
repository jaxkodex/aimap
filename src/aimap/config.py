"""Configuration from environment variables.

Accounts use numbered suffixes so several mailboxes can share one worker:

    IMAP_USER, IMAP_PASSWORD          account 1
    IMAP_USER2, IMAP_PASSWORD2        account 2
    ...

IMAP_HOST, IMAP_PORT and IMAP_MAILBOX can be set per account (IMAP_HOST2) and
fall back to the unsuffixed value.
"""

from __future__ import annotations

import os
import re
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path


class ConfigError(ValueError):
    pass


@dataclass(frozen=True)
class Account:
    user: str
    password: str = field(repr=False)
    host: str
    port: int = 993
    mailboxes: tuple[str, ...] = ("INBOX",)
    suffix: str = ""


@dataclass(frozen=True)
class S3Config:
    bucket: str
    prefix: str = ""
    endpoint_url: str | None = None
    region: str | None = None
    access_key_id: str | None = field(default=None, repr=False)
    secret_access_key: str | None = field(default=None, repr=False)
    force_path_style: bool = False


@dataclass(frozen=True)
class Settings:
    accounts: tuple[Account, ...]
    s3: S3Config
    poll_interval: float = 60.0
    initial_fetch_count: int = 100  # 0 = whole mailbox on first run
    batch_size: int = 25
    imap_timeout: float = 60.0
    max_backoff: float = 1800.0
    log_level: str = "INFO"
    log_format: str = "json"


_USER_KEY = re.compile(r"^IMAP_USER(\d*)$")


def _int(env: Mapping[str, str], name: str, default: int) -> int:
    raw = env.get(name, "").strip()
    if not raw:
        return default
    try:
        return int(raw)
    except ValueError as e:
        raise ConfigError(f"{name} must be an integer, got {raw!r}") from e


def _float(env: Mapping[str, str], name: str, default: float) -> float:
    raw = env.get(name, "").strip()
    if not raw:
        return default
    try:
        return float(raw)
    except ValueError as e:
        raise ConfigError(f"{name} must be a number, got {raw!r}") from e


def _bool(env: Mapping[str, str], name: str, default: bool) -> bool:
    raw = env.get(name, "").strip().lower()
    if not raw:
        return default
    return raw in {"1", "true", "yes", "on"}


def load_accounts(env: Mapping[str, str]) -> tuple[Account, ...]:
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

        user = get("IMAP_USER", fallback=False)
        password = get("IMAP_PASSWORD", fallback=False)
        host = get("IMAP_HOST")
        if not password:
            raise ConfigError(f"IMAP_PASSWORD{sfx} is required when IMAP_USER{sfx} is set")
        if not host:
            raise ConfigError(f"IMAP_HOST{sfx} (or IMAP_HOST) is required")
        mailboxes = tuple(m.strip() for m in (get("IMAP_MAILBOX", "INBOX") or "").split(",") if m.strip())
        accounts.append(Account(
            user=user,
            password=password.replace(" ", ""),  # app passwords are often pasted with spaces
            host=host,
            port=_int({f"IMAP_PORT{sfx}": get("IMAP_PORT", "993")}, f"IMAP_PORT{sfx}", 993),
            mailboxes=mailboxes or ("INBOX",),
            suffix=sfx,
        ))
    if not accounts:
        raise ConfigError("No accounts configured: set IMAP_USER and IMAP_PASSWORD")
    return tuple(accounts)


def load_settings(env: Mapping[str, str] | None = None) -> Settings:
    env = os.environ if env is None else env
    bucket = env.get("S3_BUCKET", "").strip()
    if not bucket:
        raise ConfigError("S3_BUCKET is required")
    prefix = env.get("S3_PREFIX", "").strip().strip("/")
    s3 = S3Config(
        bucket=bucket,
        prefix=f"{prefix}/" if prefix else "",
        endpoint_url=env.get("S3_ENDPOINT_URL", "").strip() or None,
        region=(env.get("S3_REGION") or env.get("AWS_REGION") or env.get("AWS_DEFAULT_REGION") or "").strip()
        or None,
        access_key_id=(env.get("S3_ACCESS_KEY_ID") or "").strip() or None,
        secret_access_key=(env.get("S3_SECRET_ACCESS_KEY") or "").strip() or None,
        force_path_style=_bool(env, "S3_FORCE_PATH_STYLE", False),
    )
    settings = Settings(
        accounts=load_accounts(env),
        s3=s3,
        poll_interval=_float(env, "POLL_INTERVAL_SECONDS", 60.0),
        initial_fetch_count=_int(env, "INITIAL_FETCH_COUNT", 100),
        batch_size=_int(env, "BATCH_SIZE", 25),
        imap_timeout=_float(env, "IMAP_TIMEOUT_SECONDS", 60.0),
        max_backoff=_float(env, "MAX_BACKOFF_SECONDS", 1800.0),
        log_level=env.get("LOG_LEVEL", "INFO").strip().upper() or "INFO",
        log_format=env.get("LOG_FORMAT", "json").strip().lower() or "json",
    )
    if settings.poll_interval <= 0:
        raise ConfigError("POLL_INTERVAL_SECONDS must be > 0")
    if settings.batch_size <= 0:
        raise ConfigError("BATCH_SIZE must be > 0")
    if settings.initial_fetch_count < 0:
        raise ConfigError("INITIAL_FETCH_COUNT must be >= 0")
    return settings


def load_dotenv(path: Path) -> None:
    """Minimal .env loader for local runs. Existing environment variables win."""
    if not path.is_file():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip().removeprefix("export ").strip()
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        os.environ.setdefault(key, value)
