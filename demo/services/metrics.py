"""A steady low-volume heartbeat with unicode sparklines.

Quiet enough to leave pinned in a second pane while you work on something else,
and a check that block-drawing characters measure correctly in the pane.
"""

import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from common import CYAN, GREEN, GREY, RED, RESET, YELLOW, install_clean_exit, log  # noqa: E402

BLOCKS = "▁▂▃▄▅▆▇█"


def spark(values):
    lo, hi = min(values), max(values)
    span = (hi - lo) or 1
    return "".join(BLOCKS[int((v - lo) / span * (len(BLOCKS) - 1))] for v in values)


def main():
    install_clean_exit()
    log("INFO", "metrics collector starting")
    log("INFO", f"{GREEN}ready{RESET} - scrape interval 2s")

    cpu, rps = [random.uniform(8, 40) for _ in range(10)], [random.uniform(20, 90) for _ in range(10)]
    while True:
        cpu = cpu[1:] + [max(1, min(99, cpu[-1] + random.gauss(0, 9)))]
        rps = rps[1:] + [max(1, rps[-1] + random.gauss(0, 14))]
        mem = random.uniform(280, 720)

        tint = RED if cpu[-1] > 80 else (YELLOW if cpu[-1] > 55 else GREEN)
        log(
            "INFO",
            f"cpu {tint}{cpu[-1]:5.1f}%{RESET} {GREY}{spark(cpu)}{RESET} "
            f"rps {CYAN}{rps[-1]:5.1f}{RESET} {GREY}{spark(rps)}{RESET} "
            f"mem {mem:3.0f}M",
        )
        if cpu[-1] > 85:
            log("WARN", f"{YELLOW}cpu saturation - scheduler queue depth rising{RESET}")
        time.sleep(2)


if __name__ == "__main__":
    main()
