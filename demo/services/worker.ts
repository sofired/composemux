/**
 * Background job worker: progress bars, deep stack traces, and a stress mode.
 *
 * This is the service to pin when you want to see composemux handle output that
 * is not plain lines - carriage returns redrawing a bar in place, and stack
 * traces tall enough to scroll.
 */

import { readdirSync, readFileSync, renameSync, unlinkSync } from "node:fs";
import { hostname } from "node:os";

import {
  CYAN, GREEN, GREY, JOBS, MAGENTA, RED, RESET, YELLOW,
  choice, deepThrow, flag, formatError, installCleanExit, jitter, log, progress, randint, sleep,
  type Level,
} from "./common.ts";

type Job = { id: string; kind: string; size: number };

const IDLE_CHATTER: ReadonlyArray<readonly [Level, string]> = [
  ["DEBUG", "poll: queue empty, backing off"],
  ["DEBUG", "heartbeat sent to coordinator"],
  ["INFO", "lease renewed for partition {p}"],
  ["DEBUG", "compacted {n} tombstones"],
];

/** Claim the oldest job file, or null. Renaming is the claim. */
function takeJob(): Job | null {
  let names: string[];
  try {
    names = readdirSync(JOBS).filter((n) => n.endsWith(".json")).sort();
  } catch {
    return null;
  }
  for (const name of names) {
    const path = `${JOBS}/${name}`;
    const claimed = `${path}.taken`;
    try {
      renameSync(path, claimed);
    } catch {
      continue; // another replica got there first
    }
    try {
      return JSON.parse(readFileSync(claimed, "utf8")) as Job;
    } finally {
      unlinkSync(claimed);
    }
  }
  return null;
}

async function runJob(job: Job): Promise<void> {
  const { kind, id, size } = job;
  log("INFO", `${MAGENTA}job ${id}${RESET} start kind=${CYAN}${kind}${RESET} size=${size}`);

  const steps = Math.max(10, Math.min(40, Math.floor(size / 200)));
  await progress(`job ${id} ${kind}`, steps, 0.08);

  // Roughly one job in six falls over, which is what makes the pane worth
  // scrolling back through.
  if (Math.random() < 0.17) {
    try {
      deepThrow();
    } catch (err) {
      log("ERROR", `${RED}job ${id} failed after ${steps} steps${RESET}`);
      for (const line of formatError(err)) log("ERROR", `${GREY}${line}${RESET}`);
      log("WARN", `job ${id} scheduled for retry in 30s`);
      return;
    }
  }

  log("INFO", `${MAGENTA}job ${id}${RESET} ${GREEN}done${RESET} in ${(steps * 0.05).toFixed(1)}s`);
}

/** High-volume output, for watching a scrolled-up pane hold its position. */
async function stressBurst(): Promise<void> {
  log("WARN", `${YELLOW}stress mode engaged - flooding stdout${RESET}`);
  let n = 0;
  while (flag("stress")) {
    n += 1;
    log(
      "DEBUG",
      `${GREY}batch=${String(n).padStart(6, "0")} shard=${n % 8} rows=${randint(50, 900)} ` +
        `lag=${randint(0, 400)}ms offset=${randint(10 ** 6, 10 ** 7)}${RESET}`,
    );
    await sleep(0.01);
  }
  log("INFO", `${GREEN}stress mode released after ${n} lines${RESET}`);
}

async function main(): Promise<void> {
  installCleanExit();
  log("INFO", `worker starting (container ${hostname()})`);
  log("INFO", `${GREEN}ready${RESET} - polling ${JOBS}`);
  let tick = 0;

  for (;;) {
    if (flag("stress")) {
      await stressBurst();
      continue;
    }

    const job = takeJob();
    if (job) {
      await runJob(job);
      continue;
    }

    tick += 1;
    if (tick % 4 === 0) {
      const [level, text] = choice(IDLE_CHATTER);
      log(level, text.replace("{p}", String(randint(0, 7))).replace("{n}", String(randint(1, 400))));
    }
    await sleep(jitter(0.8));
  }
}

await main();
