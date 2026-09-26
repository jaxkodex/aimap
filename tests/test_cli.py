import io
import json
import os

import boto3
import pytest
from conftest import BUCKET
from moto import mock_aws

from aimap import cli
from aimap.crypto import Cipher, generate_key


@pytest.fixture
def env(monkeypatch, tmp_path):
    with mock_aws():
        boto3.client("s3", region_name="us-east-1").create_bucket(Bucket=BUCKET)
        for k in ("IMAP_USER", "ACCOUNTS_SOURCE"):
            monkeypatch.delenv(k, raising=False)
        monkeypatch.setenv("S3_BUCKET", BUCKET)
        monkeypatch.setenv("AWS_DEFAULT_REGION", "us-east-1")
        monkeypatch.setenv("AIMAP_SECRET_KEY", generate_key())
        monkeypatch.chdir(tmp_path)  # no stray .env
        logins = []
        monkeypatch.setattr(cli, "verify_login", lambda acct, timeout: logins.append((acct.user, acct.password)))
        yield logins


def run(monkeypatch, *argv, stdin=""):
    monkeypatch.setattr("sys.stdin", io.StringIO(stdin))
    with pytest.raises(SystemExit) as e:
        cli.main(list(argv))
    return e.value.code


def stored():
    body = boto3.client("s3", region_name="us-east-1").get_object(Bucket=BUCKET, Key="config/accounts.json")
    return json.loads(body["Body"].read())["accounts"]


def test_add_list_update_remove(env, monkeypatch, capsys):
    assert run(monkeypatch, "accounts", "add", "me@x.com", "--host", "imap.x.com", "--mailbox", "INBOX",
               "--mailbox", "Sent", "--password-stdin", stdin="abcd efgh\n") == 0
    assert env == [("me@x.com", "abcdefgh")]  # verified before saving, spaces stripped
    [rec] = stored()
    assert rec["mailboxes"] == ["INBOX", "Sent"] and "abcdefgh" not in json.dumps(rec)

    assert run(monkeypatch, "accounts", "add", "me@x.com", "--host", "h", "--password-stdin", stdin="p\n") == 1
    assert "already exists" in capsys.readouterr().err

    assert run(monkeypatch, "accounts", "disable", "me@x.com") == 0
    assert stored()[0]["enabled"] is False
    assert run(monkeypatch, "accounts", "update", "me@x.com", "--port", "1993") == 0
    assert stored()[0]["port"] == 1993

    capsys.readouterr()
    assert run(monkeypatch, "accounts", "list") == 0
    out = capsys.readouterr().out
    assert "me@x.com" in out and "abcdefgh" not in out and " ok " in out

    assert run(monkeypatch, "accounts", "remove", "me@x.com") == 0
    assert stored() == []


def test_failed_login_does_not_save(env, monkeypatch, capsys):
    def boom(acct, timeout):
        raise OSError("bad credentials")

    monkeypatch.setattr(cli, "verify_login", boom)
    assert run(monkeypatch, "accounts", "add", "a", "--host", "h", "--password-stdin", stdin="p\n") == 1
    assert "login check failed" in capsys.readouterr().err
    assert run(monkeypatch, "accounts", "list") == 0
    assert "no accounts" in capsys.readouterr().out


def test_set_password_and_rotate_key(env, monkeypatch):
    run(monkeypatch, "accounts", "add", "a", "--host", "h", "--no-verify", "--password-stdin", stdin="old\n")
    assert run(monkeypatch, "accounts", "set-password", "a", "--password-stdin", stdin="new\n") == 0
    assert env[-1] == ("a", "new")

    old = os.environ["AIMAP_SECRET_KEY"]
    new = generate_key()
    monkeypatch.setenv("AIMAP_SECRET_KEY", f"{new},{old}")
    assert run(monkeypatch, "accounts", "rotate-key") == 0
    assert Cipher(new).decrypt("a", stored()[0]["credentials"]) == "new"


def test_password_prompt_needs_tty_or_stdin_flag(env, monkeypatch, capsys):
    assert run(monkeypatch, "accounts", "add", "a", "--host", "h") == 1
    assert "--password-stdin" in capsys.readouterr().err


def test_keygen(capsys):
    cli.main(["keygen"])
    Cipher(capsys.readouterr().out.strip())  # a valid key
