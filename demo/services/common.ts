/**
 * Shared helpers for the demo services.
 *
 * Everything here exists to make container output interesting to look at:
 * colour, timestamps, progress bars that redraw in place, and stack traces with
 * enough frames to need scrolling.
 *
 * Node's standard library only, and no build step: Node runs these .ts files
 * directly by stripping the types, so there is no package.json, no tsconfig and
 * nothing to install.
 */

import { existsSync } from "node:fs";

export const RESET = "\x1b[0m";
export const BOLD = "\x1b[1m";
export const DIM = "\x1b[2m";

export const RED = "\x1b[31m";
export const GREEN = "\x1b[32m";
export const YELLOW = "\x1b[33m";
export const BLUE = "\x1b[34m";
export const MAGENTA = "\x1b[35m";
export const CYAN = "\x1b[36m";
export const GREY = "\x1b[90m";

export type Level = "DEBUG" | "INFO" | "WARN" | "ERROR" | "FATAL";

const LEVEL_COLOUR: Record<Level, string> = {
  DEBUG: GREY,
  INFO: GREEN,
  WARN: YELLOW,
  ERROR: RED,
  FATAL: BOLD + RED,
};

export const STATE = "/state";
export const FLAGS = `${STATE}/flags`;
export const JOBS = `${STATE}/jobs`;

const pad = (n: number, w = 2): string => String(n).padStart(w, "0");

/** Timestamp with milliseconds - the kind of prefix real services emit. */
export function ts(): string {
  const d = new Date();
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/**
 * One log line: dim timestamp, coloured level, message.
 *
 * Node writes to stdout without the buffering that would make a service look
 * stalled in a log viewer, so unlike the Python this replaced, there is no
 * flush to remember and no environment variable to set.
 */
export function log(level: Level, message: string, colour?: string): void {
  const tint = colour ?? LEVEL_COLOUR[level];
  process.stdout.write(`${DIM}${ts()}${RESET} ${tint}${level.padEnd(5)}${RESET} ${message}\n`);
}

/** Write without a newline - for progress bars that redraw with \r. */
export function raw(text: string): void {
  process.stdout.write(text);
}

export const sleep = (seconds: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, seconds * 1000));

/**
 * A percentage bar that redraws in place using carriage returns.
 *
 * composemux runs container output through a vt100 emulator, so this renders as
 * a single updating line rather than smearing across the pane.
 */
export async function progress(label: string, steps = 24, delay = 0.06, width = 22): Promise<void> {
  for (let step = 0; step <= steps; step++) {
    const pct = step / steps;
    const filled = Math.floor(pct * width);
    const bar = "#".repeat(filled) + "-".repeat(width - filled);
    const shown = String(Math.floor(pct * 100)).padStart(3);
    raw(`\r${DIM}${ts()}${RESET} ${CYAN}..... ${label} ${RESET}[${bar}] ${shown}%`);
    await sleep(delay);
  }
  raw("\n");
}

/** True while the activity script is holding a flag file on us. */
export function flag(name: string): boolean {
  return existsSync(`${FLAGS}/${name}`);
}

/**
 * Exit 0 on SIGTERM so `compose stop` reads as a clean shutdown.
 *
 * Without this the process dies on the default disposition and Docker records
 * exit 143 - which composemux correctly shows as a failure, and which would
 * keep the auto-exit countdown from ever running.
 */
export function installCleanExit(onStop?: () => void): void {
  const bye = (name: string): void => {
    log("INFO", `${name} received, shutting down cleanly`, CYAN);
    onStop?.();
    log("INFO", "bye", CYAN);
    exitAfterFlush(0);
  };
  process.on("SIGTERM", () => bye("SIGTERM"));
  process.on("SIGINT", () => bye("SIGINT"));
}

/**
 * Exit once stdout has drained.
 *
 * stdout to a pipe is asynchronous, and a bare process.exit() truncates
 * whatever has not reached the pipe yet - which here is reliably the last line
 * before the process goes away, the one saying why it went.
 */
export function exitAfterFlush(code: number): void {
  process.stdout.write("", () => process.exit(code));
}

/** A delay that is not metronomic, so the output looks like real traffic. */
export function jitter(base: number, spread = 0.4): number {
  return Math.max(0.05, gauss(base, base * spread));
}

/** Box-Muller. The standard library has no gaussian, and uniform noise reads wrong. */
export function gauss(mean: number, sd: number): number {
  const u = 1 - Math.random();
  const v = Math.random();
  return mean + sd * Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
}

export const randint = (a: number, b: number): number => a + Math.floor(Math.random() * (b - a + 1));

export const choice = <T,>(xs: readonly T[]): T => xs[Math.floor(Math.random() * xs.length)]!;

// Default is ten frames, which would cut the chain below off halfway.
Error.stackTraceLimit = 60;

// A chain of distinct frames. A recursive call would be shorter to write and
// would give every frame the same name, which reads as a bug in the demo rather
// than a stack worth scrolling.

function socketSend(_payload: Buffer): never {
  throw new Error("ECONNRESET: Connection reset by peer");
}
function streamFlush(_conn: object, payload: Buffer): never {
  return socketSend(payload);
}
function wireWrite(conn: object, frame: string): never {
  return streamFlush(conn, Buffer.from(frame));
}
function pgExecute(conn: object, sql: string): never {
  return wireWrite(conn, sql);
}
function poolAcquireAndRun(sql: string): never {
  return pgExecute({}, sql);
}
function sessionExecute(sql: string): never {
  return poolAcquireAndRun(sql);
}
function repositoryLoad(entity: string, key: number): never {
  return sessionExecute(`SELECT * FROM ${entity} WHERE id = ${key}`);
}
function unitOfWork(entity: string, key: number): never {
  return repositoryLoad(entity, key);
}
function serviceLayer(key: number): never {
  try {
    return unitOfWork("orders", key);
  } catch (cause) {
    throw new Error(`lost connection loading order ${key}`, { cause });
  }
}
function retryWrapper(key: number): never {
  return serviceLayer(key);
}
function commandHandler(key: number): never {
  return retryWrapper(key);
}
function dispatch(command: { key: number }): never {
  return commandHandler(command.key);
}

/** Throw an Error with a two-level cause chain, deep enough to need scrolling. */
export function deepThrow(): never {
  try {
    dispatch({ key: 90211 });
  } catch (cause) {
    throw new Error("connection reset by peer while committing batch", { cause });
  }
}

/**
 * Render an error and its causes as lines, the way a log viewer will show them.
 *
 * Node keeps a cause on the error rather than splicing it into one string as
 * Python does, so the chain has to be walked by hand.
 */
export function formatError(err: unknown): string[] {
  const lines: string[] = [];
  let current: unknown = err;
  while (current instanceof Error) {
    if (lines.length) lines.push("Caused by:");
    lines.push(...(current.stack ?? String(current)).split("\n"));
    current = current.cause;
  }
  return lines;
}

