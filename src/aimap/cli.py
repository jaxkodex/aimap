"""aimap: copy IMAP mail to S3, record it in Postgres and classify it with Jev.

Worker (ingestion):
    aimap run                  poll forever (container default)
    aimap once                 one pass over every account, exit 1 if any failed
    aimap check                verify S3, Postgres and log in to every account

Classifier:
    aimap classify             take classify jobs forever
    aimap classify --once      process every due job, then exit
    aimap jobs status          job counts per stage and status
    aimap jobs retry [--done]  send failed (and with --done, finished) jobs back to the queue

Database:
    aimap migrate              apply missing schema migrations
    aimap backfill             record messages already in S3 and queue them

Profiles (what "important" means, per account):
    aimap profiles list | show NAME | set NAME --file profile.json
    aimap patterns list PROFILE | import PROFILE FILE [--format csv|json]
    aimap accounts set-profile USER PROFILE

Accounts file (live: the running worker picks changes up on its next poll):
    aimap accounts list
    aimap accounts add USER --host HOST [--port 993] [--mailbox INBOX ...]
    aimap accounts update USER [--host] [--port] [--mailbox ...]
    aimap accounts set-password USER
    aimap accounts enable USER | disable USER | remove USER
    aimap accounts rotate-key  re-encrypt every password with the first AIMAP_SECRET_KEY

Keys:
    aimap keygen               print a new AIMAP_SECRET_KEY
"""

from __future__ import annotations

import argparse
import getpass
import json
import logging
import os
import sys
from pathlib import Path

from aimap import __version__, db, logs, profiles, queue
from aimap.accounts import (
    Account,
    AccountRecord,
    AccountsDoc,
    AccountsError,
    AccountSource,
    AccountsRepo,
    BucketAccounts,
    EnvAccounts,
    clean_password,
    parse_mailboxes,
)
from aimap.backfill import backfill
from aimap.catalog import PgCatalog
from aimap.classifier import Classifier, TypeSafeAsker
from aimap.config import ConfigError, Settings, load_dotenv, load_settings
from aimap.crypto import Cipher, CryptoError, generate_key
from aimap.imap import ImapSource
from aimap.storage import ConflictError, Store
from aimap.worker import Worker

log = logging.getLogger("aimap")


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="aimap", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--version", action="version", version=f"aimap {__version__}")
    p.add_argument("--env-file", default=".env", help="load variables from this file if it exists")
    sub = p.add_subparsers(dest="command")
    sub.add_parser("run", help="poll forever")
    sub.add_parser("once", help="one pass, then exit")
    sub.add_parser("check", help="verify S3, Postgres and IMAP connectivity")
    sub.add_parser("keygen", help="print a new AIMAP_SECRET_KEY")
    sub.add_parser("migrate", help="apply missing database migrations")
    sub.add_parser("backfill", help="record messages already in S3 and queue them for classification")
    cls = sub.add_parser("classify", help="classify queued messages with Jev")
    cls.add_argument("--once", action="store_true", help="process every due job, then exit")

    jobs = sub.add_parser("jobs", help="inspect the job queue").add_subparsers(dest="action", required=True)
    jobs.add_parser("status", help="job counts per stage and status")
    retry = jobs.add_parser("retry", help="send failed jobs back to the queue")
    retry.add_argument("--done", action="store_true",
                       help="also requeue finished jobs; messages whose profile, patterns or model changed "
                            "are classified again, the rest are skipped")

    prof = sub.add_parser("profiles", help="manage classification profiles").add_subparsers(
        dest="action", required=True)
    prof.add_parser("list", help="profiles with their account and pattern counts")
    prof.add_parser("show", help="print a profile as JSON").add_argument("name")
    pset = prof.add_parser("set", help="create or replace a profile from a JSON file")
    pset.add_argument("name")
    pset.add_argument("--file", required=True, help="JSON object, '-' for stdin")

    pat = sub.add_parser("patterns", help="manage a profile's known patterns").add_subparsers(
        dest="action", required=True)
    pat.add_parser("list", help="show a profile's patterns").add_argument("profile")
    pimp = pat.add_parser("import", help="replace a profile's patterns with the contents of a file")
    pimp.add_argument("profile")
    pimp.add_argument("file", help="CSV or JSON file, '-' for stdin")
    pimp.add_argument("--format", choices=["csv", "json"], help="default: from the file extension")

    acc = sub.add_parser("accounts", help="manage the encrypted accounts file").add_subparsers(
        dest="action", required=True)
    acc.add_parser("list", help="show accounts (never shows passwords)")

    def conn_args(sp: argparse.ArgumentParser, required: bool) -> None:
        sp.add_argument("--host", required=required, help="IMAP host, e.g. imap.gmail.com")
        sp.add_argument("--port", type=int, default=993 if required else None)
        sp.add_argument("--mailbox", action="append", dest="mailboxes", metavar="NAME",
                        help="mailbox to sync; repeat for several (default INBOX)")

    def pw_args(sp: argparse.ArgumentParser) -> None:
        sp.add_argument("--password-stdin", action="store_true", help="read the password from stdin")
        sp.add_argument("--no-verify", action="store_true", help="save without test-logging in to IMAP")

    add = acc.add_parser("add", help="add an account")
    add.add_argument("user")
    conn_args(add, required=True)
    pw_args(add)
    add.add_argument("--disabled", action="store_true", help="add it disabled")

    upd = acc.add_parser("update", help="change host, port or mailboxes")
    upd.add_argument("user")
    conn_args(upd, required=False)
    upd.add_argument("--no-verify", action="store_true")

    spw = acc.add_parser("set-password", help="replace an account's password")
    spw.add_argument("user")
    pw_args(spw)

    for name in ("enable", "disable", "remove"):
        acc.add_parser(name, help=f"{name} an account").add_argument("user")
    acc.add_parser("rotate-key", help="re-encrypt every password with the first key")
    sp = acc.add_parser("set-profile", help="classify an account's mail with another profile (stored in Postgres)")
    sp.add_argument("user")
    sp.add_argument("profile")
    return p


DB_COMMANDS = {"migrate", "backfill", "classify", "jobs", "profiles", "patterns"}


def main(argv: list[str] | None = None) -> None:
    argv = sys.argv[1:] if argv is None else list(argv)
    if argv[:1] == ["aimap"]:  # the image's ENTRYPOINT is aimap; accept "aimap migrate" as a command too
        argv = argv[1:]
    args = build_parser().parse_args(argv)
    command = args.command or "run"

    if command == "keygen":
        print(generate_key())
        return

    load_dotenv(Path(args.env_file))
    set_profile = command == "accounts" and args.action == "set-profile"
    try:
        settings = load_settings(need_accounts=command not in DB_COMMANDS and not set_profile)
    except ConfigError as e:
        logs.setup()
        log.error("invalid configuration", extra={"error": str(e)})
        sys.exit(2)

    if command == "accounts" and not set_profile:
        logs.setup(settings.log_level, "text")
        sys.exit(accounts_command(args, settings))

    interactive = command in {"jobs", "profiles", "patterns", "accounts", "migrate"}
    logs.setup(settings.log_level, "text" if interactive else settings.log_format)
    if not settings.database_url:
        log.error("DATABASE_URL is required")
        sys.exit(2)

    if command == "migrate":
        try:
            applied = db.migrate(settings.database_url)
        except Exception as e:
            log.error("migration failed", extra={"error": str(e)})
            sys.exit(4)
        print(f"applied {len(applied)} migration(s)" + (f": {', '.join(applied)}" if applied else ""))
        return

    try:
        pool = db.open_pool(settings.database_url, max_size=settings.classifier.concurrency + 2)
        db.require_current(pool)
    except db.SchemaError as e:
        log.error(str(e))
        sys.exit(4)
    except Exception as e:
        log.error("cannot reach the database", extra={"error": str(e)})
        sys.exit(4)

    try:
        if command in {"jobs", "profiles", "patterns", "accounts"}:
            sys.exit(admin_command(command, args, pool))
        sys.exit(service_command(command, args, settings, pool))
    finally:
        pool.close()


def service_command(command: str, args: argparse.Namespace, settings: Settings, pool) -> int:
    store = Store(settings.s3)
    try:
        store.check()
    except Exception as e:
        log.error("cannot reach S3 bucket", extra={"bucket": settings.s3.bucket, "error": str(e)})
        return 3
    catalog = PgCatalog(pool)

    if command == "backfill":
        res = backfill(store, catalog)
        log.info("backfill complete", extra=vars(res))
        return 0

    if command == "classify":
        cs = settings.classifier
        if not cs.api_key:
            log.error("JEV_API_KEY is required for classify")
            return 2
        classifier = Classifier(cs, pool, store, lambda: TypeSafeAsker(cs.api_key, cs.model, cs.request_timeout))
        classifier.install_signal_handlers()
        if args.once:
            n = classifier.drain()
            log.info("classify pass complete", extra={"processed": n})
        else:
            classifier.run_forever()
        return 0

    try:
        source = account_source(settings, store)
    except (AccountsError, CryptoError) as e:
        log.error("invalid accounts configuration", extra={"error": str(e)})
        return 2

    if command == "check":
        log.info("database ok")
        return check(settings, source)

    worker = Worker(settings, store, catalog, source)
    worker.install_signal_handlers()
    if command == "once":
        stored = worker.run_once()
        log.info("pass complete", extra={"stored": stored, "failed_accounts": worker.failed_accounts})
        return 1 if worker.failed_accounts else 0
    worker.run_forever()
    return 0


def _read_input(path: str) -> str:
    return sys.stdin.read() if path == "-" else Path(path).read_text(encoding="utf-8")


def admin_command(command: str, args: argparse.Namespace, pool) -> int:
    try:
        with pool.connection() as conn:
            return _admin(command, args, conn)
    except (profiles.ProfileError, ValueError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


def _admin(command: str, args: argparse.Namespace, conn) -> int:
    action = args.action
    if command == "jobs":
        if action == "status":
            rows = queue.counts(conn)
            if not rows:
                print("no jobs")
            for stage, status, n in rows:
                print(f"{stage:<10} {status:<8} {n}")
            return 0
        statuses = ("failed", "done") if args.done else ("failed",)
        print(f"requeued {queue.requeue(conn, 'classify', statuses)} job(s)")
        return 0

    if command == "profiles":
        if action == "list":
            print(f"{'PROFILE':<20} {'ACCOUNTS':>8} {'PATTERNS':>8}")
            for name, accounts, patterns in profiles.list_all(conn):
                print(f"{name:<20} {accounts:>8} {patterns:>8}")
            return 0
        if action == "show":
            print(json.dumps(profiles.get(conn, args.name).profile, indent=2, ensure_ascii=False))
            return 0
        profiles.put(conn, args.name, json.loads(_read_input(args.file)))
        print(f"saved profile {args.name}")
        return 0

    if command == "patterns":
        profile = profiles.get(conn, args.profile)
        if action == "list":
            for p in profiles.patterns(conn, profile.id):
                print(f"{p.importance:<6} {p.action_bucket:<12} {p.insight}  [{'; '.join(p.tags)}]")
            return 0
        fmt = args.format or ("json" if args.file.endswith(".json") else "csv")
        items = profiles.parse_patterns(_read_input(args.file), fmt)
        with conn.transaction():
            profiles.replace_patterns(conn, profile.id, items)
        print(f"imported {len(items)} pattern(s) into {args.profile}")
        return 0

    if command == "accounts" and action == "set-profile":
        profiles.assign(conn, args.user, args.profile)
        print(f"{args.user} now uses profile {args.profile}")
        return 0
    raise AssertionError((command, action))


def account_source(settings: Settings, store: Store) -> AccountSource:
    if settings.accounts_source == "env":
        return EnvAccounts(os.environ)
    return BucketAccounts(AccountsRepo(store, settings.accounts_path), Cipher(settings.secret_key))


def check(settings: Settings, source: AccountSource) -> int:
    log.info("s3 ok", extra={"bucket": settings.s3.bucket})
    accounts = source.load()
    if not accounts:
        log.warning("no enabled accounts")
    ok = True
    for acct in accounts:
        try:
            verify_login(acct, settings.imap_timeout)
            log.info("imap ok", extra={"account": acct.user, "mailboxes": list(acct.mailboxes)})
        except Exception as e:
            ok = False
            log.error("imap failed", extra={"account": acct.user, "error": str(e)})
    return 0 if ok else 1


def verify_login(acct: Account, timeout: float) -> None:
    src = ImapSource(acct, timeout=timeout)
    try:
        for mb in acct.mailboxes:
            src.select(mb)
    finally:
        src.close()


# --------------------------------------------------------------------------------------
# accounts subcommands
# --------------------------------------------------------------------------------------


def read_password(from_stdin: bool, confirm: bool) -> str:
    if from_stdin:
        pw = sys.stdin.readline().rstrip("\r\n")
    elif sys.stdin.isatty():
        pw = getpass.getpass("IMAP password: ")
        if confirm and getpass.getpass("Repeat password: ") != pw:
            raise AccountsError("passwords do not match")
    else:
        raise AccountsError("no terminal to prompt for the password; pipe it with --password-stdin")
    pw = clean_password(pw)
    if not pw:
        raise AccountsError("empty password")
    return pw


def accounts_command(args: argparse.Namespace, settings: Settings) -> int:
    if not settings.secret_key:
        log.error("AIMAP_SECRET_KEY is required to manage the accounts file (generate: aimap keygen)")
        return 2
    try:
        return _accounts(args, settings, Cipher(settings.secret_key), AccountsRepo(Store(settings.s3),
                                                                                   settings.accounts_path))
    except (AccountsError, CryptoError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    except ConflictError:
        print("error: the accounts file kept changing under us; try again", file=sys.stderr)
        return 1


def _accounts(args, settings: Settings, cipher: Cipher, repo: AccountsRepo) -> int:
    action = args.action

    if action == "list":
        doc = repo.read()
        if not doc.records:
            print(f"no accounts in s3://{settings.s3.bucket}/{repo.key}")
            return 0
        rows = [("USER", "HOST", "PORT", "MAILBOXES", "ENABLED", "KEY", "UPDATED")]
        for r in doc.records:
            try:
                cipher.decrypt(r.user, r.credentials)
                key_ok = "ok"
            except CryptoError:
                key_ok = "cannot decrypt"
            rows.append((r.user, r.host, str(r.port), ",".join(r.mailboxes), "yes" if r.enabled else "no",
                         key_ok, r.updated_at))
        widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
        for row in rows:
            print("  ".join(c.ljust(w) for c, w in zip(row, widths, strict=True)).rstrip())
        return 0

    if action == "add":
        if repo.read().get(args.user):
            raise AccountsError(f"account {args.user!r} already exists (use update or set-password)")
        password = read_password(args.password_stdin, confirm=args.no_verify)
        acct = Account(user=args.user, password=password, host=args.host, port=args.port,
                       mailboxes=parse_mailboxes(args.mailboxes))
        if not args.no_verify:
            _verify(acct, settings)
        record = AccountRecord(user=acct.user, host=acct.host, port=acct.port, mailboxes=list(acct.mailboxes),
                               enabled=not args.disabled, credentials=cipher.encrypt(acct.user, password))

        def add(doc: AccountsDoc) -> None:
            if doc.get(args.user):
                raise AccountsError(f"account {args.user!r} was added by someone else meanwhile")
            doc.records.append(record)

        repo.modify(add)
        print(f"added {args.user}" + (" (disabled)" if args.disabled else ""))
        return 0

    if action == "update":
        current = repo.read().require(args.user)
        changes = {k: v for k, v in (("host", args.host), ("port", args.port)) if v is not None}
        if args.mailboxes:
            changes["mailboxes"] = list(parse_mailboxes(args.mailboxes))
        if not changes:
            raise AccountsError("nothing to change: pass --host, --port or --mailbox")
        if not args.no_verify:
            acct = current.to_account(cipher)
            _verify(Account(user=acct.user, password=acct.password, host=changes.get("host", acct.host),
                            port=changes.get("port", acct.port),
                            mailboxes=tuple(changes.get("mailboxes", acct.mailboxes))), settings)
        repo.modify(lambda doc: doc.require(args.user).update(**changes))
        print(f"updated {args.user}: {', '.join(changes)}")
        return 0

    if action == "set-password":
        current = repo.read().require(args.user)
        password = read_password(args.password_stdin, confirm=args.no_verify)
        if not args.no_verify:
            _verify(Account(user=current.user, password=password, host=current.host, port=current.port,
                            mailboxes=tuple(current.mailboxes)), settings)
        token = cipher.encrypt(args.user, password)
        repo.modify(lambda doc: doc.require(args.user).update(credentials=token))
        print(f"password updated for {args.user}")
        return 0

    if action in {"enable", "disable"}:
        repo.modify(lambda doc: doc.require(args.user).update(enabled=action == "enable"))
        print(f"{action}d {args.user}")
        return 0

    if action == "remove":
        def remove(doc: AccountsDoc) -> None:
            doc.records.remove(doc.require(args.user))

        repo.modify(remove)
        print(f"removed {args.user} (its stored mail stays in the bucket, its rows in Postgres)")
        return 0

    if action == "rotate-key":
        def rotate(doc: AccountsDoc) -> None:
            for r in doc.records:
                r.credentials = cipher.rotate(r.credentials)
                cipher.decrypt(r.user, r.credentials)  # still bound to the right user

        doc = repo.modify(rotate)
        print(f"re-encrypted {len(doc.records)} account(s) with the primary key")
        return 0

    raise AssertionError(action)


def _verify(acct: Account, settings: Settings) -> None:
    print(f"logging in to {acct.host}:{acct.port} as {acct.user} ...", file=sys.stderr)
    try:
        verify_login(acct, settings.imap_timeout)
    except Exception as e:
        raise AccountsError(f"login check failed ({e}); fix it or pass --no-verify") from e
