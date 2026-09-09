"""A one-shot migration job. Runs, succeeds, exits 0.

There to occupy the 'exited cleanly' row in the sidebar - the green tick with a
duration and no exit code, next to whatever else is still running.
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from common import CYAN, GREEN, GREY, RESET, install_clean_exit, log, progress  # noqa: E402

MIGRATIONS = [
    "0001_create_accounts",
    "0002_add_billing_columns",
    "0003_backfill_regions",
    "0004_index_orders_created_at",
    "0005_drop_legacy_sessions",
]


def main():
    install_clean_exit()
    log("INFO", "migrate starting")
    log("INFO", f"{GREY}target: postgres://db:5432/demo{RESET}")
    time.sleep(0.6)

    for name in MIGRATIONS:
        log("INFO", f"applying {CYAN}{name}{RESET}")
        progress(name, steps=12, delay=0.05)

    log("INFO", f"{GREEN}5 migrations applied, schema at revision 0005{RESET}")
    log("INFO", "migrate finished, exiting 0")
    sys.exit(0)


if __name__ == "__main__":
    main()
