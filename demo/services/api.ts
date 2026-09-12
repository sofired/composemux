/**
 * A small HTTP service: colourful access logs and a few endpoints that
 * misbehave on purpose.
 *
 * Sits behind the gateway, so one request from the activity script shows up in
 * two panes at once - which is the thing a two-pane log viewer is for.
 */

import { randomBytes } from "node:crypto";
import { mkdirSync, renameSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";

import {
  CYAN, GREEN, GREY, JOBS, MAGENTA, RED, RESET, YELLOW,
  choice, deepThrow, flag, formatError, installCleanExit, jitter, log, randint, sleep,
  type Level,
} from "./common.ts";

const PORT = 8000;
const UPSTREAMS = ["db", "cache", "search", "billing"] as const;
const KINDS = ["resize", "index", "export", "reconcile"] as const;

function statusColour(code: number): string {
  if (code >= 500) return RED;
  if (code >= 400) return YELLOW;
  if (code >= 300) return CYAN;
  return GREEN;
}

function levelFor(code: number): Level {
  return code < 400 ? "INFO" : code < 500 ? "WARN" : "ERROR";
}

function respond(res: ServerResponse, code: number, payload: unknown): void {
  const body = JSON.stringify(payload);
  res.writeHead(code, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  });
  res.end(body);
}

function access(method: string, path: string, code: number, started: number, note = ""): void {
  const ms = performance.now() - started;
  const tint = statusColour(code);
  log(
    levelFor(code),
    `${MAGENTA}${method.padEnd(4)}${RESET} ${path.padEnd(17)} ` +
      `${tint}${code}${RESET} ${ms.toFixed(1).padStart(6)}ms${note}`,
  );
}

async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const started = performance.now();
  const method = req.method ?? "GET";
  const full = req.url ?? "/";
  const path = full.split("?")[0]!;

  if (path === "/health") {
    // Not logged: a 5-second healthcheck would bury the real traffic. The flip
    // itself is logged by the watcher below, which is the part worth seeing.
    respond(res, flag("unhealthy") ? 503 : 200, { status: flag("unhealthy") ? "unhealthy" : "ok" });
    return;
  }

  if (path === "/boom") {
    try {
      deepThrow();
    } catch (err) {
      respond(res, 500, { error: "internal" });
      access(method, full, 500, started);
      log("ERROR", `${RED}unhandled exception serving ${path}${RESET}`);
      for (const line of formatError(err)) log("ERROR", `${GREY}${line}${RESET}`);
    }
    return;
  }

  if (path === "/slow") {
    const delay = jitter(1.6);
    await sleep(delay);
    respond(res, 200, { slept: Number(delay.toFixed(2)) });
    access(method, full, 200, started, ` ${YELLOW}slow upstream${RESET}`);
    return;
  }

  if (path === "/jobs") {
    const job = {
      id: randomBytes(3).toString("hex"),
      kind: choice(KINDS),
      size: randint(200, 9000),
    };
    mkdirSync(JOBS, { recursive: true });
    // Written beside the real name and moved into place, because the worker
    // claims a job by renaming it and parses it straight away. `writeFileSync`
    // creates the name before it writes the bytes, so a worker that listed the
    // directory in that gap would claim a file that is still empty and die on
    // the parse - `takeJob` does not guard it. The window is tiny for a 46-byte
    // job and we could not hit it on purpose, but rename is atomic and closing
    // it costs a line. `.partial` is skipped by the worker's `.json` filter.
    const published = `${JOBS}/${job.id}.json`;
    const staged = `${published}.partial`;
    writeFileSync(staged, JSON.stringify(job));
    renameSync(staged, published);
    respond(res, 202, { queued: job.id });
    access(method, full, 202, started, ` ${CYAN}queued ${job.kind}${RESET}`);
    return;
  }

  if (path === "/404" || path === "/missing") {
    respond(res, 404, { error: "not found" });
    access(method, full, 404, started);
    return;
  }

  const upstream = choice(UPSTREAMS);
  await sleep(jitter(0.02));
  const code = Math.random() > 0.06 ? 200 : choice([400, 404, 409]);
  respond(res, code, { ok: code === 200 });
  access(method, full, code, started, ` ${GREY}via ${upstream}${RESET}`);
}

async function main(): Promise<void> {
  log("INFO", `api starting on port ${PORT}`);
  log("DEBUG", `${GREY}node ${process.versions.node}, pid ${process.pid}${RESET}`);
  for (const dep of ["db", "cache"]) {
    await sleep(0.25);
    log("INFO", `connected to ${CYAN}${dep}${RESET}`);
  }
  log("INFO", `${GREEN}ready${RESET} - accepting connections`);

  const server = createServer((req, res) => {
    void handle(req, res).catch(() => {
      if (!res.headersSent) respond(res, 500, { error: "internal" });
    });
  });
  installCleanExit(() => server.close());
  server.listen(PORT, "0.0.0.0");

  // Watch the health flag so the flip is announced rather than only showing up
  // as a status change in the sidebar.
  let was = flag("unhealthy");
  setInterval(() => {
    const now = flag("unhealthy");
    if (now === was) return;
    if (now) log("ERROR", `${RED}health check failing: dependency pool exhausted${RESET}`);
    else log("INFO", `${GREEN}health check recovered${RESET}`);
    was = now;
  }, 500);
}

await main();
