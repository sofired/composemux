# Review guidelines for composemux

composemux is a terminal UI for Docker Compose logs, written in Rust (MSRV 1.88, set by `ratatui`). It reproduces
the Nx terminal UI on purpose: keybindings, layout arithmetic, colours and pinning semantics.

## Where to look

- `CONTRIBUTING.md`: scope, the Nx parity rule, docs and test conventions, and the source map.
- `README.md` ("Driving it from a script"): the exit codes and terminal restoration a wrapper script relies on.
- `src/docker/`: Engine API access, log streaming and event supervision. `src/tui/app.rs`: the state machine.
  `src/model/`: service types and the log buffer. `src/fallback.rs`: plain output when stdout isn't a TTY.
- `demo/`: a Compose project and scripts for trying the tool and recording the README screencast. It isn't part of
  the crate, but its scripts must still do what their messages and `demo/README.md` say.

## Rules a change must keep

- **Read-only.** composemux attaches to containers something else started. Code that starts, stops, restarts or
  execs into a container changes what the tool is: report it.
- **Exit codes and terminal restoration are a contract** with the wrapper script that runs composemux (README,
  "Driving it from a script"). Report any path, panics and errors included, that leaves the terminal in raw mode or
  changes an exit code.
- **Tests:** unit tests need no Docker. They use pure functions and `TestBackend`, and inject time (`now: Instant`)
  rather than sleeping. A test that would still pass with the feature deleted is a finding.
- **Doc comments** say why an item exists and must be true. A stale doc comment is a defect here, not a nit.
- **Modules derived from upstream** carry a header naming the upstream file. Keep it accurate.

## What gets acted on here

- Scripts and docs that claim a check or a cleanup they don't do, and races in the demo: the `demo/stack` and
  `demo/record` scripts and the services under `demo/services/`.
- Security in repository settings: a `CODEOWNERS` change that adds a bot or automation account (the file says
  humans only), and a workflow permission or token broader than its job needs.

## What not to flag

- **Divergence from "intuitive" behaviour that matches Nx.** Layout constants such as the one-third sidebar, the
  breakpoints and the scroll-momentum figures are ported values, not tuned ones.
- **Missing doc comments.** `clippy::missing_docs_in_private_items` already runs with `-D warnings` in CI. Files
  that predate the rule carry a counted `allow` at the top on purpose; only a new one is a finding.
- **Formatting and lints:** CI runs `cargo fmt --check` on Linux and `cargo clippy -D warnings` on Linux, macOS and
  Windows.
- **Claims about Docker or Compose behaviour you haven't confirmed.** The maintainer measures these against a real
  daemon. Include one only when the repository shows the behaviour. A claim with security impact (permissions,
  escaping a directory) may still be raised as a lower-confidence question.
