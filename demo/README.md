# composemux demo stack

A Compose project built for one purpose: giving
[composemux](..) something worth looking at.

Nine services with deliberately different temperaments — one that never shuts
up, one that draws progress bars, one that exits 42 when you ask it to, one
that says almost nothing — so every column in the service list has something in
it and every feature has something to demonstrate.

Nothing is stateful. `./stack down` leaves no volumes, no images to clean up,
and nothing on disk.

## Requirements

- Docker with Compose v2 or later (tested on Docker 29.7, Compose v5.4)
- `curl` on your PATH
- composemux on your PATH:

```bash
cargo install --path ..
```

Or build it and call the binary directly — composemux asks to be invoked
directly rather than through a task runner that captures child output, and
`cargo run` is close enough to the line to be worth avoiding:

```bash
cargo build --release --manifest-path ../Cargo.toml
```

## Quick start

Two terminals, side by side.

```bash
./stack up
```

```bash
composemux
```

Then, back in the first terminal:

```bash
./stack chaos
```

`chaos` runs continuous traffic and fires a random event every twenty seconds —
job batches, server errors, a health check going bad, a paused service. It is
the hands-off option: start it and talk over it. Everything it does is also
available as a single command, which is what the tour below uses.

## The stack

| Service | Image | What it is there for |
|---|---|---|
| `gateway` | nginx | Plain, uncoloured access logs — the same request the api also logs |
| `api` | node | Colour-coded access logs, a health check you can break, deep tracebacks |
| `worker` | node | Progress bars that redraw in place, 39-line stack traces, a stress mode |
| `metrics` | node | Quiet unicode sparklines — a good second pin |
| `flaky` | node | Runs until you tell it to exit 42 |
| `migrate` | node | Runs once, exits 0, sits in the list as a finished job |
| `db` | postgres | Real third-party output; initdb runs on every `up` |
| `cache` | redis | Says nothing at all, which is the point |
| `reporter` | node | Behind a profile, so it can arrive after composemux has attached |

The services talk through `./state`, a bind mount. The activity script drops
flag files in there and the services notice — which is why `./stack unhealthy`
takes effect without restarting anything.

## The tour

Run composemux in one terminal and these in the other. Each step is one
command and one thing to look at.

### Start with the problem

```bash
docker compose logs -f
```

Nine services interleaved into one stream, the progress bars smearing across
each other. Ctrl-C, then run `composemux`, and the same output arrives as a
service list with `api` and `worker` already pinned — that pairing comes from
[`.composemux.yaml`](.composemux.yaml).

### Two panes, one request

Press `1` on `gateway` and `2` on `api`, then:

```bash
./stack traffic 30 4
```

Every request appears twice — once as an nginx access line, once as the api's
own coloured log. That is the case for two panes in one sentence.

### Output that is not plain lines

Pin `worker` and queue some jobs:

```bash
./stack jobs 8
```

Each job draws a percentage bar that redraws in place with `\r`. Roughly one in
six fails with a 39-line traceback. Both go through a vt100 emulator before
they reach the screen, which is why the bar stays one line instead of smearing
and the trace keeps its shape.

Then hit the api directly:

```bash
./stack errors 3
```

Focus the pane with `tab`, scroll with `k`/`j` or `ctrl+u`/`ctrl+d`, `Home` and
`End` to jump. `enter` gives the pane the whole frame; `esc` puts it back
exactly as it was.

### A scrolled pane stays where you put it

```bash
./stack stress 20
```

The worker floods stdout. Scroll up mid-flood and the view holds its position
rather than drifting — it only moves once the lines you are looking at fall out
of the scrollback buffer, which [`.composemux.yaml`](.composemux.yaml) sets to
2000 rows here.

### Every status in the sidebar

`migrate` is already sitting there as a service that exited cleanly. The other
five:

```bash
./stack unhealthy      # health column turns to fail within ~10s
./stack healthy        # and back

./stack crash          # flaky exits 42 - the exit code shows in the row
./stack revive         # back under a new container ID, reattached automatically

./stack pause          # cache -> paused
./stack unpause

./stack create-reporter   # created but not started
./stack start-reporter    # and now running
```

`create-reporter` is worth doing with composemux already open: the row appears
on its own, without a restart, because a supervisor is watching Docker events
rather than polling a fixed list.

### Replicas

```bash
./stack scale 3
```

Three `worker` rows, each with its own log buffer. They share a job queue, so
watch them race for jobs:

```bash
./stack jobs 12
```

Back to one with `./stack scale 1`.

### Finding things

With nine services and three worker replicas, press `/` and type `wor`.
`enter` confirms the filter, `esc` clears it. `b` hides the service list
entirely; `m` switches the panes between stacked and side-by-side.

Press `c` in a focused pane to copy its contents — it goes out as an OSC 52
escape sequence, so it works over SSH with nothing installed at the far end.

### Not a terminal

```bash
composemux --no-tui | head -40
```

Piped or in CI, the UI is dropped for plain prefixed lines. No escape codes
written into a log file.

### Shutting down

```bash
./stack wind-down
```

Every service exits 0 — including postgres, redis and nginx — so composemux
counts down and closes on its own, which is what lets a wrapper script tear the
stack down after it. Any keypress cancels the countdown.

Now the other half of that behaviour:

```bash
./stack up && ./stack crash && ./stack wind-down
```

One service exited non-zero, so there is no countdown at all. Everything has
stopped and composemux stays open, because the moment after a crash is the
worst possible time for the log viewer to disappear.

### Clean up

```bash
./stack down
```

## Recording the README demo

The demo in the project README is recorded against this stack. Two files here
exist only for that, and the beat sheet they follow is in
[#84](https://github.com/sofired/composemux/issues/84).

`record-drive` fires activity at fixed offsets so takes are reproducible —
composemux is full screen for the whole take, so the events have to come from a
second terminal on a timer rather than from anything visible on camera.

```bash
# terminal A
./stack up && ./record-drive
```

```bash
# terminal B, when A prints "go"
asciinema rec demo.cast --cols 110 --rows 30 -c "composemux -c .composemux.demo.yaml"
```

`.composemux.demo.yaml` differs from `.composemux.yaml` in the way that matters:
nothing is pinned, so a take shows pinning happen rather than opening with it
already done. It also disables auto-exit, so no countdown popup gatecrashes a
take.

Every service's routine log lines are kept under 72 columns, which is what a
pane gets at 110×30. Widen those formats and they will wrap on camera.

## Command reference

```
Lifecycle
  up                     start the stack and wait for it to be healthy
  down                   stop and remove everything
  status                 docker compose ps, including stopped services

Traffic and logs
  traffic [SECS] [RPS]   steady request flow          (default 60s at 4/s)
  burst [N]              N concurrent requests        (default 40)
  jobs [N]               queue worker jobs -> progress bars   (default 6)
  errors [N]             500s with deep stack traces  (default 3)
  slow [N]               slow requests                (default 4)
  stress [SECS]          flood the worker with output (default 15s)

Status transitions
  unhealthy | healthy    flip the api health check
  crash                  flaky exits 42
  revive                 bring flaky back under a new container ID
  pause | unpause [SVC]  pause a service              (default cache)
  scale [N]              worker replicas              (default 3)
  restart [SVC]          restart a service            (default api)
  create-reporter        create a service without starting it
  start-reporter         start it, live, while attached
  wind-down              stop everything cleanly -> auto-exit countdown

Hands-free
  chaos [SECS]           traffic plus a random event every ~20s (default 180s)
```

The gateway is published on `http://localhost:8099` if you would rather drive
it by hand. `/boom` returns 500 with a trace, `/slow` stalls for a second or
two, `/jobs` queues work for the worker, `/404` does what it says.

## Notes on the setup

A few choices here are deliberate, and they are the same choices worth making
in a real project you intend to watch.

**TypeScript with no build step.** Node runs the `.ts` files directly, stripping
the types as it loads them, so there is no `package.json`, no `tsconfig.json`, no
`node_modules` and nothing to install. It also writes to stdout without the
buffering that would make a service look stalled in a log viewer, so there is no
flush to remember and no environment variable to set.

The trade is that stripping is not checking: the types document the code and are
erased at load, and nothing verifies them unless you point a `tsc` at them
yourself.

**No `tty: true` anywhere.** A tty strips the per-write headers the Docker
daemon otherwise puts on each log chunk, and the client then has to scan for a
newline to know where one write ends. For the progress bars here — which emit
`\r` and no newline until the bar completes — that would mean nothing appearing
until the job finished.

**SIGTERM handled explicitly.** Without a handler the process dies on the default
disposition and Docker records exit 143, which is a failure. Handling it and
exiting 0 is what makes `./stack wind-down` produce a clean stack rather than
nine failed services.

The exit itself waits for stdout to drain. Writes to a pipe are asynchronous, and
a bare `process.exit()` truncates whatever has not landed yet — reliably the last
line before the process goes, which is the one saying why it went.

**Short healthcheck intervals.** Five seconds with two retries, rather than the
usual thirty, so a health transition shows up while you are still looking at
it.

**Healthchecks use `127.0.0.1`, not `localhost`.** Busybox `wget` resolves
`localhost` to `::1` first, and these servers bind IPv4 — the healthcheck fails
against a service that is working fine.
