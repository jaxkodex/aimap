import sys
import threading
import time

from aimap import supervise
from aimap.supervise import Supervisor


def py(code: str) -> list[str]:
    return [sys.executable, "-c", code]


SLEEP = py("import time; time.sleep(60)")
STUBBORN = py("import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('ready', flush=True); "
              "time.sleep(60)")


def test_one_child_exiting_stops_the_others_and_passes_its_code():
    sup = Supervisor([("a", SLEEP), ("b", py("raise SystemExit(3)")), ("c", SLEEP)], grace=5, poll=0.05)
    started = time.monotonic()
    assert sup.run() == 3
    assert time.monotonic() - started < 5  # the sleepers were terminated, not waited out


def test_a_clean_exit_still_counts_as_failure():
    sup = Supervisor([("a", SLEEP), ("b", py("pass"))], grace=5, poll=0.05)
    assert sup.run() == 1


def test_stop_terminates_every_child_and_exits_zero():
    sup = Supervisor([("a", SLEEP), ("b", SLEEP)], grace=5, poll=0.05)
    threading.Timer(0.3, sup.stop.set).start()
    assert sup.run() == 0


def test_child_ignoring_sigterm_is_killed_after_grace():
    sup = Supervisor([("a", STUBBORN)], grace=0.5, poll=0.05)
    threading.Timer(1.0, sup.stop.set).start()  # after the child has installed its handler
    started = time.monotonic()
    assert sup.run() == 0
    assert time.monotonic() - started < 5


def test_aimap_commands_run_each_service_with_the_same_env_file():
    cmds = dict(supervise.aimap_commands("custom.env"))
    assert set(cmds) == {"worker", "classify", "api"}
    for argv in cmds.values():
        assert argv[:5] == [sys.executable, "-m", "aimap", "--env-file", "custom.env"]
    assert [cmds["worker"][-1], cmds["classify"][-1], cmds["api"][-1]] == ["run", "classify", "api"]
