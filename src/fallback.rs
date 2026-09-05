//! Plain streaming for when stdout isn't a terminal.
//!
//! The wrapping CLI may run in CI or with output piped, where a full-screen UI
//! is useless (and would emit escape sequences into a log file). In that case we
//! behave like `docker compose logs -f`: one prefixed line per log line.

use crate::docker::stream::MAX_CHUNK_BYTES;
use crate::docker::{DockerClient, LogSupervisor, SourceEvent};
use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

/// Longest unterminated run held before it is emitted anyway.
///
/// Output that never sends a newline -- a progress bar driving itself with
/// carriage returns, a stuck process spraying binary -- would otherwise buffer
/// for the life of the process. This path is the unattended one, used under CI
/// and wrapper scripts, so growing without bound here fails where nobody is
/// watching.
///
/// It bounds accumulation *across* chunks, not any single one. That is only a
/// bound at all because the chunks themselves are bounded where they are read,
/// by `MAX_CHUNK_BYTES` in the Docker layer, and by a good deal less than this.
/// A run this long therefore always arrives in several chunks and is flushed
/// here, rather than turning up whole in one chunk and defeating the cap.
const MAX_PARTIAL: usize = 1024 * 1024;

/// Keeping a forwarded piece under this cap is what bounds the buffer itself.
/// `push` appends whatever it is handed before the cap is consulted, so the
/// buffer peaks at this cap plus one piece; a piece smaller than the cap keeps
/// that peak under twice it. Tuning the two apart is a build error rather than
/// a quiet doubling.
///
/// It is *not* what keeps whole lines whole -- a line is emitted whole as long
/// as its newline arrives before the bytes held for it pass the cap, which no
/// chunk size can change for a line that fits the cap in the first place.
const _: () = assert!(MAX_CHUNK_BYTES < MAX_PARTIAL);

/// Buffers partial lines so a chunk boundary never splits output mid-line.
#[derive(Default)]
struct LineAssembler {
    partial: Vec<u8>,
}

impl LineAssembler {
    /// Returns the complete lines contained in `bytes`, holding back any tail.
    ///
    /// The cap is applied while the input is consumed, not after it has been
    /// appended. Checking afterwards bounded what was *kept* rather than what
    /// was allocated: a single unterminated chunk larger than the cap went
    /// into the buffer whole before anything looked at its size.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = bytes;

        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            let (line, tail) = rest.split_at(pos + 1);
            self.partial.extend_from_slice(line);
            let held = std::mem::take(&mut self.partial);
            let text = String::from_utf8_lossy(&held);
            lines.push(text.trim_end_matches(['\n', '\r']).to_string());
            rest = tail;
        }

        // What is left has no newline in it. Hold it back, but take it in
        // cap-sized pieces rather than letting the run grow: a long line
        // printed early is a far better failure than memory climbing until the
        // process dies. A cut can land mid-character, which `from_utf8_lossy`
        // renders as a replacement -- acceptable for output that has already
        // run a megabyte without a newline. A line whose newline arrives while
        // it is still held is emitted whole however long it runs, since
        // splitting real lines would corrupt them for whatever is parsing the
        // log; only a run that passes the cap before any newline gets cut.
        while self.partial.len() + rest.len() > MAX_PARTIAL {
            let room = MAX_PARTIAL - self.partial.len();
            self.partial.extend_from_slice(&rest[..room]);
            rest = &rest[room..];
            let held = std::mem::take(&mut self.partial);
            lines.push(String::from_utf8_lossy(&held).into_owned());
        }
        self.partial.extend_from_slice(rest);
        lines
    }
}

/// Pads service names so the prefixes line up, the way `docker compose logs`
/// does. The width grows as services appear, since they are not all known up
/// front; earlier lines keep the width they were written at.
#[derive(Default)]
struct Prefixes {
    width: usize,
}

impl Prefixes {
    /// The prefix for one line, widening the column if this name is the
    /// longest seen so far.
    ///
    /// Measured and padded in terminal columns, not scalars. `{:<width$}`
    /// counts scalars, so a name of double-width glyphs would be padded to
    /// half the space it actually occupies -- which is the alignment this
    /// exists to provide.
    fn format(&mut self, service: &str) -> String {
        let width = service.width();
        self.width = self.width.max(width);
        format!("{service}{:pad$}  | ", "", pad = self.width - width)
    }
}

/// Streams every service's logs to stdout, one prefixed line at a time, until
/// the stream ends, `cancel` fires, or a write to stdout fails -- the last of
/// which is routine here rather than exotic, since this path exists for piped
/// output and the reader can go away first.
pub async fn run(
    client: &DockerClient,
    project: &str,
    tail: usize,
    cancel: CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<SourceEvent>(4096);
    let supervisor = LogSupervisor::new(client, project, tail, tx);
    let supervisor_cancel = cancel.clone();
    tokio::spawn(async move { supervisor.run(supervisor_cancel).await });

    let mut assemblers: HashMap<String, LineAssembler> = HashMap::new();
    let mut prefixes = Prefixes::default();
    let stdout = std::io::stdout();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            message = rx.recv() => {
                let Some(message) = message else { break };
                if let SourceEvent::Output { service, bytes, .. } = message {
                    let prefix = prefixes.format(&service);
                    let assembler = assemblers.entry(service).or_default();
                    let mut lock = stdout.lock();
                    for line in assembler.push(&bytes) {
                        writeln!(lock, "{prefix}{line}")?;
                    }
                    lock.flush()?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_are_emitted_immediately() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"one\ntwo\n"), vec!["one", "two"]);
    }

    #[test]
    fn a_partial_line_is_held_until_its_newline_arrives() {
        let mut a = LineAssembler::default();
        assert!(a.push(b"half").is_empty(), "no newline yet");
        assert_eq!(a.push(b"-line\n"), vec!["half-line"]);
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"windows\r\n"), vec!["windows"]);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut a = LineAssembler::default();
        let lines = a.push(&[0xff, 0xfe, b'\n']);
        assert_eq!(lines.len(), 1);
    }

    /// Output that never sends a newline must not buffer for the life of the
    /// process.
    #[test]
    fn an_unterminated_run_is_emitted_rather_than_buffered_forever() {
        // A progress bar driving itself with carriage returns never sends a
        // newline, so the buffer would grow for the life of the process.
        let mut a = LineAssembler::default();
        let chunk = vec![b'x'; 64 * 1024];
        let mut emitted = 0;
        for _ in 0..40 {
            emitted += a.push(&chunk).len();
        }
        assert!(emitted > 0, "the held run should have been flushed");
        assert!(
            a.partial.len() <= MAX_PARTIAL,
            "buffer grew to {} bytes",
            a.partial.len()
        );
    }

    /// Services are not all known up front, so the column widens as they
    /// arrive and earlier lines keep the width they were written at.
    #[test]
    fn prefixes_line_up_as_services_appear() {
        let mut p = Prefixes::default();
        assert_eq!(p.format("api"), "api  | ");
        // A longer name widens the column for everything after it.
        assert_eq!(p.format("cleanexit"), "cleanexit  | ");
        assert_eq!(p.format("api"), "api        | ");
        assert_eq!(p.format("db"), "db         | ");
    }

    /// One oversized chunk, rather than the many small ones above: the cap
    /// used to be checked only after the whole chunk had been appended, so it
    /// bounded what was retained and not the allocation itself.
    #[test]
    fn a_single_oversized_chunk_is_broken_up_rather_than_swallowed_whole() {
        let mut a = LineAssembler::default();
        let size = 4 * MAX_PARTIAL + 7;
        let lines = a.push(&vec![b'x'; size]);

        let longest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        assert!(
            longest <= MAX_PARTIAL,
            "emitted a {longest}-byte line, past the {MAX_PARTIAL}-byte cap"
        );
        assert!(a.partial.len() <= MAX_PARTIAL, "held {}", a.partial.len());
        // Bounding it must not lose any of it.
        let total: usize = lines.iter().map(|l| l.len()).sum::<usize>() + a.partial.len();
        assert_eq!(total, size, "bytes went missing");
    }

    /// The cap must not corrupt what it splits. A uniform payload cannot show
    /// this -- every byte is interchangeable, so only a change in total length
    /// is visible -- so this one is non-uniform and reassembled byte for byte.
    #[test]
    fn capped_output_reassembles_to_exactly_what_went_in() {
        let mut a = LineAssembler::default();
        let size = 4 * MAX_PARTIAL + 7;
        // Printable ASCII, no newline: valid UTF-8, so `from_utf8_lossy` is a
        // no-op and the comparison is exact.
        let input: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();

        let lines = a.push(&input);

        let mut back: Vec<u8> = lines.iter().flat_map(|l| l.bytes()).collect();
        back.extend_from_slice(&a.partial);
        assert_eq!(back.len(), input.len(), "byte count changed");
        assert!(back == input, "the pieces do not reassemble to the input");
    }

    /// The other half of the rule: only *unterminated* runs are capped. A line
    /// that ends in a newline is emitted whole however long it runs, because
    /// splitting a real line would corrupt it for whatever parses the log.
    #[test]
    fn a_terminated_line_longer_than_the_cap_is_emitted_whole() {
        let mut a = LineAssembler::default();
        let size = 3 * MAX_PARTIAL;
        let mut input = vec![b'x'; size];
        input.push(b'\n');

        let lines = a.push(&input);

        assert_eq!(lines.len(), 1, "a terminated line was split");
        assert_eq!(lines[0].len(), size, "the line came back short");
        assert!(a.partial.is_empty(), "nothing should be held back");
    }

    /// The cap is `>`, so reaching it exactly holds rather than flushes. This
    /// pins the boundary in both directions, where off-by-one lives.
    #[test]
    fn the_cap_is_reached_before_it_is_exceeded() {
        let mut exact = LineAssembler::default();
        assert!(
            exact.push(&vec![b'x'; MAX_PARTIAL]).is_empty(),
            "reaching the cap exactly should not flush yet"
        );
        assert_eq!(exact.partial.len(), MAX_PARTIAL);

        let mut over = LineAssembler::default();
        let lines = over.push(&vec![b'x'; MAX_PARTIAL + 1]);
        assert!(lines.iter().all(|l| l.len() <= MAX_PARTIAL));
        let total: usize = lines.iter().map(|l| l.len()).sum::<usize>() + over.partial.len();
        assert_eq!(total, MAX_PARTIAL + 1, "bytes went missing at the boundary");
    }

    /// Crossing the cap across many calls, rather than inside one chunk: the
    /// other test drives the same limit but never leaves `push`.
    #[test]
    fn many_small_chunks_crossing_the_cap_lose_no_bytes() {
        let mut a = LineAssembler::default();
        let chunk = vec![b'x'; 64 * 1024];
        let mut emitted = 0usize;
        for _ in 0..40 {
            for line in a.push(&chunk) {
                // The chunk size divides the cap exactly, so `partial` lands on
                // MAX_PARTIAL and the next call enters the flush loop already
                // full -- a state no single-push test can reach.
                assert!(
                    line.len() <= MAX_PARTIAL,
                    "a cross-call flush emitted {} bytes, past the {MAX_PARTIAL}-byte cap",
                    line.len()
                );
                emitted += line.len();
            }
        }
        assert_eq!(
            emitted + a.partial.len(),
            40 * chunk.len(),
            "bytes went missing across calls"
        );
    }

    /// A chunk boundary can fall between the `\r` and the `\n`, which puts the
    /// two halves of the terminator in different calls -- and the rewrite
    /// moved the newline search from the buffer onto the incoming slice.
    #[test]
    fn a_crlf_split_across_chunks_is_still_trimmed() {
        let mut a = LineAssembler::default();
        assert!(a.push(b"windows\r").is_empty(), "no newline yet");
        assert_eq!(a.push(b"\nnext\n"), vec!["windows", "next"]);
    }

    /// Bytes flushed past the cap are a raw, unterminated run -- not a line --
    /// so a byte that merely looks like a terminator has to survive verbatim.
    ///
    /// The two loops in `push` are structurally alike and only one of them
    /// should trim: giving the cap flush the same `trim_end_matches` as the
    /// newline path silently drops a byte from output that was never a line,
    /// and every other test in this module stays green when it does.
    #[test]
    fn a_capped_flush_keeps_a_trailing_byte_that_looks_like_a_terminator() {
        let mut a = LineAssembler::default();
        let mut input = vec![b'x'; MAX_PARTIAL - 1];
        input.push(b'\r'); // lands exactly on the cap boundary
        input.extend(std::iter::repeat_n(b'x', 10));

        let lines = a.push(&input);

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].len(),
            MAX_PARTIAL,
            "the trailing byte was dropped from the flushed piece"
        );
        assert!(
            lines[0].ends_with('\r'),
            "the trailing carriage return was trimmed"
        );
    }

    /// The seam between the two loops: one call that both emits a line and
    /// holds a tail.
    #[test]
    fn one_push_can_emit_a_line_and_hold_a_tail() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"one\ntwo"), vec!["one"]);
        assert_eq!(a.push(b"-end\n"), vec!["two-end"]);
    }

    /// Service names are ASCII in practice, but the padding should be honest
    /// about what it measures.
    #[test]
    fn a_wide_name_is_padded_by_the_columns_it_occupies() {
        let mut p = Prefixes::default();
        // Three scalars, six columns.
        assert_eq!(
            p.format("\u{65e5}\u{672c}\u{8a9e}"),
            "\u{65e5}\u{672c}\u{8a9e}  | "
        );
        // The column is six wide now, so a two-column name takes four spaces.
        assert_eq!(p.format("ab"), "ab      | ");
    }

    /// The stream layer now hands this path a long line in `MAX_CHUNK_BYTES`
    /// pieces where it used to hand it over whole. Anything under the
    /// assembler's own cap has to come back out as one line regardless, or the
    /// bound at the read site would have changed what gets printed.
    #[test]
    fn a_line_arriving_in_forwarded_pieces_still_prints_whole() {
        let mut a = LineAssembler::default();
        // The largest line the buffer is guaranteed to hold, so this exercises
        // the whole of it rather than a comfortable middle. Multi-byte
        // throughout, so the cuts land mid-character -- `from_utf8_lossy` runs
        // on the reassembled buffer, so a cut there must not corrupt anything.
        let unit = "h\u{e9}llo\u{b7}w\u{f6}rld ";
        let body: String = std::iter::repeat_n(unit, MAX_PARTIAL / unit.len()).collect();
        assert!(
            body.len() > MAX_CHUNK_BYTES,
            "the line has to span more than one forwarded piece to test anything"
        );
        let mut input = body.clone().into_bytes();
        input.extend_from_slice(b"\r\n");

        let mut lines = Vec::new();
        for piece in input.chunks(MAX_CHUNK_BYTES) {
            lines.extend(a.push(piece));
        }

        assert_eq!(lines, vec![body], "the split changed what is printed");
        assert!(a.partial.is_empty(), "held {} bytes back", a.partial.len());
    }

    /// Where the split does change what is printed, pinned exactly.
    ///
    /// A terminated line survives whole while the bytes held for it stay
    /// within the cap when each piece lands, so the boundary sits past the cap
    /// rather than on it. Beyond it the line is cut, which is the behaviour
    /// bounding the frame gives up: a line that long used to be printed whole
    /// when the daemon happened to deliver it in one frame, and is now cut
    /// whichever way it arrives.
    ///
    /// The numbers below are the current constants', where a piece divides the
    /// cap exactly and the boundary lands on `MAX_PARTIAL + MAX_CHUNK_BYTES`.
    /// A piece size that does not divide the cap moves it, because the run
    /// then overshoots mid-piece instead of landing on the cap, so retuning
    /// the bound means recomputing this rather than assuming the sum holds.
    #[test]
    fn a_terminated_line_survives_until_the_cap_plus_one_piece() {
        fn pieces_for(len: usize) -> Vec<String> {
            let mut a = LineAssembler::default();
            let mut input = vec![b'x'; len];
            input.push(b'\n');
            let mut lines = Vec::new();
            for piece in input.chunks(MAX_CHUNK_BYTES) {
                lines.extend(a.push(piece));
            }
            lines
        }

        let whole = pieces_for(MAX_PARTIAL + MAX_CHUNK_BYTES - 1);
        assert_eq!(whole.len(), 1, "cut a line that still fits");
        assert_eq!(whole[0].len(), MAX_PARTIAL + MAX_CHUNK_BYTES - 1);

        let cut = pieces_for(MAX_PARTIAL + MAX_CHUNK_BYTES);
        assert_eq!(cut.len(), 2, "kept a line past the point it can be held");
        assert_eq!(cut[0].len(), MAX_PARTIAL);
    }

    #[test]
    fn a_chunk_split_mid_line_reassembles_correctly() {
        let mut a = LineAssembler::default();
        a.push(b"start");
        a.push(b"-middle");
        assert_eq!(a.push(b"-end\nnext\n"), vec!["start-middle-end", "next"]);
    }
}
