/**
 * A one-shot migration job. Runs, succeeds, exits 0.
 *
 * There to occupy the 'exited cleanly' row in the sidebar - the green tick with
 * a duration and no exit code, next to whatever else is still running.
 */

import { CYAN, GREEN, GREY, RESET, exitAfterFlush, installCleanExit, log, progress, sleep } from "./common.ts";

const MIGRATIONS = [
  "0001_create_accounts",
  "0002_add_billing_columns",
  "0003_backfill_regions",
  "0004_index_orders_created_at",
  "0005_drop_legacy_sessions",
];

async function main(): Promise<void> {
  installCleanExit();
  log("INFO", "migrate starting");
  log("INFO", `${GREY}target: postgres://db:5432/demo${RESET}`);
  await sleep(0.6);

  for (const name of MIGRATIONS) {
    log("INFO", `applying ${CYAN}${name}${RESET}`);
    await progress(name, 12, 0.05);
  }

  log("INFO", `${GREEN}5 migrations applied, schema at revision 0005${RESET}`);
  log("INFO", "migrate finished, exiting 0");
  // A registered signal listener keeps the event loop alive, so returning from
  // main is not enough to end the process here.
  exitAfterFlush(0);
}

await main();
