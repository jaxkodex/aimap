"""Command line entry point.

    aimap run      poll forever (the container default)
    aimap once     one pass over every account, then exit (cron, debugging)
    aimap check    validate config and connectivity to S3 and every IMAP account
"""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path

from aimap import __version__, logs
from aimap.config import ConfigError, load_dotenv, load_settings
from aimap.imap import ImapSource
from aimap.storage import Store
from aimap.worker import Worker

log = logging.getLogger("aimap")


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="aimap", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", action="version", version=f"aimap {__version__}")
    parser.add_argument("--env-file", default=".env", help="load variables from this file if it exists")
    parser.add_argument("command", nargs="?", default="run", choices=["run", "once", "check"])
    args = parser.parse_args(argv)

    load_dotenv(Path(args.env_file))
    try:
        settings = load_settings()
    except ConfigError as e:
        logs.setup()
        log.error("invalid configuration", extra={"error": str(e)})
        sys.exit(2)
    logs.setup(settings.log_level, settings.log_format)

    store = Store(settings.s3)
    try:
        store.check()
    except Exception as e:
        log.error("cannot reach S3 bucket", extra={"bucket": settings.s3.bucket, "error": str(e)})
        sys.exit(3)

    if args.command == "check":
        ok = True
        for acct in settings.accounts:
            try:
                src = ImapSource(acct, timeout=settings.imap_timeout)
                for mb in acct.mailboxes:
                    src.select(mb)
                src.close()
                log.info("imap ok", extra={"account": acct.user, "mailboxes": list(acct.mailboxes)})
            except Exception as e:
                ok = False
                log.error("imap failed", extra={"account": acct.user, "error": str(e)})
        log.info("s3 ok", extra={"bucket": settings.s3.bucket})
        sys.exit(0 if ok else 1)

    worker = Worker(settings, store)
    worker.install_signal_handlers()
    if args.command == "once":
        stored = worker.run_once()
        log.info("pass complete", extra={"stored": stored, "failed_accounts": worker.failed_accounts})
        sys.exit(1 if worker.failed_accounts else 0)
    else:
        worker.run_forever()
