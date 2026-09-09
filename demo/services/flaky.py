"""A service that stays up until you ask it not to.

`./demo crash` drops a flag file and this exits 42, so the sidebar shows a
failed service with a real exit code. `./demo revive` brings it back under a
new container ID, which is the reattach path.
"""

import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from common import (  # noqa: E402
    GREEN, GREY, RED, RESET, YELLOW, flag, install_clean_exit, jitter, log,
)

EXIT_CODE = 42


def main():
    install_clean_exit()
    log("INFO", "flaky-sync starting")
    log("INFO", f"{GREEN}ready{RESET} - syncing every ~3s")
    n = 0

    while True:
        if flag("crash"):
            log("ERROR", f"{RED}fatal: replication slot lost, cannot continue{RESET}")
            log("ERROR", f"{GREY}  last good offset: {random.randint(10**7, 10**8)}{RESET}")
            log("FATAL", f"{RED}exiting with code {EXIT_CODE}{RESET}")
            sys.stdout.flush()
            os._exit(EXIT_CODE)

        n += 1
        if n % 5 == 0:
            log("WARN", f"{YELLOW}sync lagging: {random.randint(200, 4000)}ms behind primary{RESET}")
        else:
            log("INFO", f"synced {random.randint(1, 90)} rows {GREY}(batch {n}){RESET}")
        time.sleep(jitter(3.0))


if __name__ == "__main__":
    main()
