import io
import json

import boto3
import pytest
from conftest import BUCKET, email_bytes
from moto import mock_aws

from aimap import cli


@pytest.fixture
def env(monkeypatch, tmp_path, pool, pg_dsn):
    with mock_aws():
        boto3.client("s3", region_name="us-east-1").create_bucket(Bucket=BUCKET)
        for k in ("IMAP_USER", "ACCOUNTS_SOURCE", "AIMAP_SECRET_KEY", "JEV_API_KEY", "TYPESAFE_API_KEY"):
            monkeypatch.delenv(k, raising=False)
        monkeypatch.setenv("S3_BUCKET", BUCKET)
        monkeypatch.setenv("AWS_DEFAULT_REGION", "us-east-1")
        monkeypatch.setenv("DATABASE_URL", pg_dsn)
        monkeypatch.chdir(tmp_path)
        yield tmp_path


def run(monkeypatch, *argv, stdin=""):
    monkeypatch.setattr("sys.stdin", io.StringIO(stdin))
    try:
        cli.main(list(argv))
    except SystemExit as e:
        return e.code
    return 0


def test_migrate_needs_no_secret_key(env, monkeypatch, capsys):
    assert run(monkeypatch, "migrate") == 0
    assert "applied 0 migration(s)" in capsys.readouterr().out


def test_database_url_required(env, monkeypatch):
    monkeypatch.delenv("DATABASE_URL")
    assert run(monkeypatch, "jobs", "status") == 2


def test_profiles_patterns_and_account_assignment(env, monkeypatch, capsys):
    (env / "work.json").write_text(json.dumps({"who": "Alex at the studio."}))
    assert run(monkeypatch, "profiles", "set", "work", "--file", "work.json") == 0
    assert run(monkeypatch, "profiles", "show", "work") == 0
    assert json.loads(capsys.readouterr().out.split("\n", 1)[1]) == {"who": "Alex at the studio."}

    (env / "p.csv").write_text("insight,importance,action_bucket,tags,example_from,example_subject\n"
                               "Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 12\n"
                               "Client invoices,High,act_now,work;billing,Client <c@x>,Invoice 13\n"
                               "Shop promos,Low,discard,promo,,\n")
    assert run(monkeypatch, "patterns", "import", "work", "p.csv") == 0
    assert run(monkeypatch, "patterns", "list", "work") == 0
    out = capsys.readouterr().out
    assert "imported 2 pattern(s)" in out and "act_now" in out and "work; billing" in out

    (env / "bad.json").write_text(json.dumps([{"insight": "x", "importance": "Huge", "action_bucket": "skim"}]))
    assert run(monkeypatch, "patterns", "import", "work", "bad.json") == 1
    assert "importance must be" in capsys.readouterr().err

    assert run(monkeypatch, "accounts", "set-profile", "me@x.com", "work") == 0
    assert run(monkeypatch, "accounts", "set-profile", "me@x.com", "nope") == 1
    assert run(monkeypatch, "profiles", "list") == 0
    out = capsys.readouterr().out
    assert "now uses profile work" in out and "work" in out.splitlines()[-1]


def test_backfill_then_jobs(env, monkeypatch, capsys, pool):
    boto3.client("s3", region_name="us-east-1").put_object(
        Bucket=BUCKET, Key="raw/me@x.com/INBOX/1/1.eml", Body=email_bytes(1))
    assert run(monkeypatch, "backfill") == 0
    assert run(monkeypatch, "jobs", "status") == 0
    assert "classify   pending  1" in capsys.readouterr().out
    assert run(monkeypatch, "jobs", "retry") == 0
    assert "requeued 0 job(s)" in capsys.readouterr().out  # only failed ones without --done
    with pool.connection() as conn:
        conn.execute("UPDATE jobs SET status = 'done'")
    assert run(monkeypatch, "jobs", "retry", "--done") == 0
    assert "requeued 1 job(s)" in capsys.readouterr().out


def test_classify_needs_api_key(env, monkeypatch):
    assert run(monkeypatch, "classify", "--once") == 2


@pytest.mark.parametrize("env_vars", [{}, {"FIREBASE_PROJECT_ID": "proj"}])
def test_api_refuses_to_start_without_firebase_settings(env, monkeypatch, env_vars):
    for k in ("FIREBASE_PROJECT_ID", "AIMAP_ALLOWED_EMAILS"):
        monkeypatch.delenv(k, raising=False)
    for k, v in env_vars.items():
        monkeypatch.setenv(k, v)
    assert run(monkeypatch, "migrate") == 0
    assert run(monkeypatch, "api") == 2
