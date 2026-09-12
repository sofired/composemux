/**
 * A steady low-volume heartbeat with unicode sparklines.
 *
 * Quiet enough to leave pinned in a second pane while you work on something
 * else, and a check that block-drawing characters measure correctly in a pane.
 */

import { CYAN, GREEN, GREY, RED, RESET, YELLOW, gauss, installCleanExit, log, sleep } from "./common.ts";

const BLOCKS = "▁▂▃▄▅▆▇█";

function spark(values: readonly number[]): string {
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const span = hi - lo || 1;
  return values
    .map((v) => BLOCKS[Math.floor(((v - lo) / span) * (BLOCKS.length - 1))])
    .join("");
}

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

async function main(): Promise<void> {
  installCleanExit();
  log("INFO", "metrics collector starting");
  log("INFO", `${GREEN}ready${RESET} - scrape interval 2s`);

  let cpu = Array.from({ length: 10 }, () => 8 + Math.random() * 32);
  let rps = Array.from({ length: 10 }, () => 20 + Math.random() * 70);

  for (;;) {
    cpu = [...cpu.slice(1), clamp(cpu[cpu.length - 1]! + gauss(0, 9), 1, 99)];
    rps = [...rps.slice(1), Math.max(1, rps[rps.length - 1]! + gauss(0, 14))];
    const mem = 280 + Math.random() * 440;
    const now = cpu[cpu.length - 1]!;

    const tint = now > 80 ? RED : now > 55 ? YELLOW : GREEN;
    log(
      "INFO",
      `cpu ${tint}${now.toFixed(1).padStart(5)}%${RESET} ${GREY}${spark(cpu)}${RESET} ` +
        `rps ${CYAN}${rps[rps.length - 1]!.toFixed(1).padStart(5)}${RESET} ${GREY}${spark(rps)}${RESET} ` +
        `mem ${String(Math.round(mem)).padStart(3)}M`,
    );
    if (now > 85) log("WARN", `${YELLOW}cpu saturation - scheduler queue depth rising${RESET}`);
    await sleep(2);
  }
}

await main();
