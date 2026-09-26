"""Service settings from environment variables. Accounts are configured separately (see accounts.py)."""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path

from aimap.jev import Thresholds


class ConfigError(ValueError):
    pass


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
class ClassifierSettings:
    api_key: str | None = field(default=None, repr=False)
    model: str = "jev-1.13.0"  # pinned so tuned thresholds stay valid
    thresholds: Thresholds = field(default_factory=Thresholds)
    body_chars: int = 1500
    concurrency: int = 4
    max_attempts: int = 5
    retry_delay: float = 30.0  # first retry; doubles each attempt, capped at one hour
    job_timeout: float = 600.0  # a job running longer than this is put back in the queue
    request_timeout: float = 120.0


@dataclass(frozen=True)
class Settings:
    s3: S3Config
    database_url: str | None = field(default=None, repr=False)
    classifier: ClassifierSettings = field(default_factory=ClassifierSettings)
    accounts_source: str = "bucket"  # "bucket" or "env"
    accounts_path: str = "config/accounts.json"
    secret_key: str | None = field(default=None, repr=False)
    poll_interval: float = 60.0
    initial_fetch_count: int = 100  # 0 = whole mailbox on first run
    batch_size: int = 25
    imap_timeout: float = 60.0
    max_backoff: float = 1800.0
    log_level: str = "INFO"
    log_format: str = "json"


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


def load_settings(env: Mapping[str, str] | None = None, *, need_accounts: bool = True) -> Settings:
    """need_accounts=False skips the AIMAP_SECRET_KEY check, for commands that never read the accounts file."""
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

    source = env.get("ACCOUNTS_SOURCE", "").strip().lower()
    if not source:
        source = "env" if env.get("IMAP_USER", "").strip() else "bucket"
    if source not in {"bucket", "env"}:
        raise ConfigError(f"ACCOUNTS_SOURCE must be 'bucket' or 'env', got {source!r}")
    secret_key = env.get("AIMAP_SECRET_KEY", "").strip() or None
    if source == "bucket" and not secret_key and need_accounts:
        raise ConfigError("AIMAP_SECRET_KEY is required for the bucket accounts file (generate: aimap keygen)")

    settings = Settings(
        s3=s3,
        accounts_source=source,
        accounts_path=env.get("ACCOUNTS_PATH", "").strip().strip("/") or "config/accounts.json",
        secret_key=secret_key,
        poll_interval=_float(env, "POLL_INTERVAL_SECONDS", 60.0),
        initial_fetch_count=_int(env, "INITIAL_FETCH_COUNT", 100),
        batch_size=_int(env, "BATCH_SIZE", 25),
        imap_timeout=_float(env, "IMAP_TIMEOUT_SECONDS", 60.0),
        max_backoff=_float(env, "MAX_BACKOFF_SECONDS", 1800.0),
        log_level=env.get("LOG_LEVEL", "INFO").strip().upper() or "INFO",
        log_format=env.get("LOG_FORMAT", "json").strip().lower() or "json",
        database_url=env.get("DATABASE_URL", "").strip() or None,
        classifier=ClassifierSettings(
            api_key=(env.get("JEV_API_KEY") or env.get("TYPESAFE_API_KEY") or "").strip() or None,
            model=env.get("JEV_MODEL", "").strip() or "jev-1.13.0",
            thresholds=Thresholds(
                pattern_confidence=_float(env, "JEV_PATTERN_CONFIDENCE", 0.5),
                tag_threshold=_float(env, "JEV_TAG_THRESHOLD", 0.8),
                review_confidence=_float(env, "JEV_REVIEW_CONFIDENCE", 0.4),
            ),
            body_chars=_int(env, "JEV_BODY_CHARS", 1500),
            concurrency=_int(env, "CLASSIFY_CONCURRENCY", 4),
            max_attempts=_int(env, "CLASSIFY_MAX_ATTEMPTS", 5),
            retry_delay=_float(env, "CLASSIFY_RETRY_SECONDS", 30.0),
            job_timeout=_float(env, "CLASSIFY_JOB_TIMEOUT_SECONDS", 600.0),
            request_timeout=_float(env, "JEV_TIMEOUT_SECONDS", 120.0),
        ),
    )
    if settings.poll_interval <= 0:
        raise ConfigError("POLL_INTERVAL_SECONDS must be > 0")
    if settings.batch_size <= 0:
        raise ConfigError("BATCH_SIZE must be > 0")
    if settings.initial_fetch_count < 0:
        raise ConfigError("INITIAL_FETCH_COUNT must be >= 0")
    if settings.classifier.concurrency <= 0:
        raise ConfigError("CLASSIFY_CONCURRENCY must be > 0")
    if settings.classifier.max_attempts <= 0:
        raise ConfigError("CLASSIFY_MAX_ATTEMPTS must be > 0")
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
