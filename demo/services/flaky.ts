/**
 * A service that stays up until you ask it not to.
 *
 * `./stack crash` drops a flag file and this exits 42, so the sidebar shows a
 * failed service with a real exit code. `./stack revive` brings it back under a
 * new container ID, which is the reattach path.
 */

import {
  GREEN, GREY, RED, RESET, YELLOW,
  exitAfterFlush, flag, installCleanExit, jitter, log, randint, sleep,
} from "./common.ts";

const EXIT_CODE = 42;

async function main(): Promise<void> {
  installCleanExit();
  log("INFO", "flaky-sync starting");
  log("INFO", `${GREEN}ready${RESET} - syncing every ~3s`);
  let n = 0;

  for (;;) {
    if (flag("crash")) {
      log("ERROR", `${RED}fatal: replication slot lost, cannot continue${RESET}`);
      log("ERROR", `${GREY}  last good offset: ${randint(10 ** 7, 10 ** 8)}${RESET}`);
      log("FATAL", `${RED}exiting with code ${EXIT_CODE}${RESET}`);
      exitAfterFlush(EXIT_CODE);
      return;
    }

    n += 1;
    if (n % 5 === 0) {
      log("WARN", `${YELLOW}sync lagging: ${randint(200, 4000)}ms behind primary${RESET}`);
    } else {
      log("INFO", `synced ${randint(1, 90)} rows ${GREY}(batch ${n})${RESET}`);
    }
    await sleep(jitter(3.0));
  }
}

await main();
