"""Background job worker: progress bars, deep stack traces, and a stress mode.

This is the service to pin when you want to see composemux handle output that
is not plain lines - carriage returns redrawing a bar in place, and tracebacks
tall enough to scroll.
"""

import json
import os
import random
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(__file__))
from common import (  # noqa: E402
    CYAN, GREEN, GREY, JOBS, MAGENTA, RED, RESET, YELLOW,
    deep_traceback, flag, install_clean_exit, jitter, log, progress,
)

IDLE_CHATTER = [
    ("DEBUG", "poll: queue empty, backing off"),
    ("DEBUG", "heartbeat sent to coordinator"),
    ("INFO", "lease renewed for partition {p}"),
    ("DEBUG", "compacted {n} tombstones"),
]


def take_job():
    """Claim the oldest job file, or None. Renaming is the claim."""
    try:
        names = sorted(n for n in os.listdir(JOBS) if n.endswith(".json"))
    except FileNotFoundError:
        return None
    for name in names:
        path = os.path.join(JOBS, name)
        claimed = path + ".taken"
        try:
            os.rename(path, claimed)
        except OSError:
            continue  # another replica got there first
        try:
            with open(claimed) as fh:
                return json.load(fh)
        finally:
            os.unlink(claimed)
    return None


def run_job(job):
    kind, jid, size = job["kind"], job["id"], job["size"]
    log("INFO", f"{MAGENTA}job {jid}{RESET} start kind={CYAN}{kind}{RESET} size={size}")

    steps = max(10, min(40, size // 200))
    progress(f"job {jid} {kind}", steps=steps, delay=0.08)

    # Roughly one job in six falls over, which is what makes the pane worth
    # scrolling back through.
    if random.random() < 0.17:
        try:
            deep_traceback()
        except RuntimeError:
            log("ERROR", f"{RED}job {jid} failed after {steps} steps{RESET}")
            for line in traceback.format_exc().rstrip().splitlines():
                log("ERROR", f"{GREY}{line}{RESET}")
            log("WARN", f"job {jid} scheduled for retry in 30s")
            return

    log("INFO", f"{MAGENTA}job {jid}{RESET} {GREEN}done{RESET} in {steps * 0.05:.1f}s")


def stress_burst():
    """High-volume output, for watching a scrolled-up pane hold its position."""
    log("WARN", f"{YELLOW}stress mode engaged - flooding stdout{RESET}")
    n = 0
    while flag("stress"):
        n += 1
        shard = n % 8
        log(
            "DEBUG",
            f"{GREY}batch={n:06d} shard={shard} rows={random.randint(50, 900)} "
            f"lag={random.randint(0, 400)}ms offset={random.randint(10**6, 10**7)}{RESET}",
        )
        time.sleep(0.01)
    log("INFO", f"{GREEN}stress mode released after {n} lines{RESET}")


def main():
    install_clean_exit()
    replica = os.environ.get("REPLICA", "?")
    log("INFO", f"worker starting (container {os.uname().nodename})")
    log("INFO", f"{GREEN}ready{RESET} - polling {JOBS}")
    tick = 0

    while True:
        if flag("stress"):
            stress_burst()
            continue

        job = take_job()
        if job:
            run_job(job)
            continue

        tick += 1
        if tick % 4 == 0:
            level, text = random.choice(IDLE_CHATTER)
            log(level, text.format(p=random.randint(0, 7), n=random.randint(1, 400)))
        time.sleep(jitter(0.8))


if __name__ == "__main__":
    main()
