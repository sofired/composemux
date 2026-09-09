"""A small HTTP service: colourful access logs and a few endpoints that
misbehave on purpose.

Sits behind the gateway, so one request from the activity script shows up in
two panes at once - which is the thing a two-pane log viewer is for.
"""

import json
import os
import random
import sys
import time
import traceback
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(__file__))
from common import (  # noqa: E402
    CYAN, GREEN, GREY, JOBS, MAGENTA, RED, RESET, YELLOW,
    deep_traceback, flag, install_clean_exit, jitter, log,
)

PORT = 8000
UPSTREAMS = ["db", "cache", "search", "billing"]


def status_colour(code):
    if code >= 500:
        return RED
    if code >= 400:
        return YELLOW
    if code >= 300:
        return CYAN
    return GREEN


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # BaseHTTPRequestHandler writes its own access log to stderr. Silence it and
    # emit our own, so the format is ours and health polling stays out of it.
    def log_message(self, *args):
        pass

    def respond(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def access(self, code, started, note=""):
        ms = (time.time() - started) * 1000
        tint = status_colour(code)
        log(
            "INFO" if code < 400 else ("WARN" if code < 500 else "ERROR"),
            f"{MAGENTA}{self.command:<4}{RESET} {self.path:<17} "
            f"{tint}{code}{RESET} {ms:6.1f}ms{note}",
        )

    def do_POST(self):
        self.do_GET()

    def do_GET(self):
        started = time.time()
        path = self.path.split("?")[0]

        if path == "/health":
            # Not logged: a 5-second healthcheck would bury the real traffic.
            # The flip itself is logged below, which is the part worth seeing.
            if flag("unhealthy"):
                self.respond(503, {"status": "unhealthy"})
            else:
                self.respond(200, {"status": "ok"})
            return

        if path == "/boom":
            try:
                deep_traceback()
            except RuntimeError:
                self.respond(500, {"error": "internal"})
                self.access(500, started)
                log("ERROR", f"{RED}unhandled exception serving {path}{RESET}")
                for line in traceback.format_exc().rstrip().splitlines():
                    log("ERROR", f"{GREY}{line}{RESET}")
            return

        if path == "/slow":
            delay = jitter(1.6)
            time.sleep(delay)
            self.respond(200, {"slept": round(delay, 2)})
            self.access(200, started, f" {YELLOW}slow upstream{RESET}")
            return

        if path == "/jobs":
            job = {
                "id": uuid.uuid4().hex[:6],
                "kind": random.choice(["resize", "index", "export", "reconcile"]),
                "size": random.randint(200, 9000),
            }
            os.makedirs(JOBS, exist_ok=True)
            with open(os.path.join(JOBS, f"{job['id']}.json"), "w") as fh:
                json.dump(job, fh)
            self.respond(202, {"queued": job["id"]})
            self.access(202, started, f" {CYAN}queued {job['kind']}{RESET}")
            return

        if path in ("/404", "/missing"):
            self.respond(404, {"error": "not found"})
            self.access(404, started)
            return

        upstream = random.choice(UPSTREAMS)
        time.sleep(jitter(0.02))
        code = 200 if random.random() > 0.06 else random.choice([400, 404, 409])
        self.respond(code, {"ok": code == 200})
        self.access(code, started, f" {GREY}via {upstream}{RESET}")


def main():
    install_clean_exit()
    log("INFO", f"api starting on port {PORT}")
    log("DEBUG", f"{GREY}python {sys.version.split()[0]}, pid {os.getpid()}{RESET}")
    for dep in ("db", "cache"):
        time.sleep(0.25)
        log("INFO", f"connected to {CYAN}{dep}{RESET}")
    log("INFO", f"{GREEN}ready{RESET} - accepting connections")

    server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    server.daemon_threads = True

    # Watch the health flag so the flip is announced rather than only showing up
    # as a status change in the sidebar.
    import threading

    def watch():
        was = flag("unhealthy")
        while True:
            now = flag("unhealthy")
            if now != was:
                if now:
                    log("ERROR", f"{RED}health check failing: dependency pool exhausted{RESET}")
                else:
                    log("INFO", f"{GREEN}health check recovered{RESET}")
                was = now
            time.sleep(0.5)

    threading.Thread(target=watch, daemon=True).start()
    server.serve_forever()


if __name__ == "__main__":
    main()
