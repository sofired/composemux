"""Shared helpers for the demo services.

Everything here exists to make container output interesting to look at:
colour, timestamps, progress bars that redraw in place, and stack traces with
enough frames to need scrolling. Standard library only, so the services run
straight from a stock python image with no build step.
"""

import os
import random
import signal
import sys
import time

RESET = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"

RED = "\033[31m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
BLUE = "\033[34m"
MAGENTA = "\033[35m"
CYAN = "\033[36m"
GREY = "\033[90m"

LEVEL_COLOUR = {
    "DEBUG": GREY,
    "INFO": GREEN,
    "WARN": YELLOW,
    "ERROR": RED,
    "FATAL": BOLD + RED,
}

STATE = "/state"
FLAGS = os.path.join(STATE, "flags")
JOBS = os.path.join(STATE, "jobs")


def ts():
    """Timestamp with milliseconds - the kind of prefix real services emit."""
    now = time.time()
    return time.strftime("%H:%M:%S", time.localtime(now)) + f".{int(now % 1 * 1000):03d}"


def log(level, message, colour=None):
    """One log line: dim timestamp, coloured level, message. Flushed immediately.

    Flushing matters. Python buffers stdout when it is a pipe rather than a
    terminal, and a service whose output sits in a buffer looks stalled in any
    log viewer - composemux included.
    """
    tint = colour or LEVEL_COLOUR.get(level, "")
    sys.stdout.write(f"{DIM}{ts()}{RESET} {tint}{level:<5}{RESET} {message}\n")
    sys.stdout.flush()


def raw(text):
    """Write without a newline - for progress bars that redraw with \\r."""
    sys.stdout.write(text)
    sys.stdout.flush()


def progress(label, steps=24, delay=0.06, width=22):
    """A percentage bar that redraws in place using carriage returns.

    composemux runs container output through a vt100 emulator, so this renders
    as a single updating line rather than smearing across the pane.
    """
    for step in range(steps + 1):
        pct = step / steps
        filled = int(pct * width)
        bar = "#" * filled + "-" * (width - filled)
        raw(f"\r{DIM}{ts()}{RESET} {CYAN}..... {label} {RESET}[{bar}] {int(pct * 100):3d}%")
        time.sleep(delay)
    raw("\n")


def flag(name):
    """True while the activity script is holding a flag file open on us."""
    return os.path.exists(os.path.join(FLAGS, name))


def install_clean_exit(on_stop=None):
    """Exit 0 on SIGTERM so `compose stop` reads as a clean shutdown.

    Without this, Python dies on the default SIGTERM disposition and Docker
    records exit 143 - which composemux correctly shows as a failure, and which
    would keep the auto-exit countdown from ever running.
    """

    def handler(signum, _frame):
        name = signal.Signals(signum).name
        log("INFO", f"{name} received, shutting down cleanly", colour=CYAN)
        if on_stop:
            on_stop()
        log("INFO", "bye", colour=CYAN)
        sys.exit(0)

    signal.signal(signal.SIGTERM, handler)
    signal.signal(signal.SIGINT, handler)


# A chain of distinct frames. Recursion would be shorter to write, but Python
# collapses repeated frames into "[Previous line repeated N more times]" and the
# result is a dozen lines - not the forty-line trace that makes a pane worth
# scrolling.


def _socket_send(payload):
    raise ConnectionResetError(104, "Connection reset by peer")


def _stream_flush(conn, payload):
    return _socket_send(payload)


def _wire_write(conn, frame):
    return _stream_flush(conn, frame.encode())


def _pg_execute(conn, sql):
    return _wire_write(conn, sql)


def _pool_acquire_and_run(sql):
    return _pg_execute(object(), sql)


def _session_execute(sql):
    return _pool_acquire_and_run(sql)


def _repository_load(entity, key):
    return _session_execute(f"SELECT * FROM {entity} WHERE id = {key}")


def _unit_of_work(entity, key):
    return _repository_load(entity, key)


def _service_layer(key):
    try:
        return _unit_of_work("orders", key)
    except ConnectionResetError as exc:
        raise IOError(f"lost connection loading order {key}") from exc


def _retry_wrapper(key, attempts=2):
    return _service_layer(key)


def _command_handler(key):
    return _retry_wrapper(key)


def _dispatch(command):
    return _command_handler(command["key"])


def deep_traceback(depth=None):
    """Raise a RuntimeError with a two-level cause chain, ~45 lines of trace.

    `depth` is accepted and ignored - the depth is the call chain above.
    """
    try:
        _dispatch({"key": 90211})
    except IOError as exc:
        raise RuntimeError("connection reset by peer while committing batch") from exc


def jitter(base, spread=0.4):
    """A delay that is not metronomic, so the output looks like real traffic."""
    return max(0.05, random.gauss(base, base * spread))
