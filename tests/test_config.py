import pytest

from aimap.config import ConfigError, load_settings

BASE = {"S3_BUCKET": "b", "IMAP_HOST": "imap.example.com"}


def test_multiple_accounts_with_fallbacks():
    s = load_settings(BASE | {
        "IMAP_USER": "a@x.com", "IMAP_PASSWORD": "abcd efgh",
        "IMAP_USER2": "b@x.com", "IMAP_PASSWORD2": "p2", "IMAP_HOST2": "imap.other.com",
        "IMAP_MAILBOX2": "INBOX, Archive",
    })
    a, b = s.accounts
    assert (a.user, a.password, a.host, a.mailboxes) == ("a@x.com", "abcdefgh", "imap.example.com", ("INBOX",))
    assert (b.host, b.mailboxes) == ("imap.other.com", ("INBOX", "Archive"))


def test_accounts_sorted_numerically():
    env = BASE | {f"IMAP_USER{n}": f"u{n}" for n in (10, 2)} | {f"IMAP_PASSWORD{n}": "p" for n in (10, 2)}
    assert [a.user for a in load_settings(env).accounts] == ["u2", "u10"]


def test_password_never_in_repr():
    s = load_settings(BASE | {"IMAP_USER": "a", "IMAP_PASSWORD": "hunter2"})
    assert "hunter2" not in repr(s)


def test_prefix_normalised():
    s = load_settings(BASE | {"IMAP_USER": "a", "IMAP_PASSWORD": "p", "S3_PREFIX": "/mail/"})
    assert s.s3.prefix == "mail/"


@pytest.mark.parametrize("env, msg", [
    ({"IMAP_USER": "a", "IMAP_PASSWORD": "p"}, "S3_BUCKET"),
    ({"S3_BUCKET": "b"}, "No accounts"),
    (BASE | {"IMAP_USER": "a"}, "IMAP_PASSWORD"),
    ({"S3_BUCKET": "b", "IMAP_USER": "a", "IMAP_PASSWORD": "p"}, "IMAP_HOST"),
    (BASE | {"IMAP_USER": "a", "IMAP_PASSWORD": "p", "POLL_INTERVAL_SECONDS": "x"}, "POLL_INTERVAL"),
])
def test_invalid(env, msg):
    with pytest.raises(ConfigError, match=msg):
        load_settings(env)
