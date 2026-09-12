/**
 * A service that is not started with the rest of the stack.
 *
 * Behind a compose profile, so you can create it (NotStarted) and start it
 * (Running) while composemux is already attached, and watch a new row appear in
 * the sidebar on its own.
 */

import { CYAN, GREEN, GREY, RESET, choice, installCleanExit, log, progress, randint, sleep } from "./common.ts";

const REPORTS = ["daily_revenue", "churn_cohorts", "usage_by_tenant", "slo_summary"] as const;

async function main(): Promise<void> {
  installCleanExit();
  log("INFO", "reporter starting - joined the project late");
  log("INFO", `${GREEN}ready${RESET} - generating on a 20s cycle`);

  for (;;) {
    const name = choice(REPORTS);
    log("INFO", `building report ${CYAN}${name}${RESET}`);
    await progress(name, 16, 0.06);
    log("INFO", `${name} written ${GREY}(${randint(4, 900)} KiB)${RESET}`);
    await sleep(20);
  }
}

await main();
