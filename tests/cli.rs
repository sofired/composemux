//! Binary-level tests.
//!
//! These cover the contract the wrapping CLI depends on: exit status, and the
//! guarantee that a startup failure never leaves escape sequences (or a
//! raw-mode terminal) behind, because it happens before the UI starts.

use std::process::{Command, Stdio};

/// A port nothing listens on, so the daemon connection is refused immediately.
const UNREACHABLE_DOCKER: &str = "tcp://127.0.0.1:1";

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_composemux"));
    cmd.stdin(Stdio::null());
    cmd
}

#[test]
fn an_unreachable_daemon_fails_with_a_useful_message() {
    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Docker daemon"),
        "stderr should name the cause, got: {stderr}"
    );
}

#[test]
fn a_startup_failure_writes_no_escape_sequences() {
    // The terminal is only put into raw mode after the daemon connection
    // succeeds. If that ordering ever changes, a failing run would corrupt the
    // calling script's terminal.
    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(
        !output.stdout.contains(&0x1b),
        "stdout contained an escape sequence"
    );
    assert!(
        !output.stderr.contains(&0x1b),
        "stderr contained an escape sequence"
    );
}

#[test]
fn help_and_version_succeed_without_touching_docker() {
    for flag in ["--help", "--version"] {
        let output = bin()
            .env("DOCKER_HOST", UNREACHABLE_DOCKER)
            .arg(flag)
            .output()
            .expect("the binary should run");
        assert!(output.status.success(), "{flag} should exit zero");
        assert!(!output.stdout.is_empty(), "{flag} should print something");

        if flag == "--version" {
            // Pin the format the Homebrew formula's `test do` asserts on
            // (`composemux <version>`), so a change to the binary name or the
            // version string fails here at PR time rather than in the release
            // rehearsal on a pushed tag.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let expected = format!("composemux {}", env!("CARGO_PKG_VERSION"));
            assert!(
                stdout.contains(&expected),
                "--version should print {expected:?}; got {stdout:?}"
            );
        }
    }
}

#[test]
fn an_unknown_flag_is_rejected() {
    let output = bin()
        .arg("--definitely-not-a-flag")
        .output()
        .expect("the binary should run");
    assert!(!output.status.success());
}

#[test]
fn a_malformed_config_file_is_reported_rather_than_ignored() {
    let dir = std::env::temp_dir().join(format!("composemux-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.yaml");
    // `pined` is a typo for `pinned`; unknown keys must be a loud error.
    std::fs::write(&path, "pined: [api]\n").unwrap();

    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--config", path.to_str().unwrap(), "--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pined") || stderr.contains("unknown field"),
        "stderr should name the bad key, got: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A stand-in Docker daemon that accepts a connection and then says nothing.
///
/// It gives a test a real readiness handshake: the accept returns only once
/// the child has reached its first call to the daemon, which is the moment
/// after the signal handlers are installed. Never answering keeps the child
/// parked there, so what the test then observes can only have come from the
/// waiting.
///
/// At file scope rather than inside `signals`, because two things now want a
/// daemon that goes quiet: the signal statuses, and the notice startup prints
/// once it has been waiting long enough to mention it (#61).
#[cfg(unix)]
struct StalledDaemon {
    /// Accepts the child's connection. Held for the life of the test: closing
    /// it would answer the child and let it get on.
    listener: std::net::TcpListener,
    /// The port `listener` was given, for the child's `DOCKER_HOST`.
    port: u16,
}

#[cfg(unix)]
impl StalledDaemon {
    /// Starts listening on a free port.
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().unwrap().port();
        Self { listener, port }
    }

    /// The command that runs the binary against this daemon, with its output
    /// left for the caller to route.
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_composemux"));
        cmd.args(["--project", "any-project", "--no-tui"])
            .env("DOCKER_HOST", format!("tcp://127.0.0.1:{}", self.port))
            // Inherited TLS settings would send it somewhere else.
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .stdin(Stdio::null());
        cmd
    }

    /// The same, with its output discarded, for a test that only watches the
    /// process.
    fn spawn_client(&self) -> std::process::Child {
        self.command()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary should run")
    }

    /// Blocks until the child is waiting on the daemon.
    fn await_client(&self) -> std::net::TcpStream {
        let (stream, _) = self.listener.accept().expect("the child should connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = [0u8; 1];
        // It has sent its first request and is now waiting on a reply that
        // will never come.
        let _ = std::io::Read::read(&mut (&stream), &mut request);
        stream
    }
}

/// #61: startup awaits the daemon with nothing beside it but the cancellation
/// token, so a daemon that takes the connection and goes quiet leaves a
/// terminal that has printed nothing for the two minutes bollard's own request
/// timeout takes to end it.
///
/// Here rather than beside the unit tests because the unit tests cover the
/// wait and the wording, and what they cannot reach is `run` wiring the two
/// together: unwire it and the notice is still built, still correct, and never
/// printed. That is the whole of the user-visible fix, and it is one line.
#[cfg(unix)]
mod startup {
    use super::StalledDaemon;
    use std::io::{BufRead, BufReader, Read};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Long enough for the five-second bound plus a slow machine, short enough
    /// that a notice that never comes is reported rather than left to the
    /// harness's own timeout.
    const DEADLINE: Duration = Duration::from_secs(30);

    #[test]
    fn a_startup_the_daemon_never_answers_says_so_on_stderr() {
        let daemon = StalledDaemon::start();
        let mut child = daemon
            .command()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the binary should run");
        // Held for the rest of the test: dropping it would answer the child
        // with a closed connection and let it exit on its own.
        let _connection = daemon.await_client();

        // Read on a thread, because the notice arrives while the child is
        // still running -- there is no output to collect at exit, which is the
        // difference between this and every other test in this file.
        let (tx, rx) = mpsc::channel();
        let stderr = child.stderr.take().expect("stderr was piped");
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        let notice = rx.recv_timeout(DEADLINE);
        // Killed before stdout is read, not after: reading to EOF waits for
        // the child to exit, and the whole point of this one is a child that
        // does not, until bollard's own two-minute timeout ends it.
        let _ = child.kill();
        let _ = child.wait();
        let mut stdout = String::new();
        let _ = child
            .stdout
            .take()
            .expect("stdout was piped")
            .read_to_string(&mut stdout);

        let notice = notice.expect("startup said nothing at all while it waited");
        assert!(
            notice.contains("waiting for the Docker daemon"),
            "the first thing startup said was not that it was waiting: {notice:?}"
        );
        assert!(
            notice.contains(&daemon.port.to_string()),
            "the notice has to name what it is waiting on: {notice:?}"
        );
        // stderr, not stdout: in `--no-tui` the calling script is capturing
        // stdout, and a note written into it would be indistinguishable from a
        // line the service logged.
        assert!(
            !stdout.contains("waiting for the Docker daemon"),
            "the notice belongs on stderr, not in the captured output: {stdout:?}"
        );
    }
}

#[cfg(unix)]
mod signals {
    use super::StalledDaemon;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};

    /// A terminating signal should produce the status a supervisor expects,
    /// `128 + signo`, so it can tell its own shutdown from a user quitting.
    ///
    /// Returns the raw status rather than a code, because the difference that
    /// matters here is invisible in the number: killing the process outright
    /// yields `128 + signo` as well, since that is what a shell reports for a
    /// signalled child. Only a *normal* exit carrying that code proves our own
    /// handler ran and got the chance to put the terminal back.
    fn exit_status_for(signal: &str) -> std::process::ExitStatus {
        let daemon = StalledDaemon::start();
        let mut child = daemon.spawn_client();
        // Held open for the rest of the test: dropping it would answer the
        // child with a closed connection and let it exit on its own.
        let _connection = daemon.await_client();

        let killed = Command::new("kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status()
            .expect("kill should run");
        assert!(killed.success(), "could not signal the child");

        wait_bounded(&mut child, signal)
    }

    /// Reaps the child, giving up rather than blocking forever.
    ///
    /// The regression this test guards against is the process *not* exiting,
    /// and `wait()` on a child that never exits blocks until the harness gives
    /// up -- reporting a timeout on the whole binary rather than naming the
    /// signal that went unhandled.
    fn wait_bounded(child: &mut Child, signal: &str) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait().expect("the child should be reapable") {
                Some(status) => return status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the child was still running 10s after SIG{signal}");
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn terminating_signals_use_the_conventional_status() {
        use std::os::unix::process::ExitStatusExt;

        for (signal, expected) in [("TERM", 143), ("HUP", 129), ("INT", 130)] {
            let status = exit_status_for(signal);
            assert_eq!(
                status.signal(),
                None,
                "SIG{signal} killed the process outright; the handler never ran, \
                 so the terminal was left as it was"
            );
            assert_eq!(
                status.code(),
                Some(expected),
                "SIG{signal} should exit {expected}"
            );
        }
    }
}
