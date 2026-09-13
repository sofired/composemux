# composemux

Six services, one interleaved stream, and the stack trace you actually needed
has already scrolled off the top. `docker compose logs -f` shows you
everything at once and gives you no way to say "keep the API in front of me
while I poke at the others."

composemux does that one thing: your services down the left with live status,
and up to two log panes you can pin open.

It's read-only, deliberately. It attaches to containers something else already
started and never starts, stops, restarts or execs into anything — so it's safe
to drop into the middle of a script that owns `compose up` and `compose down`.

<p align="center">
  <img src="demo/demo.gif" width="800"
       alt="composemux with a nine-service demo stack: the service list on the left, gateway and api pinned open on the right, a job's progress bar, a stack trace, and flaky exiting 42 and turning its row red">
</p>

<p align="center"><sub>Recorded against the <a href="demo/">demo stack</a> in this repository; <code>demo/record</code> reproduces it.</sub></p>

## What it does

- **A service list with live status** — running, exited clean, exited non-zero,
  unhealthy, paused — with health and uptime columns.
- **Pin one or two services** so they stay on screen while you browse the rest.
- **Real container state**, read from the Docker Engine API rather than scraped
  out of `docker compose logs`: actual statuses, actual exit codes, and a
  reattach when a container restarts under a new ID.
- **Output that renders properly.** ANSI colour, `\r` progress bars and
  forty-line Java stack traces all go through a vt100 emulator first.
- **Copy that survives SSH** — `c` copies the pane via OSC 52.
- **Sensible behaviour when nobody's watching.** Piped or in CI, it drops the
  UI and streams plain prefixed lines instead of a screenful of escape codes.

## Install

```sh
cargo install composemux
```

Prebuilt binaries are attached to each
[release](https://github.com/sofired/composemux/releases) for Linux (x86-64 gnu
and musl, arm64), macOS (Apple Silicon) and Windows (x86-64). On Intel Macs,
build from source — GitHub retired its Intel macOS runners, so there's no
longer a machine to build that binary on.

## Use

Run it in a directory with a running Compose project:

```sh
composemux
```

Or name the project, which is what you want when a script is doing the
launching:

```sh
composemux --project my-stack --pin api --pin worker
```

### Options

| Flag | Meaning |
|---|---|
| `-p, --project <NAME>` | Compose project to attach to. Defaults to `$COMPOSE_PROJECT_NAME`, else the directory name. |
| `-c, --config <PATH>` | Config file. Defaults to the nearest `.composemux.yaml`. |
| `--pin <SERVICE>` | Pin a service to an output pane at startup. Repeatable, max two. |
| `--tail <N>` | Lines of history to load per service before following. |
| `--scrollback <N>` | Rows of output retained per service (default 1000, ~7 MB each). |
| `--no-tui` | Stream plain prefixed lines instead of the full-screen UI. |

## Keys

Press `?` and it'll tell you all of this. But for the skimmers:

**In the service list**

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Move the selection |
| `1` / `2` | Pin the selected service to output pane 1 or 2 |
| `0` | Close every pane |
| `space` | Open a single pane that follows the selection |
| `enter` | Open the selected service's pane and focus it |
| `tab` / `shift+tab` | Move focus between the list and the panes |
| `b` | Hide or show the service list |
| `m` | Switch between stacked and side-by-side layouts |
| `/` | Filter services; `enter` confirms, `esc` clears |

Pinning is idempotent in the way you'd hope: pressing `1` or `2` on a service
that's already in that pane unpins it, and pinning something sitting in the
*other* pane moves it across rather than opening a second copy of the same
logs.

Full screen is a stronger version of `b`: hiding the list still leaves two
pinned panes splitting the frame, whereas `enter` on a focused pane gives it
everything. It's modal while it lasts — the keys that would put the list or the
other pane back are ignored, and `enter` doesn't toggle back out — and `esc`
restores the arrangement exactly as it was, hidden list included, without also
giving up the pane. Press `esc` a second time for the service list — unless you
had hidden it with `b`, in which case it stays hidden and focus stays where it
is.

**In an output pane**

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Scroll — accelerates while held |
| `ctrl+u` / `ctrl+d` | Scroll half a page |
| `Home` / `End` | Jump to the start or end |
| `c` | Copy the buffer to the clipboard |
| `enter` | Full screen: this pane takes the frame, list and all |
| `esc` | Leave full screen if it's on, otherwise back to the service list |
| `esc` (again) | After full screen: back to the service list, if it's showing |

**Anywhere:** `?` help · `q` quit · `ctrl+c` interrupt · `F10` toggle mouse
capture.

Scroll up to read something and it stays where you put it as new output
arrives, rather than drifting off the top. It only moves once the lines you are
looking at fall out of the buffer entirely — `scrollback`, 1000 rows by default.
Raise it if you need to hold a position for longer; it costs roughly 7 MB per
service per 1000 rows.

Copying sends an OSC 52 escape sequence, handing the buffer straight to your
terminal emulator — so it works when composemux is running on a remote box over
SSH, with nothing installed at that end. Default builds then also make a
best-effort write to the native clipboard, for terminals that ignore OSC 52.
That second path is what `--no-default-features` drops.

## Configuration

Entirely optional. Drop a `.composemux.yaml` next to your compose file:

```yaml
project: my-stack       # usually passed as --project instead
include: [api, worker]  # empty means every service
exclude: [migrate]
pinned:  [api, db]      # pane 1 and pane 2 at startup
tail: 200               # lines of history per service
scrollback: 1000        # rows retained per service (~7 MB each)
auto_exit: 3            # seconds to wait once every service has exited; false disables
```

| Key | Meaning |
|---|---|
| `project` | Compose project name. Usually passed as `--project` instead. |
| `include` | Services to show. Empty means all of them. |
| `exclude` | Services to hide, applied after `include`. |
| `pinned` | Up to two services, opened in panes 1 and 2 at startup. |
| `tail` | Lines of history loaded per service before following. |
| `scrollback` | Rows of output retained per service. |
| `auto_exit` | Seconds to wait once every service has exited cleanly, or `false` to disable. |

Without `--config`, composemux looks for `.composemux.yaml` in the working
directory and each parent above it, then falls back to a user config file
(`$XDG_CONFIG_HOME/composemux/config.yaml`, or `~/.config` on Linux and macOS,
or `%APPDATA%` on Windows). A missing file isn't an error — it just runs with
defaults.

Unknown keys are rejected rather than ignored, so a typo gets you a loud error
instead of a pin that quietly never happens.

## Driving it from a script

composemux is built to sit inside a wrapper that owns the Compose lifecycle:
bring the project up, block on the TUI, tear down when it exits. The parts of
its behaviour that matter to that caller:

- **Exit codes.** `0` when the user quits with `q` or the stack exits on its
  own, `130` on `ctrl+c`, non-zero on error — so the wrapper can tell a
  deliberate quit from an interrupt.
- **Terminal restoration** runs on every exit path, panics and
  `SIGTERM`/`SIGHUP` included. Your script never inherits a terminal stuck in
  raw mode.
- **Non-TTY output** falls back to plain prefixed lines automatically, so a
  piped run doesn't write escape sequences into a log file.
- **Auto-exit.** Once every service has exited *cleanly*, a countdown appears
  and composemux closes so the wrapper can clean up. Any keypress cancels it.
  If any service exited non-zero the countdown doesn't run at all — "everything
  exited" usually means the stack fell over, and the moment after a crash is
  the worst possible time for your log viewer to helpfully disappear and let a
  script tear down the evidence.

One caveat: invoke the binary directly rather than through a task runner that
captures child output. A TUI nested inside another TUI renders neither.

## How it works

Logs come from the Docker Engine API, not from parsing `docker compose logs`
output. That's what buys the per-service streams, the real container statuses
and the exit codes. A supervisor watches Docker events, so a container that
restarts — and therefore gets a new ID — is picked back up, and services
created after startup show up on their own.

If a container's log stream drops while the container keeps running (a daemon
restart, say), reconnecting resumes from a one-second boundary, because that's
the finest resolution the Engine API offers here. A couple of already-visible
lines can reappear as a result. That's the deliberate trade: a duplicated line
beats a missing one.

If the daemon stops answering, composemux keeps retrying rather than exiting,
and the status bar says so, so a frozen screen can't be mistaken for a quiet
stack. Which note you get depends on how it's failing, because the three send
you to different places:

- `Docker daemon unreachable - retrying` — nothing was reached. Start Docker.
- `Docker daemon not answering - retrying` — the request was taken and never
  came back. Docker is running; it's wedged, and restarting composemux won't
  help.
- `Docker daemon rejected the request - retrying` — the request was refused
  rather than lost: Docker answered with an error of its own, or the socket
  wouldn't let composemux open it. Starting Docker is the one remedy this
  rules out. Run with `COMPOSEMUX_DEBUG=1` and the actual error is written to
  `composemux.log` in your temp directory.

  Being unable to open the socket usually stops you before this, at startup,
  with the error printed rather than a note in the bar — this is the note for
  a daemon that starts refusing while composemux is already running.

Statuses and logs stay at their last known values until it answers again, at
which point the note clears on its own. It takes a short run of failed polls to
appear, so a single dropped request never flashes it.

Startup has no status bar to write into, so a daemon that takes the connection
and goes quiet used to leave you with a blank terminal. After five seconds it
says on stderr that it's still waiting, and which `DOCKER_HOST` it's waiting
on, repeating every thirty seconds. It keeps waiting either way: a daemon
that's still coming up is a normal thing for a wrapper script to race.

Everything then goes through a `vt100` terminal emulator before it reaches the
screen, which is why colour, cursor movement and progress bars behave rather
than smearing themselves across the UI. It's also a safety property — container
logs are untrusted input, and they're never passed through to your terminal
verbatim.

## Known limitations

Two ways a Compose project can surprise composemux, both worth knowing before
you go looking for the bug in your own service.

**A `tty: true` service that writes without newlines will look stalled.** Set
`tty: false` and it streams as it should. That's the whole remedy — with the
caveat that the service then sees a pipe rather than a terminal, so anything
that checks for one — a `\r` progress bar redrawing in place, say — will render
differently. `tty: true` isn't Compose's default, so this only bites a service
that asked for one.

What's going on: the daemon splits a log message at 16 KiB whether or not the
service has a tty, and what a tty removes is the per-write header saying where
each split falls. The Docker client composemux reads logs through (bollard)
then has nothing to frame on, scans for a newline instead, and holds the
daemon's chunks until one arrives. Measured: a service writing 48 KiB with no
newline in it delivers three 16 KiB pieces over eleven seconds without a tty;
with one, nothing at all for those eleven seconds and then all three at once.
Output that never contains a newline is held, in memory, until the stream ends.
The framing is settled inside the Docker client before composemux sees a byte,
so there's nothing to fix at this layer;
[#38](https://github.com/sofired/composemux/issues/38) tracks the upstream
change. Ordinary line-oriented output isn't affected: the newline ending each
line releases it, so nothing accumulates.

**Replicas are told apart by a Compose label.** composemux reads
`com.docker.compose.container-number` to decide which replica of a scaled
service a container is, and reads a container without that label as replica 1.
Compose v5.5.0 sets it on every container composemux follows — unscaled
services and `container_name:` overrides alike — so it's unlikely to be a
limitation you meet. A Compose version that omitted it on a scaled service
would give every replica the same identity: a row each in the sidebar for as
long as they're running, all carrying the same name, and one log buffer behind
them all, holding their output interleaved with no way to separate them.
There's nothing to configure at this end, and nothing to fix for an unscaled
service — only a scaled one needs a Compose that sets the label.
[#51](https://github.com/sofired/composemux/issues/51) has the detail.

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, the
test conventions, and one constraint worth two minutes of your time before you
spend an afternoon on a keybinding: composemux's interaction model is adapted
from the [Nx terminal UI](https://nx.dev/blog/nx-21-terminal-ui), and matching
it is deliberate rather than incidental.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). For
security issues, please report privately — see [SECURITY.md](SECURITY.md) —
rather than opening an issue.

## Attribution

composemux's terminal UI — its layout, pinning model, colours and keys — is
adapted from the Nx terminal UI, which is MIT licensed and copyright 2017-2026
Narwhal Technologies Inc. The full upstream notice is in
[LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY), and modules derived from Nx name
their upstream file in a header comment. If you already use the Nx TUI, the
keys and the layout here are the same on purpose.

composemux is an independent project. It is not affiliated with, endorsed by,
or sponsored by Nrwl / Nx.

## License

MIT — see [LICENSE](LICENSE).

Contributions are accepted under the same licence (inbound = outbound). You
keep the copyright in your own work; there is no CLA.
