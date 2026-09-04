# Contributing to composemux

Bug reports, fixes, docs, features — all welcome, and small PRs are genuinely
fine. This guide exists so you don't spend an evening on something that was
always going to be declined, and so you know what CI is about to say before it
says it.

Start with the next section. It's short, and it's the most common reason a
well-written PR gets turned down.

## Before you change a keybinding

composemux reproduces the [Nx terminal UI](https://nx.dev/blog/nx-21-terminal-ui)
— its keybindings, layout arithmetic, colours and pinning semantics — on
purpose, because people arrive already knowing how to drive it, and that
transfer *is* the feature. So a change that makes a binding "more intuitive"
but diverges from Nx will usually be declined even when it's better in
isolation; if you think a divergence is genuinely warranted, open an issue and
make the case before you write the code.

Two related notes, so they don't surprise you mid-review:

- Layout constants (`⌊width/3⌋` sidebars, the auto-layout breakpoints, the
  scroll-momentum figures) are ported values, not tuned ones. They look
  arbitrary because they are somebody else's arbitrary.
- Modules derived from upstream carry a header comment naming the file they
  came from. Keep it accurate when you edit the module.

Deviations do exist — auto-exit only firing on a clean shutdown, and scroll
position anchoring to content — and both earned their place by following from
the same fact: compose services are long-running where Nx tasks are short.
That's the shape of argument that works.

If you're adapting code from anywhere else, say so in the PR and name the
upstream file. See [LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY).

## Scope

composemux is **read-only** by design. It attaches to containers something else
started, and never starts, stops, restarts or execs into them. It's meant to
sit inside a wrapper script that owns `compose up` and `compose down`, which is
why its exit codes and terminal restoration are part of its contract with that
caller rather than incidental details.

A PR that adds lifecycle control would change what the tool *is*, so please
open an issue first rather than arriving with an implementation you've already
written.

## Getting set up

You need Rust 1.88 or newer (the MSRV, set by `ratatui`):

```sh
git clone https://github.com/sofired/composemux
cd composemux
cargo build
```

Docker is **not** required to build or to run the tests — they use pure
functions, a `TestBackend`, and a fake environment rather than a live daemon.
You'll want Docker to actually drive the thing:

```sh
cargo run -- --project <some-running-compose-project>
```

## Before you open a PR

CI runs these on Linux, macOS and Windows, and they all have to pass:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --no-default-features   # clipboard fallback is optional
```

## Tests

The suite is the safety net that makes a port maintainable: it's what stops an
innocuous refactor from quietly changing behaviour that's supposed to match
upstream. New behaviour should come with tests, and the existing style is worth
matching:

- **Name the scenario and the outcome**, not the function under test.
  `a_running_container_whose_task_died_is_reattached`, not `test_resync`. Test
  names are the only documentation anyone reads at 2am.
- **Prefer pure functions over mocks.** Where logic seems to need Docker or a
  terminal, the decision usually pulls out — see `event_decision` and
  `plan_attachments` in `src/docker/stream.rs`, or `build_service` in
  `src/docker/client.rs`.
- **Inject time rather than sleeping.** `ScrollMomentum::scroll` and
  `App::tick` both take a `now: Instant` for exactly this reason.
- **Assert on observable behaviour.** A test that would still pass with the
  feature deleted is worse than no test, because it reads as coverage.

Rendering is tested by drawing into a `ratatui::backend::TestBackend` and
asserting on the resulting buffer (`src/tui/render.rs`), and the layout
arithmetic has property tests that sweep every terminal size.

## Where things live

```
src/config.rs          config file + CLI flag merging
src/project.rs         compose project-name resolution
src/docker/            Engine API access, log streaming, event supervision
src/model/             Service/status types, and the vt100-backed log buffer
src/tui/app.rs         state machine: focus, pinning, filter, key dispatch
src/tui/components/    rendering
src/fallback.rs        plain streaming when stdout is not a TTY
```

## Commits and pull requests

- Short, imperative subject line: "Fix …", not "Fixed …".
- Explain *why* in the body when the diff doesn't already say it.
- One logical change per PR, where you can manage it.
- If you change user-visible behaviour, update the README and mention it in the
  PR.

## Licensing your contribution

composemux is MIT and stays MIT — it's derived from MIT-licensed code, and a
single permissive licence keeps the provenance unambiguous.

By submitting a contribution you agree it's licensed under the MIT Licence, the
same terms as the project ("inbound = outbound"). You keep the copyright in
your own work; there's no CLA and no copyright assignment.

Please sign off your commits to certify you have the right to submit them under
that licence — this is the
[Developer Certificate of Origin](https://developercertificate.org/):

```sh
git commit -s
```

## Reporting bugs

Include your OS, `composemux --version`, `docker version`, and what the
terminal was doing at the time. For streaming or attachment problems,
`COMPOSEMUX_DEBUG=1` writes diagnostics to `composemux.log` in your system temp
directory — the UI can't log to stdout while it owns the screen.

For security issues, don't open an issue: [SECURITY.md](SECURITY.md) has the
private reporting route.
