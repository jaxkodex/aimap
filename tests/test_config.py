import pytest

from aimap.accounts import AccountsError, EnvAccounts, parse_env_accounts
from aimap.config import ConfigError, load_settings
from aimap.crypto import generate_key

KEY = generate_key()


def test_source_defaults_to_bucket_and_needs_key():
    with pytest.raises(ConfigError, match="AIMAP_SECRET_KEY"):
        load_settings({"S3_BUCKET": "b"})
    s = load_settings({"S3_BUCKET": "b", "AIMAP_SECRET_KEY": KEY})
    assert s.accounts_source == "bucket" and s.accounts_path == "config/accounts.json"
    assert KEY not in repr(s)


def test_source_defaults_to_env_when_imap_user_set():
    s = load_settings({"S3_BUCKET": "b", "IMAP_USER": "a"})
    assert s.accounts_source == "env" and s.secret_key is None


def test_prefix_normalised():
    s = load_settings({"S3_BUCKET": "b", "AIMAP_SECRET_KEY": KEY, "S3_PREFIX": "/mail/"})
    assert s.s3.prefix == "mail/"


@pytest.mark.parametrize("env, msg", [
    ({}, "S3_BUCKET"),
    ({"S3_BUCKET": "b", "ACCOUNTS_SOURCE": "db"}, "ACCOUNTS_SOURCE"),
    ({"S3_BUCKET": "b", "AIMAP_SECRET_KEY": KEY, "POLL_INTERVAL_SECONDS": "x"}, "POLL_INTERVAL"),
])
def test_invalid_settings(env, msg):
    with pytest.raises(ConfigError, match=msg):
        load_settings(env)


ENV = {"IMAP_HOST": "imap.example.com"}


def test_env_accounts_with_fallbacks():
    a, b = parse_env_accounts(ENV | {
        "IMAP_USER": "a@x.com", "IMAP_PASSWORD": "abcd efgh",
        "IMAP_USER2": "b@x.com", "IMAP_PASSWORD2": "p2", "IMAP_HOST2": "imap.other.com",
        "IMAP_MAILBOX2": "INBOX, Archive",
    })
    assert (a.user, a.password, a.host, a.mailboxes) == ("a@x.com", "abcdefgh", "imap.example.com", ("INBOX",))
    assert (b.host, b.mailboxes) == ("imap.other.com", ("INBOX", "Archive"))
    assert "abcdefgh" not in repr(a)


def test_env_accounts_sorted_numerically():
    env = ENV | {f"IMAP_USER{n}": f"u{n}" for n in (10, 2)} | {f"IMAP_PASSWORD{n}": "p" for n in (10, 2)}
    assert [a.user for a in parse_env_accounts(env)] == ["u2", "u10"]


@pytest.mark.parametrize("env, msg", [
    ({}, "not set"),
    (ENV | {"IMAP_USER": "a"}, "IMAP_PASSWORD"),
    ({"IMAP_USER": "a", "IMAP_PASSWORD": "p"}, "IMAP_HOST"),
    (ENV | {"IMAP_USER": "a", "IMAP_PASSWORD": "p", "IMAP_PORT": "x"}, "IMAP_PORT"),
])
def test_invalid_env_accounts(env, msg):
    with pytest.raises(AccountsError, match=msg):
        EnvAccounts(env)


def test_api_settings():
    s = load_settings({"S3_BUCKET": "b", "FIREBASE_PROJECT_ID": "proj", "PORT": "9000",
                       "AIMAP_ALLOWED_EMAILS": " Me@Example.com, ,you@example.com "}, need_accounts=False)
    assert s.api.firebase_project_id == "proj" and s.api.port == 9000
    assert s.api.allowed_emails == frozenset({"me@example.com", "you@example.com"})
    assert load_settings({"S3_BUCKET": "b"}, need_accounts=False).api.allowed_emails == frozenset()
    with pytest.raises(ConfigError, match="API_POOL_SIZE"):
        load_settings({"S3_BUCKET": "b", "API_POOL_SIZE": "0"}, need_accounts=False)
