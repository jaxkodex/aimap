"""Run the ingest worker, the classifier and the API as child processes of one container.

Each child is a normal `aimap run`, `aimap classify` or `aimap api` process with its own signal handlers and
database pool. SIGTERM and SIGINT are forwarded to every child. If one child exits, the others are stopped
and the supervisor exits with that child's code, so the platform restarts the whole container.
"""

from __future__ import annotations

import logging
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence

log = logging.getLogger("aimap.supervise")

SERVICES = (("worker", ("run",)), ("classify", ("classify",)), ("api", ("api",)))


def aimap_commands(env_file: str) -> list[tuple[str, list[str]]]:
    base = [sys.executable, "-m", "aimap", "--env-file", env_file]
    return [(name, [*base, *args]) for name, args in SERVICES]


class Supervisor:
    def __init__(self, commands: Sequence[tuple[str, list[str]]], *, grace: float = 30.0, poll: float = 0.5):
        self.commands = commands
        self.grace = grace
        self.poll = poll
        self.stop = threading.Event()

    def run(self) -> int:
        procs = {name: subprocess.Popen(argv) for name, argv in self.commands}
        for name, p in procs.items():
            log.info("child started", extra={"child": name, "pid": p.pid})
        code = 0
        while not self.stop.wait(self.poll):
            exited = [(name, p.returncode) for name, p in procs.items() if p.poll() is not None]
            if exited:
                name, code = exited[0]
                log.error("child exited; stopping the others", extra={"child": name, "code": code})
                code = code or 1  # a child is never meant to exit on its own
                break
        self._shutdown(procs)
        return code

    def _shutdown(self, procs: dict[str, subprocess.Popen]) -> None:
        for p in procs.values():
            if p.poll() is None:
                p.terminate()
        deadline = time.monotonic() + self.grace
        for name, p in procs.items():
            try:
                p.wait(max(0.0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                log.warning("child did not stop in time; killing it", extra={"child": name})
                p.kill()
                p.wait()
            log.info("child stopped", extra={"child": name, "code": p.returncode})

    def install_signal_handlers(self) -> None:
        def handle(signum, _frame):
            log.info("shutdown requested", extra={"signal": signal.Signals(signum).name})
            self.stop.set()

        signal.signal(signal.SIGTERM, handle)
        signal.signal(signal.SIGINT, handle)
