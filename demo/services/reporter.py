"""A service that is not started with the rest of the stack.

Behind a compose profile, so you can create it (NotStarted) and start it
(Running) while composemux is already attached, and watch a new row appear in
the sidebar on its own.
"""

import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from common import CYAN, GREEN, GREY, RESET, install_clean_exit, log, progress  # noqa: E402

REPORTS = ["daily_revenue", "churn_cohorts", "usage_by_tenant", "slo_summary"]


def main():
    install_clean_exit()
    log("INFO", "reporter starting - joined the project late")
    log("INFO", f"{GREEN}ready{RESET} - generating on a 20s cycle")

    while True:
        name = random.choice(REPORTS)
        log("INFO", f"building report {CYAN}{name}{RESET}")
        progress(name, steps=16, delay=0.06)
        log("INFO", f"{name} written {GREY}({random.randint(4, 900)} KiB){RESET}")
        time.sleep(20)


if __name__ == "__main__":
    main()
