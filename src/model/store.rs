#![allow(clippy::missing_docs_in_private_items)] // 13 left to document
//! Per-service log buffer.
//!
//! Container output is fed straight into a `vt100` terminal emulator, the same
//! way nx feeds its PTY output. That gets us SGR colour, `\r` progress rewrites
//! and cursor motion handled correctly, plus per-cell access so search matches
//! can be highlighted over already-coloured output.
//!
//! Scrollback semantics follow nx: the offset counts rows *back from the
//! bottom*, so `0` means "tailing live output".

/// Rows of scrollback retained per service. Matches nx's `SCROLLBACK_SIZE`.
pub const DEFAULT_SCROLLBACK: usize = 1_000;

/// Hard ceiling on the retained buffer, per service.
///
/// The line budget below is what keeps the buffer correct; this only guards
/// against pathological content — enormous lines, or output with no newlines at
/// all — where a line count says nothing about size. Hitting it can still cost
/// retained history, but at eight megabytes rather than at a few hundred
/// kilobytes, which is where a bytes-per-row estimate used to give out.
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;

/// Whether `dropped` can have moved the pen away from wherever it stood.
///
/// What this claims is about vte's framing rather than about which attributes
/// exist: only an escape sequence changes attribute state, and below `0x80`
/// nothing but `ESC` starts one. C0 controls move the cursor and printable
/// bytes write cells in the pen already set, so a run of plain seven-bit output
/// leaves the pen exactly where it was.
/// `no_seven_bit_byte_but_escape_moves_the_pen` puts that to the crate rather
/// than asserting it, so it fails if `vt100` ever gives one of those bytes an
/// effect on attributes -- which is the same reason `attributes_formatted` is
/// used below rather than a list of attributes.
///
/// Bytes at or above `0x80` are excluded rather than reasoned about, and that
/// half is defensive rather than load-bearing. `vt100` 0.16.2 drives vte in
/// UTF-8 mode, where `0x9B` is a bad UTF-8 lead byte rather than the C1 CSI it
/// is in an eight-bit stream, so no high byte moves the pen today: there is no
/// pen this half saves, and no test can show one. It is here because that is a
/// fact about a mode rather than about the protocol.
///
/// Which makes it exactly the kind of condition someone mutation-testing their
/// way through this function deletes as dead. So it is pinned at the predicate
/// instead of at a pen: `anything_with_an_escape_byte_in_it_still_reaches_the_
/// parse` fails on its non-ASCII case if the half goes. The cost of keeping it
/// is only that a service whose logs are not plain ASCII pays what every
/// service pays today.
fn can_move_the_pen(dropped: &[u8]) -> bool {
    dropped.iter().any(|b| *b == 0x1b || *b >= 0x80)
}

/// The SGR sequence reproducing the styling left active after `prefix` and then
/// `dropped` are parsed.
///
/// Both halves defer to the crate. The dropped bytes go through a scratch
/// emulator rather than being scanned for escape codes, and the result is
/// serialised by `attributes_formatted`, which diffs the parser's own attribute
/// state against the default. Hand-enumerating attributes is what dropped `dim`
/// once already, and would drop the next one the crate learns about.
///
/// Unconditional: `prefix` and `dropped` are parsed whatever is in them. The
/// caller is where a parse gets skipped, because skipping one is only sound
/// against a `prefix` that leaves vte in its ground state, and this function
/// has no way to know that it does. `the_carried_pen_reproduces_every_attribute`
/// splits a run of SGR sequences at an arbitrary byte, so it reaches this with
/// a `prefix` ending mid-sequence -- and caught exactly that when the guard
/// lived here.
fn pen_after(prefix: &[u8], dropped: &[u8]) -> Vec<u8> {
    let mut scratch = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
    scratch.process(prefix);
    scratch.process(dropped);
    scratch.screen().attributes_formatted()
}

fn bytecount(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
}

fn keep_lines_for(scrollback: usize, rows: u16) -> usize {
    scrollback.saturating_add(rows as usize).max(1)
}

/// Floor on the emulated screen size.
///
/// `vt100` underflows in `col_wrap` on very narrow grids, so this is a crash
/// guard rather than a cosmetic minimum. It matches the floor the pane's own
/// geometry already applies, so it never binds in the render path.
const MIN_ROWS: u16 = 3;
const MIN_COLS: u16 = 20;

/// Size used before the first layout pass tells us the real pane geometry.
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

/// Written into the retained stream when the store changes hands, ahead of the
/// row break, to keep the dead container's emulator state off its successor.
///
/// Seven sequences, plus one thing that happens before any of them runs.
///
/// That first thing is the abort, and it is the leading `ESC` rather than a
/// sequence of its own. `ESC` is an anywhere-transition in vte's state
/// machine, so whatever half-written sequence the dead container left open --
/// a CSI part way through its parameters, a bare `ESC`, an unterminated OSC,
/// DCS, APC or PM string -- ends at the first byte of `CSI ? 47 l`, and what
/// follows is parsed as itself rather than eaten as somebody else's
/// parameters. Without it a container that stopped after `\x1b[3` has its
/// successor's first byte complete `\x1b[3n` and vanish, and one that stopped
/// inside an OSC swallows everything the successor writes until some byte
/// happens to terminate the string -- which may be nothing it ever writes.
/// #72 has both measurements.
///
/// So this has to *lead* with an escape sequence, and the row break has to
/// come after it rather than before: a string sequence collects C0 controls
/// instead of executing them, so a break written first is swallowed with
/// everything else. An explicit `ESC \` in front was tried and removed --
/// with `CSI ? 47 l` behind it no test could tell the two apart, because the
/// abort is the `ESC`, not the ST.
///
/// `CSI ? 47 l` leaves the alternate screen. A container that entered it and
/// died leaves the pane rendering an alternate grid that has no scrollback and
/// none of the service's history in it, and its successor writing into that
/// same grid; the pane looks empty and stays that way. `47` rather than the
/// `1049` an application would have entered with, because vt100 keeps one
/// `MODE_ALTERNATE_SCREEN` bit for both -- so this clears the mode however it
/// was set -- while its `1049` reset also ends in a `decrc`, which here would
/// restore a `saved_pos` nothing in the handover had written yet and move the
/// cursor to a row that already has output on it. The `CSI ? 1049 l` at the
/// end of the handover is safe for exactly the reason this one would not be:
/// by then the `CSI ? 1049 h` in front of it has taken that save itself.
///
/// `CSI m` resets the pen. This is the half that does not need a half-written
/// sequence to bite: a container that sets a colour and exits cleanly on a
/// newline leaves the pen set, and every line its successor writes comes out in
/// the dead container's colour.
///
/// `ESC 7` `CSI r` `ESC 8` resets the scroll region, which is #75. A region
/// left set is not a cosmetic inheritance: `Grid::scroll_up` pushes an evicted
/// row into the scrollback only `if !self.scroll_region_active()`, so a
/// successor writing inside a dead container's region scrolls within it and
/// the pane accumulates no history at all.
///
/// `CSI r` is the only sequence vt100 0.16.2 implements that resets the
/// primary grid's region without throwing the grid away with it, and with no
/// parameters it is a reset at any pane size:
/// `canonicalize_params_decstbm` defaults them to the full height, and
/// `set_scroll_region` falls back to the full height on its `else` branch in
/// any case. What made it unusable alone is its last two lines,
/// `self.pos.row = self.scroll_top; self.pos.col = 0` -- the cursor homed to
/// the top of the visible grid, where the successor would write over rows the
/// dead container's output is still on. #75 read that as the reason the region
/// had to be left set. The save and restore around it are what make it
/// affordable: `ESC 7` and `ESC 8` are DECSC and DECRC, which vt100 routes to
/// `Screen::save_cursor` and `Screen::restore_cursor`, so the position the
/// homing throws away goes straight back -- and all three are plain bytes,
/// which is what the replay constraint asks for.
///
/// The order of the three inside the handover is not free.
///
/// `ESC 7` has to come after `CSI ? 47 l`, because `saved_pos` lives on the
/// `Grid` and vt100 reaches it through `grid_mut()`. Saved on the alternate
/// grid and restored on the primary one, the restore reads a `saved_pos`
/// nothing wrote and homes the cursor after all.
/// `a_recreate_brings_a_pane_back_from_the_alternate_screen` does *not* catch
/// that: it enters with `CSI ? 1049 h`, which takes a `decsc` on the primary
/// grid on the way in, so the misplaced restore reads a `saved_pos` that
/// happens to be right. `the_handover_saves_the_cursor_on_the_grid_it_puts_it_back_on`
/// enters with `CSI ? 47 h`, which does not.
///
/// `CSI m` has to come before `ESC 7`, because `Screen::save_cursor` saves
/// `self.attrs` alongside the position and `restore_cursor` puts both back.
/// Saving first would park the dead container's pen in `saved_attrs`, where a
/// successor's own `ESC 8` hands it straight back.
///
/// What `ESC 7` overwrites is a saved cursor the dead container set, which is
/// as dead as the rest of its state, and what replaces it is at worst
/// harmless. A save is three things here, not one: the position the handover
/// happens at, which is where the successor is about to write; the default pen
/// the `CSI m` in front of it has just set; and, because `Grid::save_cursor`
/// covers it too, the dead container's origin mode -- which is inert while the
/// region stays full height. `Grid` reads the flag four times, and that save is
/// one of them. The other three are in `set_pos` and are the only ones with any
/// geometric effect, and all three collapse once `CSI r` has put `scroll_top`
/// back to 0: the offset adds nothing, `row_clamp_top` cannot fire against a
/// top of 0, and `row_clamp_bottom`'s region bound is `size.rows - 1`, which is
/// the bound it would have used anyway.
///
/// `CSI ? 1049 h` `CSI ? 1049 l` clears the alternate grid, which is #79.
/// Everything grid-scoped ahead of this pair lands on the *primary* grid, and
/// the `CSI ? 47 l` is what makes it: vt100 keeps `pos`, `saved_pos`,
/// `scroll_top`, `scroll_bottom`, `origin_mode` and `saved_origin_mode` per
/// `Grid` and reaches them through `Screen::grid_mut()`, which picks a grid
/// off `MODE_ALTERNATE_SCREEN`. So the region reset and the row break behind
/// it both land on the primary grid, and a container that drew on the
/// alternate one and died leaves its content, its cursor and its scroll region
/// sitting there for a successor. The pen is not among them, and that is worth
/// saying rather than leaving to be inferred: `Screen::attrs` is not per-grid,
/// so the `CSI m` already covers both.
///
/// The two ways in are not symmetric, which is the whole of it. `CSI ? 47 h`
/// is `enter_alternate_grid()` alone; `CSI ? 1049 h` is `decsc();
/// alternate_grid.clear(); enter_alternate_grid()`. A successor entering with
/// `1049` clears the grid on its own way in and inherits nothing; one entering
/// with bare `47` inherits all of it. Entering and leaving once here is how
/// that clear is reached without waiting for a successor that may never ask
/// for it. `Grid::clear` resets every row and all six of the scalars above in
/// one call -- everything a dead container can move on a grid except the
/// geometry and the scrollback, and the alternate grid is `Grid::new(size, 0)`
/// with no scrollback to move.
///
/// Of the three inherited things the scroll region is the expensive one, for
/// #75's reason on the grid #75's fix does not reach: a successor confined to
/// a dead container's region has the pane's usable height cut to that region's
/// for as long as the store lives. The content costs a row neither container
/// wrote, which is the #60 fault the row break exists to prevent and cannot,
/// because the break lands on the primary grid. The cursor costs leading blank
/// rows and one indented line, and self-corrects within a screenful.
///
/// The pair goes at the end, and both halves of that placement have a reason.
///
/// It has to be after the `CSI ? 47 l`, because `CSI ? 1049 l` ends in a
/// `decrc` and `decrc` reads whichever grid `MODE_ALTERNATE_SCREEN` selects
/// when it runs. Ahead of the exit, with the dead container still on the
/// alternate grid, the `1049 h` saves there while the `1049 l` -- which clears
/// the mode before it restores -- reads the primary grid's `saved_pos`
/// instead: the dead container's, or `Pos::default()`. That is the `ESC 7`
/// constraint above, arriving from the other end.
///
/// It is after the `ESC 8` rather than merely after the `CSI ? 47 l` so that
/// the `ESC 7` keeps its own reason to be where it is. `CSI ? 1049 h`'s
/// `decsc` is itself a save on the primary grid, so a pair sitting ahead of
/// the `ESC 7` covers for an `ESC 7` taken on the wrong grid and
/// `the_handover_saves_the_cursor_on_the_grid_it_puts_it_back_on` stops
/// failing when the `ESC 7` is moved in front of the `CSI ? 47 l`. Measured on
/// that move: five tests fail with the pair last, four with the pair between
/// the `CSI ? 47 l` and the `CSI m`, and the one that drops out is the cursor
/// test. Both orderings fix #79 identically, so the tie is broken on which
/// keeps that constraint observable.
///
/// What the pair does not do is reach origin mode on the primary grid, which
/// is #78. `Grid::clear` resets `origin_mode`, but only ever on the alternate
/// grid -- `self.alternate_grid.clear()` is its one caller in the crate -- and
/// `1049 l`'s `decrc` restores `saved_origin_mode` from the `decsc` `1049 h`
/// took a moment earlier, so the round trip returns primary origin mode
/// exactly as it found it. #78 is untouched by this, in either direction.
///
/// What it costs is an alternate grid that would otherwise stay unallocated:
/// `enter_alternate_grid` ends on `allocate_rows()`, so every store that takes
/// a handover now materialises one, where before this change `rows` stayed
/// `vec![]` unless a container really used the alternate screen. Measured against
/// vt100 0.16.2 with a counting global allocator, live bytes across one
/// `CSI ? 1049 h` `CSI ? 1049 l` round trip: 62,208 B at 24x80, 321,600 B at
/// 50x200, 961,920 B at 60x500 -- `rows * (cols + 1) * 32` at all three, which
/// is the grid plus the `Row` headers. A second round trip costs zero, because
/// `allocate_rows` is a no-op once `rows` is non-empty, so this is once per
/// store rather than once per recreate. Against the filled-grid figures in
/// [`LogStore::release`]'s doc that is 2.4% at 24x80, 4.8% at 50x200 and 5.7%
/// at 60x500, and `release` drops it with the rest of the parser, so it is
/// bounded by the panes that are actually on screen.
///
/// A trim cannot orphan the restore into something worse. `trim_point`'s line
/// budget cuts immediately after a `\n` and this carries none, so it takes the
/// whole handover or none of it; only the byte-ceiling fallback can land
/// inside, and it cuts at the *front* of what survives -- where the cursor is
/// at the origin and `saved_pos` is still `Pos::default()`, so an `ESC 8` with
/// no `ESC 7` ahead of it restores the position the replay already has.
/// There are two restores rather than one now, and the argument covers both
/// unchanged, because it is an argument about `decrc` rather than about
/// `ESC 8`: `CSI ? 1049 l` is `exit_alternate_grid()` -- inert on a parser not
/// in the alternate screen -- followed by that same `decrc`, reading the same
/// `Pos::default()`. A cut shallow enough to keep the `ESC 8` keeps the
/// `CSI ? 1049 h` with it, so the pair is matched and only the `ESC 8` is
/// orphaned; a cut deep enough to orphan the `CSI ? 1049 l` has taken the
/// `ESC 8` away entirely. Either way it is one restore against a replay that
/// has read nothing but the pen.
///
/// The full resets stay rejected. `ESC c` (RIS) rebuilds the screen from
/// nothing, discarding the grid *and* the scrollback, which is the pane
/// history #46 exists to protect, and it is in `raw`, so the replay discards
/// it again. `CSI ! p` (DECSTR) reaches vt100's unhandled-CSI callback and
/// does nothing at all.
const HANDOVER: &[u8] = b"\x1b[?47l\x1b[m\x1b7\x1b[r\x1b8\x1b[?1049h\x1b[?1049l";

pub struct LogStore {
    parser: vt100::Parser,
    /// Rows of scrollback the parser retains; needed to rebuild it on resize.
    scrollback_len: usize,
    /// Normalised bytes as fed to the parser, replayed when the width changes.
    ///
    /// `vt100` stores rows already wrapped and does not reflow them, so the
    /// only way to rewrap history is to parse it again at the new width.
    raw: Vec<u8>,
    /// Complete lines to keep in `raw`.
    keep_lines: usize,
    /// Newlines currently in `raw`, tracked so trimming does not rescan.
    lines: usize,
    /// Whether the previous chunk ended on a carriage return, so a `\r\n` split
    /// across chunks isn't mistaken for a bare newline.
    pending_cr: bool,
    /// ID of the container whose output the store is currently holding.
    ///
    /// Empty until the first [`LogStore::adopt`], which cannot collide with a
    /// real container ID -- and would not matter if it could, since there is
    /// no row part way through at that point to break.
    container: String,
    /// Which attach to that container the output is currently coming from.
    ///
    /// Zero until the first [`LogStore::adopt`]; `AttachIds` hands out ids from
    /// one, so no attach can be mistaken for that starting state.
    attach: u64,
    /// Set when the parser holds a size it was never given content at -- a
    /// replay that had to be skipped, or an emulator released while off-screen
    /// -- so the next resize replays instead of short-circuiting on the size.
    replay_pending: bool,
    /// The styling active where `raw` begins.
    ///
    /// Trimming drops the bytes that set it, so without this a replay would
    /// render the retained lines in default colours. A service that sets a
    /// colour once and leaves it on loses it otherwise.
    pen: Vec<u8>,
    /// True once any output at all has been received.
    has_output: bool,
    /// Whether the emulator is the placeholder a release left behind rather
    /// than this store's screen.
    ///
    /// While it is set, `process` keeps `raw` and skips the parser: a released
    /// grid is one nothing is allowed to read until `resize` replays `raw` into
    /// it, so parsing into it is work done for nobody. Distinct from
    /// `replay_pending`, which says only that the next resize must not
    /// short-circuit on an unchanged size.
    released: bool,
}

impl LogStore {
    pub fn new(scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(INITIAL_ROWS, INITIAL_COLS, scrollback),
            scrollback_len: scrollback,
            raw: Vec::new(),
            keep_lines: keep_lines_for(scrollback, INITIAL_ROWS),
            lines: 0,
            pending_cr: false,
            container: String::new(),
            attach: 0,
            pen: Vec::new(),
            replay_pending: false,
            has_output: false,
            released: false,
        }
    }

    pub fn has_output(&self) -> bool {
        self.has_output
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.live_screen()
    }

    /// The screen, for a caller entitled to read it.
    ///
    /// The assertion pins the invariant the release rests on.
    /// `release_offscreen_stores` releases every store no pane slot holds, and
    /// a released store's grid is empty until `resize` replays `raw` into it --
    /// so a reader that reaches a store by any route other than `App::pane_key`
    /// draws a blank pane instead of the service's output. Skipping the parse
    /// is what turned that from wasted work into a wrong pane, so every read
    /// goes through here and a stray one fails in any debug build, the test
    /// suite included, rather than quietly on a user's screen.
    fn live_screen(&self) -> &vt100::Screen {
        debug_assert!(
            !self.released,
            "read the screen of a released store: readers must resolve through \
             `App::pane_key`, the set `release_offscreen_stores` leaves alone"
        );
        self.parser.screen()
    }

    /// The screen, for a caller entitled to move its scroll offset. Held to the
    /// same rule as `live_screen`, and for the same reason.
    fn live_screen_mut(&mut self) -> &mut vt100::Screen {
        debug_assert!(
            !self.released,
            "moved the scroll offset of a released store: readers must resolve \
             through `App::pane_key`, the set `release_offscreen_stores` leaves alone"
        );
        self.parser.screen_mut()
    }

    /// Hands the store to one attach to one container, ending the row and the
    /// emulator state the previous one left behind.
    ///
    /// A store is keyed on `(service, replica)` and is meant to outlive the
    /// container: that shared buffer is what lets a pane keep its history when
    /// compose recreates the container behind it, which is why #46 turned down
    /// keying it on the ID. But compose gives the replacement the same
    /// `container-number`, so the key outlives the container -- and a `web-1`
    /// that stopped mid-line leaves the emulator's cursor part way along a row.
    /// Without this the replacement's first chunk continues that row and the
    /// pane shows one line neither container wrote.
    ///
    /// The ID does not answer that on its own, which is what #68 is about. A
    /// container's log task can end while the container keeps running, and the
    /// next resync reattaches the *same* container from `since = ended_at`.
    /// `since` resolves to the second, so the replay can restart the very entry
    /// the cursor is part way along and continue that row with it: `GET /one`
    /// on screen, `GET /one 200` replayed, one row reading
    /// `GET /oneGET /one 200`. Nothing about the container changed, so the pair
    /// is compared rather than the ID alone -- the same pair, for the same
    /// reason, that `LineAssembler::adopt` compares in the fallback.
    ///
    /// It is the *row* that ends here, not the buffer. Nothing is discarded:
    /// the break is written into the stream, so the dead container's tail stays
    /// on screen as its own line and the replacement starts on the next one.
    ///
    /// A row is not all that carries over, though, which is what #72 is about:
    /// the emulator's parse state, its pen and its screen selection are the
    /// dead container's too, and none of them is undone by ending a row -- nor
    /// is its scroll region, which #75 split out and then fixed, nor the
    /// alternate grid's content, cursor and region, which #79 split out again
    /// because the rest of the reset lands on the primary grid and misses them.
    /// [`HANDOVER`] is what resets them and carries the reasoning for each
    /// sequence in it. It goes in ahead of the break rather than after it,
    /// because a container that died inside an OSC, DCS, APC or PM string
    /// leaves the parser in a state that collects C0 controls instead of
    /// executing them -- a break written first is swallowed along with
    /// everything else until the string ends.
    ///
    /// Both writes go through [`LogStore::process`] rather than to the parser
    /// directly, so they are recorded in `raw` as well as on the grid. Anything
    /// else would splice again on the next resize, which replays `raw` from
    /// scratch -- and would do nothing at all for a released store, whose grid
    /// is rebuilt from `raw` when a pane next shows it.
    ///
    /// A bare `\n` because `normalise_newlines` is what decides whether a `\r`
    /// belongs in front of one, and it is holding the state that decision needs:
    /// a chunk that ended on a `\r` already left one in `pending_cr`, and this
    /// has no business adding a second byte no container wrote. The grid would
    /// not show the difference -- a `\r` at column 0 moves nothing -- but `raw`
    /// is replayed, and adopting the ONLCR rule wholesale is what keeps this
    /// from having a second opinion about it.
    ///
    /// The handover write is what makes that decision need help. `process`
    /// re-derives the carry from the bytes it is given, and [`HANDOVER`] ends
    /// on the `l` of an alternate-screen exit rather than a carriage return,
    /// so by the time the break is written the flag describes our bytes rather
    /// than the container's and the `\r` goes back in. The carry is put back where the
    /// container left it so that the two writes together retain exactly the
    /// bytes the break alone used to.
    ///
    /// The scroll offset is carried the same way, and for a related reason:
    /// the handover writes a piece of emulator state that is not the dead
    /// container's to leave behind. [`HANDOVER`]'s `CSI ? 1049 h` reaches
    /// `Screen::enter_alternate_grid`, which calls `grid_mut().set_scrollback(0)`
    /// *before* it sets `MODE_ALTERNATE_SCREEN` -- so the reset lands on the
    /// primary grid, and in vt100 `set_scrollback` is
    /// `scrollback_offset = rows.min(scrollback.len())`, a scroll position and
    /// not a capacity. In this application that position is the reader's, and
    /// a recreate is not a reason to move them:
    /// `a_recreate_does_not_move_a_scrolled_up_reader` is the assertion, and
    /// it fails on the handover alone.
    ///
    /// Carrying it by hand rather than in bytes is what #69's constraint
    /// allows here and not elsewhere. The offset is app state -- nothing in
    /// `raw` encodes it, `resize` reconstructs it by hand across a replay for
    /// exactly that reason, and a released store's is discarded outright by
    /// `release`. So there is nothing for the replay to disagree with: a
    /// resize rebuilds the grid from `raw` and then sets the offset itself,
    /// and the handover's clobber is not in `raw` to be replayed either way.
    ///
    /// Skipped while released, because there is nothing to undo. `process`
    /// does not feed the parser at all in that state, so no `CSI ? 1049 h`
    /// ever runs against the placeholder, and the placeholder is a grid
    /// `live_screen_mut` exists to keep readers out of. The restore is exact
    /// rather than approximate on the live path: the handover carries no `\n`,
    /// so no row evicts while it is parsed and `scrollback.len()` is the same
    /// on both sides of it. The break that may follow is left alone, because
    /// there vt100's own advance is the correct answer -- the same one
    /// `process` deliberately does not compensate for.
    ///
    /// `raw`'s last byte is what says whether there is a row to end, because
    /// trimming only ever cuts from the front -- so the last byte of `raw` is
    /// the last byte the store received, and "has anything arrived since the
    /// last newline" is the same question `LineAssembler` asks of `partial`.
    /// The same question and, having twice been described here as merely a
    /// similar one, the same answer at every input: `push`'s cap flush runs only
    /// while strictly more bytes remain than it takes, so it always puts a
    /// remainder back and never leaves `partial` empty part way along a line.
    /// What `MAX_PARTIAL` changes is what the fallback *prints* -- a run past
    /// the cap is emitted in pieces, none of them a line anyone wrote -- not
    /// whether it believes a line is open.
    ///
    /// An empty `raw` is a store nothing has been written to, which has no row
    /// to end and must not be given a blank one -- nor a handover, which would
    /// be the first bytes the store ever received and would set `has_output`,
    /// replacing the pane's "waiting" placeholder with a blank screen before
    /// any container had written a byte. It has no emulator state to reset
    /// either, having parsed nothing. `raw` cannot empty any other way:
    /// `trim_point` never cuts the whole buffer away, which is the same fact
    /// `release` relies on.
    ///
    /// The predicate over-approximates in one direction, deliberately. A
    /// container whose last bytes after its final newline are escape-only -- a
    /// `\x1b[0m` or a cursor-show on the way out -- has an empty row on screen
    /// but a non-newline last byte, so the recreate costs a blank row and a
    /// line of the retention budget. Telling that apart would mean parsing
    /// `raw`'s tail rather than looking at one byte of it, and the fallback
    /// makes exactly the same call: `LineAssembler` holds those same bytes in
    /// `partial` and prints them as their own prefixed line. Diverging here to
    /// save a blank row would be the two paths disagreeing about where a line
    /// ends, which is the thing #50 and #60 are both about.
    pub fn adopt(&mut self, container: &str, attach: u64) {
        if self.container == container && self.attach == attach {
            return;
        }
        self.container.clear();
        self.container.push_str(container);
        self.attach = attach;
        // Read before anything is written, because the handover appends to
        // `raw` and would otherwise be the last byte this asks about.
        let Some(last) = self.raw.last().copied() else {
            return;
        };
        let carry = self.pending_cr;
        // `None` while released: nothing is fed to the placeholder, so nothing
        // moves its offset, and reading it would be reading a grid
        // `live_screen` forbids.
        let offset = (!self.released).then(|| self.scroll_offset());
        self.process(HANDOVER);
        if let Some(offset) = offset {
            self.live_screen_mut().set_scrollback(offset);
        }
        self.pending_cr = carry;
        if last != b'\n' {
            self.process(b"\n");
        }
    }

    /// Feeds raw container output to the emulator, or past it while released.
    ///
    /// A view at offset 0 keeps tailing; a view scrolled up stays on the content
    /// it is showing. Both come from `vt100`, which advances the scroll offset
    /// as rows evict into scrollback, so nothing here needs to compensate.
    ///
    /// Doing that by hand was worse than doing nothing. The correction only had
    /// a signal to work from while the buffer was filling, and once it was full
    /// it overwrote the emulator's own -- correct -- offset, producing exactly
    /// the drift it was meant to prevent.
    pub fn process(&mut self, bytes: &[u8]) {
        // An empty write is not output: marking it as such would replace the
        // "waiting" placeholder with a blank pane.
        if bytes.is_empty() {
            return;
        }
        let normalised = self.normalise_newlines(bytes);
        self.retain(&normalised);
        // Everything a replay needs was just updated -- `raw`, `pen`,
        // `pending_cr`, and `has_output` below -- so while the emulator is a
        // released placeholder the parse is pure cost: nothing may read that
        // grid, and the resize that ends the release rebuilds it from `raw`
        // regardless of what was fed to it in the meantime.
        if !self.released {
            self.parser.process(&normalised);
        }
        self.has_output = true;
    }

    /// Turns a lone `\n` into `\r\n`, the way a terminal driver's ONLCR would.
    ///
    /// Container logs are LF-terminated. Fed to the emulator raw, a bare `\n`
    /// moves the cursor down without returning it to column 0, so every line
    /// starts where the last one ended and the output walks off to the right.
    ///
    /// The carry flag matters: output arrives in arbitrary chunks, so a `\r` can
    /// end one and its `\n` begin the next. Deciding per chunk would insert a
    /// spurious `\r` at that seam.
    fn normalise_newlines(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut prev_cr = self.pending_cr;
        for &byte in input {
            if byte == b'\n' && !prev_cr {
                out.push(b'\r');
            }
            out.push(byte);
            prev_cr = byte == b'\r';
        }
        self.pending_cr = prev_cr;
        out
    }

    /// Keeps the output needed to rewrap on resize.
    ///
    /// Trimming scans the buffer, so it runs once the surplus is worth the
    /// scan rather than on every write.
    fn retain(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        self.lines += bytecount(bytes);

        let comfortable = self.lines <= self.keep_lines.saturating_mul(2);
        if comfortable && self.raw.len() <= MAX_RAW_BYTES {
            return;
        }

        let cut = self.trim_point();
        if cut == 0 {
            return;
        }
        // The parse is what a trim costs, and a trim drops about as many lines
        // as were ingested since the last one -- so amortised it is one scratch
        // parse per line of output, for every store, on-screen or off. Widening
        // the hysteresis band above does not touch that: it trims less often
        // and drops more each time, leaving the bytes parsed per byte ingested
        // where they were.
        //
        // Skipping it is sound *here* in a way it would not be inside
        // `pen_after`: `self.pen` is only ever that function's own
        // `attributes_formatted` output or the empty vector it starts as, both
        // of which leave vte in its ground state, so bytes that cannot move the
        // pen cannot be swallowed as somebody else's parameters either. And
        // `self.pen` already spells out where the pen stands, so there is
        // nothing to recompute.
        //
        // `bench_the_pen_carried_across_a_trim` measures it on 61-byte log
        // lines, in release, on one machine: over a plain dropped prefix the
        // scan costs 0.03 us per line against 0.7-0.8 for the parse it
        // replaces, and a released store's whole ingest falls from 1.2-1.4 us
        // per line to 0.33-0.36. For output with SGR in it neither figure
        // moves: the scan stops at the first escape byte and the parse runs.
        if can_move_the_pen(&self.raw[..cut]) {
            self.pen = pen_after(&self.pen, &self.raw[..cut]);
        }
        self.lines -= bytecount(&self.raw[..cut]);
        // `drain` would leave the old capacity behind: one oversized chunk can
        // hold many times the ceiling for the life of the store. `split_off`
        // hands back a right-sized buffer and frees the original.
        let tail = self.raw.split_off(cut);
        self.raw = tail;
    }

    /// Where to cut so the newest `keep_lines` lines survive, without letting
    /// the buffer past its ceiling.
    fn trim_point(&self) -> usize {
        let mut seen = 0;
        let mut cut = 0;
        for (index, byte) in self.raw.iter().enumerate().rev() {
            if *byte == b'\n' {
                seen += 1;
                if seen > self.keep_lines {
                    cut = index + 1;
                    break;
                }
            }
        }

        if self.raw.len() - cut <= MAX_RAW_BYTES {
            return cut;
        }

        // Over the ceiling even after keeping only the budgeted lines, so the
        // lines themselves are enormous or there are none at all. Cut to the
        // ceiling, preferring a line boundary; landing mid-sequence garbles at
        // most the first replayed row, which beats losing the pane.
        let floor = self.raw.len() - MAX_RAW_BYTES;
        self.raw[floor..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|offset| floor + offset + 1)
            // A line boundary at the very end would cut everything away, which
            // is the one outcome worse than a garbled first row.
            .filter(|cut| *cut < self.raw.len())
            .unwrap_or(floor)
    }

    /// Resizes the emulated terminal to the pane's inner area.
    ///
    /// Any size change rebuilds the parser and replays the retained bytes.
    ///
    /// A width change needs it because `vt100` keeps rows at the width they
    /// arrived at and will not rewrap them. A height change needs it because
    /// `Grid::set_size` shrinks its row vector from the end, discarding the
    /// newest lines rather than moving them into scrollback -- which left a
    /// pane unable to reach its own tail, with `End` powerless because the
    /// offset was already at the bottom of what remained.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        // Straight to the parser rather than through `live_screen`: this is the
        // one place entitled to look at a released grid, because ending the
        // release is what it is for.
        let (cur_rows, cur_cols) = self.parser.screen().size();

        // A pending replay has to defeat this, or a resize back to a size that
        // was applied without one would short-circuit and never replay at all.
        if (cur_rows, cur_cols) == (rows, cols) && !self.replay_pending {
            return;
        }

        // More visible rows means more history to be able to reproduce.
        self.keep_lines = keep_lines_for(self.scrollback_len, rows);

        let old_offset = self.parser.screen().scrollback();

        if self.raw.is_empty() && self.has_output {
            // Not reachable for a released store: `release` refuses this state,
            // and `raw` never empties again once it has been filled. Asserted
            // rather than argued, because the screen kept here is the released
            // placeholder if it ever is.
            debug_assert!(
                !self.released,
                "a released store reached the branch that keeps its grid"
            );
            // Rebuilding from nothing would blank a pane that has content.
            // Keep what is on screen and try again on the next resize.
            self.parser.screen_mut().set_size(rows, cols);
            self.replay_pending = true;
        } else {
            let mut rebuilt = vt100::Parser::new(rows, cols, self.scrollback_len);
            rebuilt.process(&self.pen);
            rebuilt.process(&self.raw);
            self.parser = rebuilt;
            self.replay_pending = false;
        }
        // The rebuild is the only branch a released store can reach, but the
        // clear covers both, on the other branch's own terms: the screen it
        // keeps is one the pane goes on rendering, so a flag left set there
        // would stop `process` feeding a grid that is on screen, and every
        // read of it would trip `live_screen`'s assertion.
        self.released = false;

        // Losing height moves the bottom of the window up under a scrolled-up
        // reader, so pull the offset back by the rows lost. Anything else keeps
        // its position.
        let target = if rows < cur_rows && old_offset > 0 {
            old_offset.saturating_sub((cur_rows - rows) as usize)
        } else {
            old_offset
        };
        self.parser.screen_mut().set_scrollback(target);
    }

    /// Drops the emulator, keeping the bytes needed to rebuild it.
    ///
    /// A filled grid is `(scrollback + rows) x cols` cells of 32 bytes each --
    /// 2.5 MB at the 24x80 default, 6.4 MB at 50x200, 16.2 MB at 60x500 --
    /// while the raw bytes it was built from are 75-220 kB for typical log
    /// lines. A service no pane is showing is holding the larger figure for a
    /// grid nothing reads, so this releases it and leaves `resize` to replay.
    ///
    /// A store that has taken a handover holds a second, smaller grid as well:
    /// [`HANDOVER`]'s `CSI ? 1049 h` allocates the alternate one, at
    /// `rows x cols` with no scrollback behind it, which is 2.4% to 5.7% on
    /// top of the figures above. Dropping the parser drops that too, so it
    /// does not change what this is for -- only how much of it there is.
    ///
    /// The replay is the same work a first view already does -- a store is
    /// created at the default geometry and `resize` replays it the moment it
    /// lands in a pane of any other size -- but it is done once per view
    /// rather than once per session.
    ///
    /// What comes back is what a `resize` would have rebuilt, which is all of
    /// the history while the *line* budget is what binds. Under `MAX_RAW_BYTES`
    /// it is less: escape-heavy output spends raw bytes without spending
    /// emulator rows, so `raw` falls short of `keep_lines` and the replay
    /// cannot refill the grid. A resize has always paid that; releasing moves
    /// the cost from "when the pane changes size" to "whenever the pane
    /// closes".
    ///
    /// What stops is the feed into this store's own grid, not the ingest.
    /// Output keeps arriving for a closed pane and keeps being retained, so an
    /// off-screen service still costs the bytes and the line accounting. It
    /// costs the scratch parse `retain` carries `pen` forward with as well, but
    /// only over a dropped prefix with an escape or non-ASCII byte somewhere in
    /// it: plain ASCII output fails `can_move_the_pen` and skips it. Non-ASCII
    /// is not exotic -- one `e`-acute or box-drawing character in a log line
    /// puts that trim back on the full parse. What it stops paying for either
    /// way is a parse into a grid nothing will read.
    ///
    /// Scroll position does not survive, and should not: the offset counts rows
    /// back from the bottom, and output kept arriving while the pane was
    /// closed, so the row it named is no longer the row the reader left.
    ///
    /// Everything else is left alone, `pending_cr` included. The byte stream
    /// feeding `raw` is continuous across a release -- a chunk boundary can
    /// still fall between a `\r` and its `\n` -- so the carry is mid-stream
    /// state a release has no business in, exactly as `pen` and `lines` are.
    /// Clearing it happens to be invisible today, but only because a doubled
    /// carriage return is; that is a fact about the emulator, not a licence.
    pub fn release(&mut self) {
        // Housekeeping runs on every service poll, so most calls land on a
        // store that is already released. Nothing has been parsed into the
        // placeholder since -- that is the point of it -- so building a second
        // one would allocate an identical empty grid every few seconds.
        if self.released {
            return;
        }
        // Rebuilding from nothing would blank a pane that has content, which is
        // the same hazard `resize` keeps the screen for rather than replaying.
        if self.raw.is_empty() && self.has_output {
            return;
        }
        self.parser = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
        self.released = true;
        // A pane floored at the minimum geometry would otherwise short-circuit
        // the resize and render this empty grid as the service's output.
        self.replay_pending = true;
    }

    /// Rows scrolled back from the bottom. `0` means tailing.
    pub fn scroll_offset(&self) -> usize {
        self.live_screen().scrollback()
    }

    pub fn scroll_up(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_add(lines as usize);
        self.live_screen_mut().set_scrollback(target);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_sub(lines as usize);
        self.live_screen_mut().set_scrollback(target);
    }

    pub fn scroll_to_top(&mut self) {
        // vt100 clamps to the number of retained rows.
        self.live_screen_mut().set_scrollback(usize::MAX);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.live_screen_mut().set_scrollback(0);
    }

    /// The full retained buffer as plain text, for clipboard copy.
    ///
    /// Walks the scrollback from the top down. Each window overlaps the next by
    /// all but `advance` rows, so only the first `advance` rows of each window
    /// are new — taking the whole window would duplicate content whenever the
    /// scrollback depth is not an exact multiple of the pane height.
    pub fn all_text(&mut self) -> String {
        let saved = self.scroll_offset();
        let (rows, cols) = self.live_screen().size();

        self.live_screen_mut().set_scrollback(usize::MAX);
        let mut offset = self.live_screen().scrollback();

        let mut lines: Vec<String> = Vec::new();
        loop {
            self.live_screen_mut().set_scrollback(offset);
            let window = self.live_screen().rows(0, cols);
            if offset == 0 {
                lines.extend(window);
                break;
            }
            let advance = (rows as usize).min(offset);
            lines.extend(window.take(advance));
            offset -= advance;
        }

        self.live_screen_mut().set_scrollback(saved);

        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Visible rows as plain strings.
    ///
    /// Test-only: rendering blits cells straight from the emulator, and the
    /// scrollbar measures the screen's geometry, so nothing in the running
    /// program needs the text materialised.
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        let (_, cols) = self.live_screen().size();
        self.live_screen().rows(0, cols).collect()
    }

    /// The parser's grid whatever state it is in, released included.
    ///
    /// Test-only, and the one deliberate exception to `live_screen`'s rule: the
    /// tests that prove a store *was* released have to look at the grid the
    /// release left behind. Nothing in the running program may.
    #[cfg(test)]
    pub fn released_grid(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view is tailing when it sits at the bottom of the buffer.
    fn tailing(store: &LogStore) -> bool {
        store.scroll_offset() == 0
    }

    /// Visible rows with the blank padding removed.
    fn non_empty(store: &LogStore) -> Vec<String> {
        store
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn store_with(lines: usize) -> LogStore {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..lines {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        s
    }

    /// The stream layer bounds how much one write carries, so output that
    /// used to arrive in a single write can now arrive cut at an offset with
    /// no relation to its content: mid escape sequence, between a `\r` and its
    /// `\n`, inside a multi-byte character. That is only safe because the
    /// emulator's parse state survives between writes, which is verified here
    /// rather than assumed. The `pending_cr` carry needs its own test: a
    /// duplicated `\r` is invisible to a terminal, so a broken carry would
    /// slip past this comparison.
    ///
    /// Cutting every seven bytes is the harsher version of a 64 KiB cut: it
    /// puts a seam inside every construct in the input instead of one seam
    /// somewhere in it.
    #[test]
    fn output_renders_the_same_however_it_was_cut_up() {
        let mut whole = LogStore::new(DEFAULT_SCROLLBACK);
        whole.resize(10, 40);
        let mut split = LogStore::new(DEFAULT_SCROLLBACK);
        split.resize(10, 40);

        let mut input = Vec::new();
        for i in 0..200 {
            let colour = 31 + i % 7;
            input.extend_from_slice(format!("\x1b[{colour}mrow {i} k\u{e9}\r\n").as_bytes());
        }

        whole.process(&input);
        for piece in input.chunks(7) {
            split.process(piece);
        }

        // Formatted contents, not plain rows: a cut inside an SGR sequence
        // loses the colour rather than the text, which plain rows would miss.
        assert_eq!(
            split.screen().contents_formatted(),
            whole.screen().contents_formatted(),
            "the pane renders differently when the output arrives cut up"
        );
        assert_eq!(split.all_text(), whole.all_text());
    }

    /// The `pending_cr` carry, which the comparison above cannot see: a `\r`
    /// inserted twice moves the cursor to column zero twice, so a broken carry
    /// is invisible in the rendered output of CRLF input. What it is not
    /// invisible in is *bare* LF input, where a carry wrongly reported as set
    /// suppresses the `\r` this normalisation exists to insert and the line
    /// walks off to the right.
    ///
    /// Splitting a frame is what makes this reachable: container logs are
    /// LF-terminated, so a cut at a piece boundary routinely leaves a bare
    /// `\n` starting the next write.
    #[test]
    fn a_bare_newline_starting_a_write_still_returns_to_column_zero() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);

        // The cut lands between the line and its terminator, so the second
        // write opens on the `\n` with no `\r` anywhere near it.
        s.process(b"first line");
        s.process(b"\nsecond line\n");

        assert_eq!(non_empty(&s), ["first line", "second line"]);
    }

    #[test]
    fn an_empty_write_is_not_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(5, 40);
        s.process(b"");
        assert!(
            !s.has_output(),
            "an empty write should leave the pane waiting"
        );
    }

    #[test]
    fn a_new_store_reports_no_output() {
        let s = LogStore::new(DEFAULT_SCROLLBACK);
        assert!(!s.has_output());
    }

    #[test]
    fn processing_marks_output_and_tails() {
        let s = store_with(3);
        assert!(s.has_output());
        assert!(tailing(&s), "a fresh store should be pinned to the bottom");
    }

    #[test]
    fn newest_lines_are_visible_when_tailing() {
        let s = store_with(50);
        let visible = s.visible_lines();
        assert!(
            visible.iter().any(|l| l.contains("line 49")),
            "expected the newest line, got: {visible:?}"
        );
        assert!(!visible.iter().any(|l| l.contains("line 0")));
    }

    #[test]
    fn scrolling_up_then_down_returns_to_tailing() {
        let mut s = store_with(50);
        s.scroll_up(5);
        assert_eq!(s.scroll_offset(), 5);
        assert!(!tailing(&s));
        s.scroll_down(5);
        assert!(tailing(&s));
    }

    #[test]
    fn scrolling_down_past_the_bottom_clamps() {
        let mut s = store_with(50);
        s.scroll_up(3);
        s.scroll_down(999);
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn scrolling_up_past_the_top_clamps_to_retained_rows() {
        let mut s = store_with(50);
        s.scroll_up(u16::MAX);
        let max = s.scroll_offset();
        assert!(max > 0 && max < 50, "expected a bounded top, got {max}");
        // Already at the top: going further changes nothing.
        s.scroll_up(10);
        assert_eq!(s.scroll_offset(), max);
    }

    #[test]
    fn top_and_bottom_helpers_reach_the_extremes() {
        let mut s = store_with(50);
        s.scroll_to_top();
        assert!(!tailing(&s));
        s.scroll_to_bottom();
        assert!(tailing(&s));
    }

    #[test]
    fn a_tailing_view_keeps_tailing_as_output_arrives() {
        let mut s = store_with(20);
        s.process(b"newest\r\n");
        assert!(tailing(&s));
        assert!(s.visible_lines().iter().any(|l| l.contains("newest")));
    }

    #[test]
    fn a_scrolled_view_stays_on_the_same_content_as_output_arrives() {
        let mut s = store_with(20);
        s.scroll_up(4);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();

        s.process(b"newest\r\n");

        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(
            before, after,
            "a scrolled-up reader should not be dragged along by new output"
        );
        assert_eq!(s.scroll_offset(), 5, "the offset absorbs the new row");
    }

    #[test]
    fn a_scrolled_view_holds_still_even_once_the_buffer_is_full() {
        // Regression: hand-rolled anchoring used to overwrite the emulator's own
        // offset here, which drifted the view and was mistaken for a limitation
        // of the emulator.
        let mut s = LogStore::new(64);
        s.resize(10, 40);
        for i in 0..500 {
            s.process(format!("line {i}\n").as_bytes());
        }
        s.scroll_up(5);
        let before = non_empty(&s);
        for i in 0..20 {
            s.process(format!("burst {i}\n").as_bytes());
        }
        assert_eq!(
            before,
            non_empty(&s),
            "a scrolled-up reader should not be dragged along by new output"
        );
    }

    #[test]
    fn a_scrolled_view_absorbs_a_burst_of_output() {
        let mut s = store_with(30);
        s.scroll_up(6);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        for i in 0..25 {
            s.process(format!("burst {i}\r\n").as_bytes());
        }
        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(before, after, "content should hold still through a burst");
    }

    // ---- newline normalisation (#11) ----

    #[test]
    fn bare_line_feeds_start_at_column_zero() {
        // Container logs are LF-terminated. Fed raw to the emulator, each line
        // would start where the last one ended and walk off to the right.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"first line\nsecond line\nthird line\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["first line", "second line", "third line"]);
    }

    #[test]
    fn carriage_return_line_feed_is_left_alone() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_crlf_split_across_chunks_is_not_treated_as_a_bare_newline() {
        // The seam case: deciding per chunk would insert a spurious \r here.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r");
        s.process(b"\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_line_split_mid_word_across_chunks_still_reads_correctly() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"data");
        s.process(b"base ready\n");
        assert!(s
            .visible_lines()
            .iter()
            .any(|l| l.trim_end() == "database ready"));
    }

    // ---- reflow on resize (#8) ----

    #[test]
    fn widening_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        // At 40 columns it needs two rows.
        assert_eq!(non_empty(&s).len(), 2);

        s.resize(10, 100);
        // At 100 it fits on one, which only happens if history was reparsed.
        let after = non_empty(&s);
        assert_eq!(after.len(), 1, "history should rewrap, got {after:?}");
        assert_eq!(after[0], long);
    }

    #[test]
    fn narrowing_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 100);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        assert_eq!(non_empty(&s).len(), 1);

        s.resize(10, 40);
        assert_eq!(non_empty(&s).len(), 2, "narrowing should rewrap too");
    }

    #[test]
    fn shrinking_height_keeps_the_newest_lines_reachable() {
        // vt100's Grid::set_size shrinks its row vector from the end, so the
        // newest lines were discarded outright. The pane could not reach its own
        // tail, and End was powerless because the offset was already 0.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(24, 100);
        for i in 0..400 {
            s.process(format!("L{i:03}\n").as_bytes());
        }
        assert_eq!(non_empty(&s).last().map(String::as_str), Some("L399"));

        s.resize(12, 100);
        s.scroll_to_bottom();
        assert_eq!(
            non_empty(&s).last().map(String::as_str),
            Some("L399"),
            "the tail must survive a height reduction"
        );
    }

    #[test]
    fn growing_height_reveals_more_without_losing_the_tail() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 100);
        for i in 0..400 {
            s.process(format!("L{i:03}\n").as_bytes());
        }
        s.resize(30, 100);
        let visible = non_empty(&s);
        assert_eq!(visible.last().map(String::as_str), Some("L399"));
        assert!(visible.len() > 10, "a taller pane should show more rows");
    }

    #[test]
    fn a_height_only_change_keeps_the_content() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"stable line\n");
        let before = non_empty(&s);
        s.resize(20, 40);
        assert_eq!(before, non_empty(&s));
    }

    #[test]
    fn scroll_position_survives_a_widen() {
        let mut s = store_with(60);
        s.scroll_up(5);
        s.resize(10, 100);
        assert_eq!(s.scroll_offset(), 5, "widening should not move the reader");
    }

    #[test]
    fn losing_height_pulls_a_scrolled_reader_back_by_the_rows_lost() {
        let mut s = store_with(60);
        s.scroll_up(10);
        s.resize(6, 40); // same width, four rows shorter
        assert_eq!(s.scroll_offset(), 6);
    }

    #[test]
    fn a_tailing_reader_keeps_tailing_across_a_resize() {
        let mut s = store_with(60);
        assert!(tailing(&s));
        s.resize(10, 100);
        assert!(tailing(&s), "a reader at the bottom should stay there");
    }

    #[test]
    fn the_retained_buffer_is_bounded_and_cut_at_a_line_boundary() {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        for i in 0..5_000 {
            s.process(format!("line {i} with some padding to take up room\n").as_bytes());
        }
        assert!(
            s.lines <= s.keep_lines * 2,
            "retained lines must stay bounded"
        );
        assert!(
            s.raw.starts_with(b"line "),
            "a trim must land on a line boundary, not mid-sequence"
        );
        // And it still rewraps correctly after trimming.
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("line 4999")));
    }

    #[test]
    fn output_without_newlines_survives_a_resize() {
        // A \r-driven progress bar emits no newline at all, so the retained
        // buffer can pass its cap with no line boundary to trim at.
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(b"visible content\n");
        let blob = vec![b'X'; MAX_RAW_BYTES + 1];
        s.process(&blob);

        assert!(
            !s.raw.is_empty(),
            "an oversized record must not discard every retained byte"
        );
        s.resize(5, 100);
        assert!(
            !non_empty(&s).is_empty(),
            "the pane went blank after a resize"
        );
    }

    #[test]
    fn a_skipped_replay_is_retried_rather_than_lost() {
        // Applying a size without rewrapping used to make the early-return
        // treat that size as done, so the pane never rewrapped at that width
        // again -- silently reintroducing the bug this all exists to fix.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());

        s.raw.clear();
        s.resize(10, 100);
        assert!(s.replay_pending, "the replay should be recorded as owed");
        assert_eq!(non_empty(&s).len(), 2, "old wrapping is kept, not blanked");

        // Buffer refills; the same width must now actually rewrap.
        s.process(format!("{long}\n").as_bytes());
        s.resize(10, 100);
        assert!(!s.replay_pending);
        assert!(
            non_empty(&s).iter().any(|l| l == &long),
            "the retried rewrap should have unwrapped the line"
        );
    }

    #[test]
    fn a_resize_never_blanks_a_pane_that_has_content() {
        // Belt and braces for the case above: even if the retained buffer were
        // empty, what is already on screen must survive a width change.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(5, 40);
        s.process(b"still here\n");
        s.raw.clear();
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("still here")));
    }

    /// Fills past the retained cap so a trim is guaranteed, then rewraps.
    fn trim_then_resize(setup: &[u8]) -> LogStore {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(setup);
        let filler = "y".repeat(4096);
        for _ in 0..200 {
            s.process(format!("{filler}\n").as_bytes());
        }
        assert!(!s.pen.is_empty(), "a trim should have happened");
        s.resize(5, 100);
        s
    }

    #[test]
    fn styling_set_before_the_retained_window_survives_a_resize() {
        // The bytes that set the colour are trimmed away, so a replay would
        // render everything in the default colour without the carried pen.
        let s = trim_then_resize(b"\x1b[31m");
        assert_eq!(
            s.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn a_carried_pen_covers_attributes_and_backgrounds() {
        let s = trim_then_resize(b"\x1b[1m\x1b[4m\x1b[44m");
        let cell = s.screen().cell(0, 0).unwrap();
        assert!(cell.bold(), "bold should survive");
        assert!(cell.underline(), "underline should survive");
        assert_eq!(cell.bgcolor(), vt100::Color::Idx(4));
    }

    #[test]
    fn a_carried_pen_covers_dim() {
        let s = trim_then_resize(b"\x1b[2m");
        assert!(s.screen().cell(0, 0).unwrap().dim(), "dim should survive");
    }

    #[test]
    fn a_carried_pen_covers_bright_indexed_and_rgb_colours() {
        let bright = trim_then_resize(b"\x1b[91m");
        assert_eq!(
            bright.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(9)
        );

        let indexed = trim_then_resize(b"\x1b[38;5;200m");
        assert_eq!(
            indexed.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(200)
        );

        let rgb = trim_then_resize(b"\x1b[38;2;10;20;30m");
        assert_eq!(
            rgb.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Rgb(10, 20, 30)
        );
    }

    /// The styling a trim must carry is not always at the front of what it
    /// drops. A service that runs plain for a while and then turns a colour on
    /// has its SGR buried in the middle of the dropped prefix, which is where a
    /// scan bounded to a window at either end of it would lose it -- the
    /// heuristic `can_move_the_pen` deliberately is not.
    ///
    /// `styling_set_before_the_retained_window_survives_a_resize` and its
    /// neighbours all set the colour before the first byte of output, so the
    /// escape sits at offset zero in every prefix they drop.
    #[test]
    fn styling_set_part_way_through_the_dropped_prefix_survives_a_trim() {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        let filler = "y".repeat(4096);
        for _ in 0..10 {
            s.process(format!("{filler}\n").as_bytes());
        }
        s.process(b"\x1b[31m");
        for _ in 0..200 {
            s.process(format!("{filler}\n").as_bytes());
        }
        assert!(!s.pen.is_empty(), "a trim should have happened");
        s.resize(5, 100);
        assert_eq!(
            s.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1),
            "a colour set inside the dropped prefix was lost"
        );
    }

    #[test]
    fn styling_that_was_reset_before_the_trim_is_not_resurrected() {
        // The pen must reflect the state at the trim boundary, not every
        // sequence that ever appeared.
        let s = trim_then_resize(b"\x1b[31m\x1b[0m");
        assert_eq!(
            s.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
    }

    /// The warrant for the guard in `retain`, asked of the crate rather than
    /// asserted here: with every attribute `vt100` knows how to serialise set,
    /// no seven-bit byte but `ESC` moves the pen.
    ///
    /// Two states rather than one, because "every attribute at once" is not a
    /// thing the crate can be put into. It keeps bold and dim in a single
    /// field, so `CSI 1;2 m` leaves dim set and bold clear -- and a loop run
    /// against one everything-at-once sequence would quietly be testing six
    /// attributes while claiming seven. That is this file's own cautionary
    /// tale: an attribute silently missing from the set under test is why
    /// `pen_after` defers to `attributes_formatted` instead of a hand-written
    /// list. Bold is also what the checked-in `codes = [1]` regression seed is
    /// about, so it is the one least worth losing. The premise is pinned below
    /// rather than assumed, so a `vt100` that separates the two fails here
    /// instead of leaving a stale comment.
    ///
    /// Each byte is fed repeatedly and interleaved with text, so a byte that
    /// needed a second one to take vte out of its ground state would still get
    /// there. `0x80` and above are outside the claim and outside the guard.
    ///
    /// This is what makes the guard the same kind of thing as
    /// `attributes_formatted` rather than the hand-enumeration that dropped
    /// `dim`: if a later `vt100` gives one of these bytes an effect on
    /// attributes, this fails instead of the pen quietly going wrong.
    #[test]
    fn no_seven_bit_byte_but_escape_moves_the_pen() {
        let bold = b"\x1b[1;3;4;7;31;46m".as_slice();
        let dim = b"\x1b[2;3;4;7;31;46m".as_slice();

        // The premise for splitting them: between the two, every attribute the
        // crate exposes is set somewhere, and neither sequence sets both halves
        // of the bold/dim field.
        let attrs = |seq: &[u8]| {
            let mut scratch = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
            scratch.process(seq);
            scratch.process(b"x");
            let cell = scratch.screen().cell(0, 0).unwrap();
            (
                cell.bold(),
                cell.dim(),
                cell.italic(),
                cell.underline(),
                cell.inverse(),
                cell.fgcolor(),
                cell.bgcolor(),
            )
        };
        let colours = (vt100::Color::Idx(1), vt100::Color::Idx(6));
        assert_eq!(
            attrs(bold),
            (true, false, true, true, true, colours.0, colours.1),
            "the bold half of the pair is not set, so the loop below does not cover it"
        );
        assert_eq!(
            attrs(dim),
            (false, true, true, true, true, colours.0, colours.1),
            "the dim half of the pair is not set, so the loop below does not cover it"
        );

        for set in [bold, dim] {
            for byte in 0u8..=0x7f {
                if byte == 0x1b {
                    continue;
                }
                let mut scratch = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
                scratch.process(set);
                let before = scratch.screen().attributes_formatted();
                for _ in 0..4 {
                    scratch.process(&[byte]);
                    scratch.process(b"text");
                    scratch.process(&[byte, byte]);
                }
                assert_eq!(
                    scratch.screen().attributes_formatted(),
                    before,
                    "byte {byte:#04x} moved the pen set by {:?}, which the guard in `retain` \
                     says it cannot",
                    String::from_utf8_lossy(set).escape_debug().to_string()
                );
            }
        }
    }

    /// What the guard in `retain` rests on: a dropped prefix that
    /// `can_move_the_pen` clears leaves the pen where it was, so keeping
    /// `self.pen` is what the parse would have produced.
    ///
    /// Compared through `pen_after`'s canonical spelling rather than as raw
    /// bytes, because the pen a store starts with is the empty vector while the
    /// parse would render the same default state as `ESC [ m`. Those are the
    /// same pen and replay identically; only their spelling differs.
    #[test]
    fn a_dropped_prefix_that_cannot_move_the_pen_leaves_it_alone() {
        let pens: [&[u8]; 6] = [
            b"",
            b"\x1b[31m",
            b"\x1b[1;2;3;4;7m",
            b"\x1b[38;5;200m",
            b"\x1b[48;2;10;20;30m",
            b"\x1b[91;42m",
        ];
        let plain: [&[u8]; 4] = [
            b"",
            b"plain output\r\n",
            b"a line\r\nand another one\r\n",
            // Every C0 control but ESC, which is the half of the guard that
            // has to hold for real log output rather than for a mode.
            b"\x07\x08\x09\x0b\x0c\x0d\x0e\x0f\x7f",
        ];
        for pen in pens {
            // The pen as `retain` holds it: `pen_after`'s own output, or empty.
            let carried = if pen.is_empty() {
                Vec::new()
            } else {
                pen_after(pen, &[])
            };
            for dropped in plain {
                assert!(
                    !can_move_the_pen(dropped),
                    "the guard flagged {:?}, so this case proves nothing",
                    String::from_utf8_lossy(dropped).escape_debug().to_string()
                );
                assert_eq!(
                    pen_after(&carried, dropped),
                    pen_after(&carried, &[]),
                    "pen {:?} moved across dropped bytes {:?}",
                    String::from_utf8_lossy(&carried).escape_debug().to_string(),
                    String::from_utf8_lossy(dropped).escape_debug().to_string(),
                );
            }
        }
    }

    /// The guard is allowed to be conservative and not allowed to be wrong, so
    /// anything carrying an escape byte has to reach the parse whether or not
    /// it turns out to move the pen. The last case does not move it, and is
    /// here because the guard cannot tell.
    #[test]
    fn anything_with_an_escape_byte_in_it_still_reaches_the_parse() {
        let cases: [&[u8]; 4] = [
            b"\x1b[32m",
            b"\x1b[0mplain output\r\n",
            b"line\r\n\x1b[1mline\r\n",
            b"text \xc3\xa9 with a non-ascii byte\r\n",
        ];
        for dropped in cases {
            assert!(
                can_move_the_pen(dropped),
                "{:?} skipped the parse",
                String::from_utf8_lossy(dropped).escape_debug().to_string()
            );
        }
    }

    /// What a trim costs, and what the guard takes off it. Ignored by default:
    /// it is a measurement, not a threshold, and a threshold on a shared
    /// runner would be a flake.
    ///
    /// Run with `cargo test --release -- --ignored --nocapture
    /// bench_the_pen_carried_across_a_trim`. Release matters: the debug figures
    /// are an order of magnitude larger and in a different proportion.
    ///
    /// The minimum of several rounds rather than the mean of one: the mean over
    /// a single burst moved by 40% run to run on the machine this was written
    /// on, while the minimum held to a few percent.
    #[test]
    #[ignore = "measurement, not a threshold"]
    fn bench_the_pen_carried_across_a_trim() {
        use std::hint::black_box;
        use std::time::Instant;

        // One trim's worth of dropped prefix at the default scrollback and a
        // 40-row pane, in the shape a service's logs come in.
        let keep = keep_lines_for(DEFAULT_SCROLLBACK, 40);
        let prefix = |coloured: bool| {
            let mut v = Vec::new();
            for i in 0..keep {
                let line = if coloured && i % 8 == 0 {
                    format!("2026-01-01T00:00:00.000Z \x1b[33mWARN \x1b[0m mod handled id={i}\r\n")
                } else {
                    format!("2026-01-01T00:00:00.000Z INFO  mod handled id={i} status=200\r\n")
                };
                v.extend_from_slice(line.as_bytes());
            }
            v
        };
        let plain = prefix(false);
        let coloured = prefix(true);

        let per_line = |f: &dyn Fn()| {
            let round = || {
                let t = Instant::now();
                for _ in 0..20 {
                    f();
                }
                t.elapsed().as_secs_f64() / 20.0 / keep as f64 * 1e6
            };
            for _ in 0..5 {
                round();
            }
            (0..12).map(|_| round()).fold(f64::MAX, f64::min)
        };

        println!("bytes per line: {}", plain.len() / keep);
        // What the guard replaces, and what it costs, over the same prefix.
        println!(
            "pen_after over a plain prefix                {:.4} us/line",
            per_line(&|| {
                black_box(pen_after(black_box(b"\x1b[33m"), black_box(&plain)));
            })
        );
        println!(
            "can_move_the_pen over the same prefix        {:.4} us/line",
            per_line(&|| {
                black_box(can_move_the_pen(black_box(&plain)));
            })
        );
        println!(
            "pen_after over a coloured prefix             {:.4} us/line",
            per_line(&|| {
                black_box(pen_after(black_box(b"\x1b[33m"), black_box(&coloured)));
            })
        );

        // Whole-store ingest, so the parse sits next to what it is a share of.
        // Released, because that is the store #58 left this as the residual
        // cost of.
        let ingest = |body: &dyn Fn(usize) -> Vec<u8>| {
            let n = 40_000;
            let lines: Vec<Vec<u8>> = (0..n).map(body).collect();
            let mut s = LogStore::new(DEFAULT_SCROLLBACK);
            s.resize(40, 120);
            s.process(b"x\r\n");
            s.release();
            let t = Instant::now();
            for l in &lines {
                s.process(l);
            }
            t.elapsed().as_secs_f64() / n as f64 * 1e6
        };
        // Same reason as `per_line`: one pass over 40,000 lines moved by 20%
        // run to run, which is wider than some of the differences it is here
        // to show.
        let ingest =
            |body: &dyn Fn(usize) -> Vec<u8>| (0..5).map(|_| ingest(body)).fold(f64::MAX, f64::min);
        println!(
            "released ingest, plain output                {:.4} us/line",
            ingest(
                &|i| format!("2026-01-01T00:00:00.000Z INFO  mod handled id={i} status=200\n")
                    .into_bytes()
            )
        );
        println!(
            "released ingest, one line in eight coloured  {:.4} us/line",
            ingest(&|i| if i % 8 == 0 {
                format!("2026-01-01T00:00:00.000Z \x1b[33mWARN \x1b[0m mod handled id={i}\n")
                    .into_bytes()
            } else {
                format!("2026-01-01T00:00:00.000Z INFO  mod handled id={i} status=200\n")
                    .into_bytes()
            })
        );
    }

    #[test]
    fn colour_dense_lines_are_not_lost_by_a_rewrap() {
        // Bytes per visual row depend on the content, not just the width: output
        // that reissues an SGR code per character costs many times a plain row.
        // A bytes-per-row budget silently dropped most of the scrollback here.
        let scrollback = 500;
        let mut s = LogStore::new(scrollback);
        s.resize(20, 80);
        for i in 0..scrollback {
            let mut line = format!("L{i:05} ");
            for c in 0..70 {
                line.push_str(&format!("\x1b[38;5;{}m", 16 + (c % 216)));
                line.push('x');
            }
            s.process(format!("{line}\n").as_bytes());
        }

        s.resize(20, 100);
        s.scroll_to_top();
        let after = non_empty(&s);
        assert!(
            after.iter().any(|l| l.trim_start().starts_with("L00000")),
            "the oldest retained line was lost by the rewrap: {:?}",
            after.first()
        );
    }

    #[test]
    fn a_wide_pane_keeps_its_oldest_row_across_a_width_change() {
        // The retention budget must scale with the pane: at ~1000 columns a
        // fixed per-row guess trimmed rows the parser was still holding, so a
        // rewrap silently dropped the top of the scrollback.
        let scrollback = 200;
        let mut s = LogStore::new(scrollback);
        s.resize(20, 1000);
        for i in 0..scrollback {
            // Rows that fill the width, which is the expensive case.
            s.process(format!("{i:04} {}\n", "w".repeat(980)).as_bytes());
        }

        s.scroll_to_top();
        let oldest = non_empty(&s)
            .first()
            .cloned()
            .expect("something should be on screen at the top");

        s.resize(20, 1100);
        s.scroll_to_top();
        let after = non_empty(&s);
        assert!(
            after.iter().any(|l| l.starts_with(&oldest[..8])),
            "oldest retained row was lost by the rewrap: {:?}",
            after.first()
        );
    }

    #[test]
    fn the_retention_budget_grows_with_the_pane() {
        let mut s = LogStore::new(1000);
        s.resize(20, 80);
        let shallow = s.keep_lines;
        s.resize(60, 1000);
        assert!(
            s.keep_lines > shallow,
            "more visible rows means more history to reproduce: {shallow} -> {}",
            s.keep_lines
        );
    }

    #[test]
    fn an_oversized_chunk_does_not_leave_its_allocation_behind() {
        // Trimming the length is not enough: the buffer would keep the peak
        // capacity for the life of the store.
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(&vec![b'z'; MAX_RAW_BYTES * 2]);
        assert!(
            s.raw.capacity() <= MAX_RAW_BYTES + MAX_RAW_BYTES / 2,
            "retained {} bytes of capacity for a {MAX_RAW_BYTES} byte ceiling",
            s.raw.capacity()
        );
    }

    #[test]
    fn ansi_colour_is_interpreted_not_printed() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"\x1b[31mRED\x1b[0m\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.contains("RED")));
        assert!(
            !visible.iter().any(|l| l.contains("\x1b")),
            "escape sequences should be consumed by the emulator"
        );
        let cell = s.screen().cell(0, 0).unwrap();
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn carriage_returns_rewrite_the_line() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"50%\r100%\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.starts_with("100%")));
    }

    #[test]
    fn resizing_is_idempotent_and_clamps_to_a_safe_floor() {
        let mut s = store_with(5);
        s.resize(20, 60);
        assert_eq!(s.screen().size(), (20, 60));
        // vt100 underflows on very narrow grids, so the floor is a crash guard.
        s.resize(0, 0);
        assert_eq!(s.screen().size(), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn collapsing_to_nothing_does_not_panic_while_replaying() {
        // Regression: rebuilding the parser replays retained output, and
        // replaying into a one-column grid panicked inside vt100.
        let mut s = store_with(40);
        for cols in [1, 2, 5, 19, 20, 21, 200, 1] {
            s.resize(1, cols);
        }
        assert!(s.screen().size().1 >= MIN_COLS);
    }

    #[test]
    fn all_text_does_not_duplicate_lines_at_an_uneven_scrollback_depth() {
        // 45 lines in a 10-row window leaves 35 rows of scrollback, which is not
        // a multiple of the window height - the case where a naive chunked walk
        // re-reads overlapping windows.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..45 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        for i in 0..45 {
            let needle = format!("line {i}");
            let hits = text.lines().filter(|l| l.trim_end() == needle).count();
            assert_eq!(hits, 1, "expected exactly one 'line {i}', found {hits}");
        }
    }

    #[test]
    fn all_text_covers_more_than_the_retained_window() {
        // Beyond DEFAULT_SCROLLBACK the oldest lines are evicted, but everything
        // still retained must appear exactly once.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..1500 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        assert!(
            !text.contains("line 0\n"),
            "oldest lines should have been evicted"
        );
        assert!(text.contains("line 1499"), "newest line must be present");
        assert_eq!(
            text.lines().filter(|l| l.trim_end() == "line 1400").count(),
            1
        );
        assert_eq!(s.scroll_offset(), 0, "copying must not move the view");
    }

    #[test]
    fn all_text_includes_scrolled_off_lines() {
        let mut s = store_with(40);
        let text = s.all_text();
        assert!(text.contains("line 0"), "expected scrollback in the copy");
        assert!(text.contains("line 39"), "expected the newest line too");
        assert!(tailing(&s), "copying must not disturb the scroll position");
    }

    // ---- releasing an off-screen emulator ----

    /// The point of releasing is that the grid goes away, and a grid is the
    /// only part of a store big enough to be worth releasing. Asserting on the
    /// emulated size is the observable stand-in for the allocation: a `10x40`
    /// grid *would* reach 1.3 MB of cells once its 1000-row scrollback filled,
    /// while a `3x20` one with no scrollback cannot pass 2 kB. Rows are
    /// allocated as output arrives, so this 200-line store is far short of that
    /// ceiling -- what is released is the geometry, not the figure.
    #[test]
    fn releasing_shrinks_the_grid_to_nothing() {
        let mut s = store_with(200);
        assert_eq!(s.screen().size(), (10, 40));

        s.release();

        // Through the released-grid peephole: `screen` is now the accessor a
        // pane reads through, and reading it here would trip the assertion that
        // keeps a released store off a user's screen.
        assert_eq!(
            s.released_grid().size(),
            (MIN_ROWS, MIN_COLS),
            "the emulator was kept at its pane size"
        );
    }

    /// Releasing is only safe because the raw bytes outlive the grid. A store
    /// that came back short of what it had would be trading a memory bug for a
    /// data-loss one.
    #[test]
    fn a_released_store_comes_back_with_the_same_content() {
        let mut s = store_with(200);
        let before = non_empty(&s);
        let deep = {
            let mut probe = store_with(200);
            probe.scroll_to_top();
            non_empty(&probe)
        };

        s.release();
        s.resize(10, 40);

        assert_eq!(non_empty(&s), before, "the visible rows changed");
        s.scroll_to_top();
        assert_eq!(non_empty(&s), deep, "the scrollback did not come back");
    }

    /// A pane floored at the emulator's own minimum is a real geometry -- a
    /// terminal barely large enough to draw one. Rebuilding has to happen on
    /// the size comparison alone being unable to tell "released" from
    /// "already this size", or that pane renders an empty grid.
    #[test]
    fn a_release_is_replayed_even_back_to_the_minimum_size() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(MIN_ROWS, MIN_COLS);
        s.process(b"hello\r\n");
        let before = non_empty(&s);

        s.release();
        s.resize(MIN_ROWS, MIN_COLS);

        assert_eq!(non_empty(&s), before, "the pane came back empty");

        // The same resize is where the release has to end, not just where the
        // replay happens: a store that came back with its history but kept
        // skipping the parse would show nothing new until the pane next
        // changed size. This is the geometry where that is easiest to miss,
        // because the resize that has to end the release changes nothing else.
        s.process(b"and more\r\n");
        assert!(
            non_empty(&s).iter().any(|l| l == "and more"),
            "a reopened store never resumed parsing"
        );
    }

    /// Rewrapping on resize is the behaviour several merged fixes exist to
    /// protect, and a release must route through it rather than around it: a
    /// store released at one width and reopened at another has to look exactly
    /// like one that was only ever resized.
    #[test]
    fn a_released_store_still_rewraps_at_the_width_it_reopens_at() {
        let line = "x".repeat(70);
        let mut released = LogStore::new(DEFAULT_SCROLLBACK);
        released.resize(10, 80);
        let mut resized = LogStore::new(DEFAULT_SCROLLBACK);
        resized.resize(10, 80);
        for i in 0..40 {
            let bytes = format!("\x1b[3{}m{i:03} {line}\r\n", 1 + i % 7);
            released.process(bytes.as_bytes());
            resized.process(bytes.as_bytes());
        }

        released.release();
        released.resize(10, 30);
        resized.resize(10, 30);

        assert_eq!(non_empty(&released), non_empty(&resized));
        // Formatted contents, not plain rows: the SGR codes live in `raw` and
        // are re-parsed by the replay, so a release that mangled the styling
        // rather than the text would read as identical rows.
        assert_eq!(
            released.screen().contents_formatted(),
            resized.screen().contents_formatted()
        );
    }

    /// A service that has produced nothing still runs an emulator at the
    /// default geometry and still pays for it. Releasing has to reach those
    /// too, or the idle floor -- one 24x80 grid per silent service -- never
    /// comes down, which is most of what a large stack costs before anyone
    /// opens a pane at all.
    #[test]
    fn a_store_that_has_never_had_output_still_releases() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        assert_eq!(s.screen().size(), (INITIAL_ROWS, INITIAL_COLS));

        s.release();

        assert_eq!(s.released_grid().size(), (MIN_ROWS, MIN_COLS));
    }

    /// A released store is still the live buffer for its service: output keeps
    /// arriving while no pane shows it, and all of it has to be there when the
    /// pane comes back. So the retention budget has to stay sized for the pane
    /// the store will return to, not for the minimal grid it is wearing in the
    /// meantime -- a release that reset it would trim history away in the gap
    /// between one poll and the next.
    #[test]
    fn output_arriving_while_released_is_retained_for_the_pane_it_returns_to() {
        let mut s = LogStore::new(2);
        s.resize(20, 40);
        s.process(b"before\r\n");

        s.release();
        for i in 0..30 {
            s.process(format!("while closed {i}\r\n").as_bytes());
        }
        s.resize(20, 40);

        // Counted over the whole retained buffer rather than the visible rows:
        // the budget is what is under test, and a pane's worth of rows would
        // pass on a buffer trimmed to just under one screen.
        //
        // The budget here is `keep_lines_for(2, 20)`, so 22 lines, one of which
        // is "before" -- about 21 survive. A budget reset to the released
        // geometry would be `keep_lines_for(2, 3)`, so 5. The threshold sits in
        // that gap rather than on either edge, so neither trimming jitter nor a
        // near-miss decides the result.
        let text = s.all_text();
        let kept = text.lines().filter(|l| l.contains("while closed")).count();
        assert!(
            kept >= 15,
            "only {kept} lines survived the closed period, of 30:\n{text}"
        );
    }

    /// A released store keeps ingesting, and the point is that what it ingests
    /// stops costing anything until the service is looked at again. The
    /// emulator is where that cost was: every byte was parsed into a grid the
    /// next resize throws away, and a scrollback budget left intact would have
    /// let it refill besides.
    ///
    /// So the assertion is on the grid rather than on its scroll depth: a
    /// released emulator that is fed nothing at all can neither accumulate
    /// scrollback nor spend anything parsing, which is the stronger of the two
    /// properties and the one this change is for. It has to look at the grid
    /// through the released-grid peephole, because reading a released store
    /// through `screen` is the mistake the whole thing is guarding against.
    ///
    /// The scrollback budget the placeholder is built with is deliberately not
    /// asserted any more, and cannot be: `vt100` allocates scrollback rows as
    /// they arrive, so a grid nothing is parsed into costs nothing whatever
    /// that budget says, and the old check reached it through a `scroll_to_top`
    /// that is itself now a read of a released store.
    #[test]
    fn a_released_store_parses_nothing_while_off_screen() {
        let mut s = store_with(200);
        s.release();
        for i in 0..500 {
            s.process(format!("after {i}\r\n").as_bytes());
        }

        let grid = s.released_grid().contents();
        assert!(
            grid.trim().is_empty(),
            "the released emulator parsed output nothing can read:\n{grid}"
        );
    }

    /// The skip is only sound while nothing reads a released store's screen,
    /// and that is not a property of this file: it holds because every reader
    /// resolves a store through `App::pane_key`, which is exactly the set
    /// `release_offscreen_stores` leaves alone. A reader added later that goes
    /// straight to `stores` would compile, pass review, and draw an empty pane.
    ///
    /// So the rule is asserted where it can be enforced -- on the read itself,
    /// which no new reader can avoid -- rather than restated in a comment.
    ///
    /// Through `screen`, which is what `log_pane` blits a pane from. Reached
    /// through `visible_lines` instead this passed while `screen` was routed
    /// around the guard entirely, because `visible_lines` is `#[cfg(test)]` and
    /// draws nothing.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "read the screen")]
    fn reading_a_released_store_trips_an_assertion() {
        let mut s = store_with(200);
        s.release();

        let _ = s.screen();
    }

    /// Moving the offset reaches the grid through `live_screen_mut`, a second
    /// door with its own assertion: a pane that resolved its store correctly
    /// for the cells but not for the scrollbar would be just as wrong.
    ///
    /// `scroll_to_top` rather than `scroll_up`, which reads the current offset
    /// first and so trips the *read* assertion before it ever reaches the
    /// second door -- written with `scroll_up` this test passed with
    /// `live_screen_mut`'s assertion deleted. The expected message names the
    /// door for the same reason: "released store" appears in both.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "moved the scroll offset")]
    fn scrolling_a_released_store_trips_an_assertion() {
        let mut s = store_with(200);
        s.release();

        s.scroll_to_top();
    }

    /// The release has to end where the grid is rebuilt, or a reopened pane
    /// shows its replayed history and then nothing further until it next
    /// changes size. Before this test
    /// and the line `a_release_is_replayed_even_back_to_the_minimum_size` now
    /// writes after its own reopen, nothing pinned that: with the assertions
    /// compiled out -- which is every release build -- a store that came back
    /// and never resumed parsing was caught by no behaviour at all.
    #[test]
    fn a_reopened_store_parses_again() {
        let mut s = store_with(200);
        s.release();
        s.resize(10, 40);

        s.process(b"after the reopen\r\n");

        let rows = non_empty(&s);
        assert!(
            rows.iter().any(|l| l == "after the reopen"),
            "a reopened store never resumed parsing:\n{rows:#?}"
        );
    }

    /// A service that has said nothing yet is released too, so the skip covers
    /// its first output as well: those bytes reach `raw`, the line count and
    /// the carry, but not the emulator, and the replay is the only thing that
    /// can ever put them on a screen.
    #[test]
    fn the_first_output_after_a_release_survives_the_reopen() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.release();

        s.process(b"first words\r\n");

        // The premise, not just the conclusion: without this the test would
        // still pass in a world where a store that has never had output is
        // never released in the first place.
        assert!(
            !s.released_grid().contents().contains("first words"),
            "the store parsed its first output while released"
        );

        s.resize(10, 40);

        let rows = non_empty(&s);
        assert!(
            rows.iter().any(|l| l == "first words"),
            "the first output a released store took in was lost:\n{rows:#?}"
        );
    }

    /// Skipping the parse must cost the reopened pane nothing. A store that was
    /// released, kept taking output for the whole time it was closed, and was
    /// then put back in a pane has to show exactly what a store that never left
    /// its pane shows -- same rows, same styling.
    ///
    /// The input is picked for the state the emulator is no longer there to
    /// hold: a colour set before the release and left on, which comes back only
    /// if the replay reproduces it, and a `\r\n` cut in half by the release,
    /// which is where `pending_cr` has to keep carrying across a boundary the
    /// parser no longer sees.
    ///
    /// `raw` is compared as well as the rendered screen, because the rendering
    /// alone would not notice a lost carry: the newline would be normalised a
    /// second time and the doubled carriage return absorbed by the emulator.
    /// The bytes are the level at which "the release changed the ingest" is
    /// visible at all.
    #[test]
    fn output_arriving_while_released_reopens_identically() {
        let feed = |s: &mut LogStore, from: usize| {
            for i in from..from + 20 {
                s.process(format!("line {i}\r\n").as_bytes());
            }
        };
        // Escaped rather than compared as bytes: the difference this is here to
        // catch is a stray control character, and a `Vec<u8>` mismatch prints
        // several hundred decimal numbers to hide it in.
        let raw_of = |s: &LogStore| String::from_utf8_lossy(&s.raw).escape_debug().to_string();

        let mut released = LogStore::new(DEFAULT_SCROLLBACK);
        let mut kept = LogStore::new(DEFAULT_SCROLLBACK);
        for s in [&mut released, &mut kept] {
            s.resize(10, 40);
            s.process(b"\x1b[31mred from here on\r\n");
            feed(s, 0);
            // Leave the stream mid-CRLF, so the release lands between a
            // carriage return and the newline that belongs to it.
            s.process(b"line 20\r");
        }

        released.release();
        for s in [&mut released, &mut kept] {
            s.process(b"\n");
            feed(s, 21);
        }
        released.resize(10, 40);

        assert_eq!(
            raw_of(&released),
            raw_of(&kept),
            "the release changed what the store took in, not just what it parsed"
        );
        assert_eq!(
            released.screen().contents_formatted(),
            kept.screen().contents_formatted(),
            "a reopened pane differs from one that never closed"
        );
    }

    /// Trimming drops the bytes that set the styling still in force, which is
    /// why `LogStore` carries it in `pen` at all. A release that lost the pen
    /// would replay the retained lines in default colours, so a service that
    /// sets a colour once and leaves it on would come back white.
    #[test]
    fn the_carried_pen_survives_a_release() {
        let build = || {
            let mut s = LogStore::new(2);
            s.resize(4, 20);
            for i in 0..40 {
                let bytes = if i == 0 {
                    format!("\x1b[31mline {i}\r\n")
                } else {
                    format!("line {i}\r\n")
                };
                s.process(bytes.as_bytes());
            }
            s
        };
        let mut released = build();
        let resized = build();
        assert!(
            !released.pen.is_empty(),
            "the test is not exercising a trim, so it proves nothing"
        );

        released.release();
        released.resize(4, 20);

        assert_eq!(
            released.screen().contents_formatted(),
            resized.screen().contents_formatted(),
            "the styling carried across the trim was lost"
        );
    }

    /// The one thing a release costs that a closed pane did not cost before.
    ///
    /// While the line budget binds, a replay is lossless. Under `MAX_RAW_BYTES`
    /// it is not: escape-heavy output spends raw bytes without spending
    /// emulator rows, so `raw` falls short of `keep_lines` and the replay
    /// cannot refill the grid. A resize has always paid that -- but before
    /// releasing existed, reopening a pane at an unchanged size short-circuited
    /// `resize` and paid nothing, so the shortfall was only ever charged when a
    /// pane actually changed size. It is now charged whenever a pane closes.
    ///
    /// This asserts the loss deliberately, at an unchanged geometry where there
    /// used to be none. It is accepted because the alternative is holding the
    /// grid for a service nobody is looking at, and because the content that
    /// triggers it is already past the ceiling the buffer exists to enforce.
    /// The test is here so that it cannot silently deepen.
    #[test]
    fn under_the_byte_ceiling_a_release_costs_history_a_reopen_used_to_keep() {
        // Carriage-return padding: each one costs a byte and no row, and leaves
        // the text before it on screen. That is the shape that drives `raw`
        // past the byte ceiling while the line budget stays untouched -- the
        // same asymmetry escape-heavy output has, without the parser cost of
        // eight megabytes of escape sequences.
        //
        // Fed as one write, not six hundred: past the ceiling every write
        // rescans the whole buffer for its trim point, so writing line by line
        // costs hundreds of eight-megabyte scans and minutes of runtime for the
        // same end state.
        let payload = {
            let padding = "\r".repeat(15_000);
            let mut v = Vec::new();
            for i in 0..600 {
                v.extend_from_slice(format!("line {i}{padding}\r\n").as_bytes());
            }
            v
        };
        let build = || {
            let mut s = LogStore::new(DEFAULT_SCROLLBACK);
            s.resize(10, 40);
            s.process(&payload);
            s
        };
        let oldest = |s: &mut LogStore| {
            let text = s.all_text();
            text.lines()
                .find_map(|l| l.trim().strip_prefix("line ").map(str::to_string))
                .and_then(|n| n.parse::<usize>().ok())
                .expect("some retained line")
        };

        let mut reopened = build();
        let mut released = build();
        // The premise: the *byte* ceiling is what trimmed, not the line budget.
        // Fewer lines retained than the budget allows is what proves it.
        assert!(
            reopened.lines < reopened.keep_lines,
            "the byte ceiling never bound: {} lines against a budget of {}",
            reopened.lines,
            reopened.keep_lines
        );
        let live = oldest(&mut reopened);

        // Before this change, reopening at an unchanged size cost nothing,
        // because `resize` short-circuited on the size it already had.
        reopened.resize(10, 40);
        assert_eq!(
            oldest(&mut reopened),
            live,
            "an unchanged resize lost history"
        );

        released.release();
        released.resize(10, 40);

        assert!(
            oldest(&mut released) > live,
            "expected the byte ceiling to cost history across a release"
        );
    }

    /// The byte ceiling can bind while a store is off screen, and `trim_point`
    /// then takes its second path: cut back to the ceiling, preferring a line
    /// boundary. Released, the reopen's replay is the only parse those bytes
    /// will ever get, so what that cut leaves is exactly what the pane comes
    /// back showing.
    ///
    /// The two limits had not been tested together.
    /// `under_the_byte_ceiling_a_release_costs_history_a_reopen_used_to_keep`
    /// crosses the ceiling *before* the release, and the released-store tests
    /// exercise only the line budget.
    ///
    /// Carriage-return padding, and one write rather than six hundred, for the
    /// reasons that test spells out: each `\r` costs a byte and no row, and
    /// past the ceiling every write rescans the whole buffer for its trim
    /// point.
    #[test]
    fn crossing_the_byte_ceiling_while_released_reopens_on_the_newest_content() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"before the release\r\n");
        s.release();

        let padding = "\r".repeat(15_000);
        let mut payload = Vec::new();
        for i in 0..600 {
            payload.extend_from_slice(format!("line {i}{padding}\r\n").as_bytes());
        }
        s.process(&payload);

        // The premise: the *byte* ceiling is what trimmed, not the line budget.
        // Fewer lines retained than the budget allows is what proves it.
        assert!(
            s.lines < s.keep_lines,
            "the byte ceiling never bound: {} lines against a budget of {}",
            s.lines,
            s.keep_lines
        );
        assert!(
            s.raw.len() <= MAX_RAW_BYTES,
            "retained {} bytes against a {MAX_RAW_BYTES} byte ceiling",
            s.raw.len()
        );
        // And it took the line boundary the second path prefers rather than
        // cutting flat at the ceiling. Nothing else covers that: the boundary
        // assertion in `the_retained_buffer_is_bounded_and_cut_at_a_line_boundary`
        // is reached through `trim_point`'s *first* path, where the line budget
        // is what cuts.
        assert!(
            s.raw.starts_with(b"line "),
            "the ceiling cut ignored the line boundary on offer: {:?}",
            String::from_utf8_lossy(&s.raw[..20.min(s.raw.len())])
        );

        s.resize(10, 40);
        let visible = non_empty(&s);
        assert!(
            visible.iter().any(|l| l == "line 599"),
            "the newest line did not come back: {visible:?}"
        );
    }

    /// A ceiling cut that would take the whole buffer with it.
    ///
    /// `trim_point` prefers a line boundary at or after the ceiling, and the
    /// only boundary on offer can be the buffer's last byte -- output with no
    /// newline for eight megabytes and then one at the very end. Taking it
    /// would leave `raw` empty with `has_output` set. For a store on screen
    /// that costs history but nothing visible: `resize` keeps the grid it has,
    /// which still holds the output. Released, there is no such grid -- the
    /// placeholder is empty and the replay is all there is -- so the pane comes
    /// back blank, and `resize`'s own assertion says so. The `filter` rejecting
    /// that cut is what this covers.
    ///
    /// Padded with `\r` rather than with printable bytes so the replay is eight
    /// megabytes of column-zero returns rather than two hundred thousand rows
    /// of scrolling.
    #[test]
    fn a_ceiling_cut_at_the_very_end_does_not_empty_a_released_buffer() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"before the release\r\n");
        s.release();

        let mut payload = vec![b'\r'; MAX_RAW_BYTES];
        payload.extend_from_slice(b"tail line\r\n");
        s.process(&payload);

        assert!(!s.raw.is_empty(), "the ceiling cut took the whole buffer");

        s.resize(10, 40);
        assert_eq!(
            non_empty(&s),
            vec!["tail line".to_string()],
            "the newest line did not come back"
        );
    }

    /// A store with output it cannot reproduce must keep the grid it has.
    ///
    /// No public sequence reaches this: `trim_point` never cuts the whole
    /// buffer away, so `raw` is non-empty whenever `has_output` is. The guard
    /// is insurance mirroring the one in `resize`, and the test has to reach
    /// past the API to exercise it -- releasing such a store would blank a pane
    /// permanently, which is worth a branch that costs nothing.
    #[test]
    fn a_store_that_cannot_be_rebuilt_keeps_its_grid() {
        let mut s = store_with(200);
        s.raw.clear();

        s.release();

        assert_eq!(s.screen().size(), (10, 40));
        assert!(!non_empty(&s).is_empty(), "the pane was blanked");
    }

    /// A compose recreate replaces `web-1` with a *new* container carrying the
    /// same `container-number`, so the replacement's output lands on the store
    /// the dead one was using. If the dead container stopped mid-line, the
    /// emulator's cursor is still part way along that row, and the
    /// replacement's first chunk continues it -- one row showing text neither
    /// container wrote.
    ///
    /// Nothing but the change of identity separates the two writes: no resize,
    /// no topology event, no elapsed time. That is the whole signal there is,
    /// which is why it is the one the fix acts on.
    #[test]
    fn a_recreated_container_does_not_inherit_the_dead_one_s_held_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"Error: shutting");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["Error: shutting", "listening on 8080"],
            "the dead container's row ran on into its replacement's first chunk"
        );
    }

    /// The break ends a row; it does not insert one. This is the only test
    /// whose *rendered-row* assertion can catch a blank one on a break path.
    /// Most of the others read their rows through `non_empty`, which discards
    /// exactly the blank a doubled break would leave; the two that index the
    /// rows directly emit no break at all; and
    /// `a_recreate_does_not_move_a_scrolled_up_reader`, which does read
    /// unfiltered rows after a break, compares them against a snapshot rather
    /// than against text -- `vt100` advances the scroll offset by however many
    /// rows evicted, so a doubled break moves the snapshot with it and still
    /// compares equal.
    ///
    /// The newline count is not the same watch twice over, but it is not the
    /// only one either: `the_break_is_the_stream_s_own_newline` compares whole
    /// `raw` buffers and so catches a doubled break in the bytes as well. What
    /// it does not catch is a break doubled on the grid alone, which is why
    /// both halves are here.
    ///
    /// Both halves are needed. The rendered rows catch a blank in the pane; the
    /// newline count catches one that is only in `raw`, which costs a line of
    /// the retention budget per recreate whether or not a pane is open to show
    /// where it went.
    #[test]
    fn the_break_a_recreate_makes_costs_exactly_one_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"Error: shutting");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        let rows = s.visible_lines();
        assert_eq!(rows[0].trim_end(), "Error: shutting");
        assert_eq!(
            rows[1].trim_end(),
            "listening on 8080",
            "a blank row was pushed between the two containers"
        );
        assert_eq!(
            s.raw.iter().filter(|b| **b == b'\n').count(),
            2,
            "the break spent more than one line of the retention budget"
        );
    }

    /// The break is written as a bare `\n` and left to `normalise_newlines` to
    /// dress, rather than as a `\r\n` of its own. Nothing about the rendered
    /// rows can tell the two apart -- a `\r` at column 0 moves nothing -- so
    /// this reads the bytes, which is where the difference lives and where a
    /// replay will find it.
    ///
    /// The held row ends on a carriage return, which is the case that
    /// discriminates: `pending_cr` is set, so the stream needs the `\n` alone
    /// and a break supplying its own `\r` would put one in `raw` that no
    /// container sent.
    ///
    /// Since #72 that is not only a question of what the break writes but of
    /// what the handover before it leaves behind. `process` re-derives the
    /// carry from every write, and [`HANDOVER`] ends on the `l` of an
    /// alternate-screen exit rather than a carriage return, so without `adopt`
    /// putting the carry back the break would find the flag clear and supply
    /// the `\r` itself. `HANDOVER` is spliced into the expectation rather than spelled
    /// out, because what this test is about is the byte either side of it.
    ///
    /// The carry the break leaves behind is checked directly rather than
    /// through its effects, because here it has none to check. A break that
    /// bypassed the normalisation would leave the carry set, and a stale carry
    /// suppresses the `\r` in front of the next chunk's opening newline -- but
    /// the break has just put the cursor at column 0, so a bare line feed and a
    /// carriage-return-plus-line-feed land in the same place and the rows come
    /// out identical. The damage a stale carry does is an indent, and an indent
    /// needs a cursor that is somewhere else. So the flag is asserted, and the
    /// write that follows is asserted on the retained bytes, where the missing
    /// `\r` does show and where a replay would eventually find it.
    #[test]
    fn the_break_is_the_stream_s_own_newline() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"downloading 50%\r");
        s.adopt("web-1-second", 1);

        let mut expected = b"downloading 50%\r".to_vec();
        expected.extend_from_slice(HANDOVER);
        expected.push(b'\n');
        assert_eq!(
            s.raw, expected,
            "the break wrote a carriage return no container sent"
        );
        assert!(
            !s.pending_cr,
            "the break left the carry claiming a carriage return is still open"
        );

        s.process(b"\nlistening on 8080\n");
        expected.extend_from_slice(b"\r\nlistening on 8080\r\n");
        assert_eq!(
            s.raw, expected,
            "the carry the break left behind ate the next chunk's carriage return"
        );
    }

    /// The break has to be in `raw`, not only on the grid. A resize throws the
    /// grid away and replays `raw` from scratch, so a break written only to the
    /// emulator would splice the two containers back together the first time
    /// the pane changed size -- and the pane the store was created for is
    /// almost always a different size from the one it starts at, so that resize
    /// is the ordinary case rather than an unusual one.
    ///
    /// The narrower width also proves the break is a real line rather than a
    /// wrap: rewrapped at 40 columns the two texts would sit on one row
    /// together if they had been joined.
    #[test]
    fn the_break_a_recreate_makes_survives_a_replay() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 80);

        s.adopt("web-1-first", 1);
        s.process(b"Error: shutting");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        s.resize(10, 40);

        assert_eq!(
            non_empty(&s),
            vec!["Error: shutting", "listening on 8080"],
            "the replay spliced the two containers back together"
        );
    }

    /// A pane whose store was released is rebuilt entirely from `raw` when it
    /// comes back, and while released the parser is skipped altogether (#58).
    /// So a recreate that happens with no pane open has only `raw` to record it
    /// in, and the row it broke has to still be broken when someone looks.
    ///
    /// The premise is asserted, not assumed. `release` declines a store it
    /// could not rebuild, so if its conditions ever change this would quietly
    /// stop being about the released path and become a second copy of the
    /// replay test above -- passing, and covering nothing the other does not.
    /// Read through `released_grid`, which is the deliberate test-only
    /// exception for looking at a grid a release left behind.
    #[test]
    fn a_recreate_while_released_still_breaks_the_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.adopt("web-1-first", 1);
        s.process(b"Error: shutting");

        s.release();
        assert_eq!(
            s.released_grid().size(),
            (MIN_ROWS, MIN_COLS),
            "the store was never released, so this is only the replay test again"
        );

        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");
        s.resize(10, 40);

        assert_eq!(
            non_empty(&s),
            vec!["Error: shutting", "listening on 8080"],
            "a recreate that happened off-screen was spliced by the replay"
        );
    }

    /// The other half of the rule: output really is joined across chunks while
    /// the container stays put. The stream layer cuts frames at
    /// `MAX_CHUNK_BYTES` with no regard for content, so breaking on every write
    /// instead of on a change of identity would pass the recreate tests above
    /// and break every line that arrives in two pieces.
    ///
    /// `raw` is checked as well as the rows, because an unchanged identity has
    /// to leave the retained buffer alone and not merely leave the pane looking
    /// right: a break that landed in `raw` and nowhere else would render
    /// correctly now and split the line on the next resize.
    #[test]
    fn one_container_s_row_is_still_joined_across_chunks() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"GET /one");
        s.adopt("web-1-first", 1);
        s.process(b" 200\n");

        assert_eq!(non_empty(&s), vec!["GET /one 200"]);
        assert_eq!(
            s.raw, b"GET /one 200\r\n",
            "an unchanged identity wrote a break into the retained buffer"
        );
    }

    /// A recreate that catches the dead container between lines has no row part
    /// way along to end, and must not manufacture one: a blank row pushed in on
    /// every recreate is a gap in the pane and a line off the retention budget
    /// each time.
    ///
    /// Asserted on the rows as they sit rather than through `non_empty`, which
    /// discards exactly the blank row under test, and on `raw` besides, because
    /// a blank line written there costs the budget whether or not a pane is
    /// currently wide enough to show where it went.
    #[test]
    fn a_recreate_on_a_line_boundary_adds_no_blank_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"clean exit\n");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        let rows = s.visible_lines();
        assert_eq!(rows[0].trim_end(), "clean exit");
        assert_eq!(
            rows[1].trim_end(),
            "listening on 8080",
            "a blank row was pushed between the two containers"
        );
        assert_eq!(
            s.raw.iter().filter(|b| **b == b'\n').count(),
            2,
            "a blank line was written into the retained buffer"
        );
    }

    /// The first container to write has nothing before it to break away from,
    /// and the store starts with an empty ID that no real container can carry.
    /// Ending a row here would push every pane in the stack down by one and
    /// spend a line of its budget before any output arrived.
    #[test]
    fn the_first_container_to_write_gets_no_leading_break() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            s.visible_lines()[0].trim_end(),
            "listening on 8080",
            "the pane's first row was given away to a blank"
        );
        assert_eq!(
            s.raw.iter().filter(|b| **b == b'\n').count(),
            1,
            "a leading blank line was written into the retained buffer"
        );
    }

    /// A chunk can end between a `\r` and its `\n`, and can end on a `\r` a
    /// progress rewrite meant to keep. Either way the store is part way along a
    /// row, so a recreate there has a row to end.
    ///
    /// This is the splice that leaves no seam to see. Elsewhere the two
    /// containers' text runs together and the join is visible in the result;
    /// here the replacement writes from column 0 over a row the dead container
    /// had already returned the cursor to, so the row simply shows the wrong
    /// container's text and nothing looks wrong at all.
    ///
    /// What the break has to do with the carry is a separate question, settled
    /// in `the_break_is_the_stream_s_own_newline` -- this scenario cannot ask
    /// it, because the replacement's chunk opens on a printable byte and a
    /// mishandled carry is only visible to a chunk opening on a newline.
    #[test]
    fn a_recreate_part_way_through_a_progress_rewrite_still_breaks_the_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"downloading 50%\r");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["downloading 50%", "listening on 8080"],
            "the replacement overwrote the dead container's progress row"
        );
    }

    /// The predicate reads one byte -- `raw`'s last -- and the argument that
    /// this is the last byte the store received rests entirely on trimming only
    /// ever cutting from the front. Nothing exercised that: every other recreate
    /// test here runs on a buffer far too small to trim.
    ///
    /// Trimming before the tail arrives is not enough to exercise it, though,
    /// and an earlier version of this test made that mistake. A trim here is
    /// triggered by the line budget, so a write carrying no newline never
    /// causes one at these sizes -- `MAX_RAW_BYTES` is the other trigger and
    /// is eight megabytes away. Send the lines and the tail separately and
    /// every trim has already happened by the time a partial is being held,
    /// which is the one arrangement where nothing can go wrong. The tail has
    /// to arrive in the same write that overruns the budget, so the cut runs
    /// with the partial already appended to `raw`. That is not a contrivance:
    /// a docker frame routinely carries several complete lines and then stops
    /// part way along the next one.
    ///
    /// What this then catches is a trim that drops the held partial on its way
    /// past: the predicate reads a byte that is not the last one written and
    /// the break goes missing. A trim that cut from the end instead would break
    /// the same argument, but the retention tests already fail on that long
    /// before it reached here.
    #[test]
    fn a_recreate_after_the_buffer_has_been_trimmed_still_breaks_the_row() {
        let mut s = LogStore::new(1);
        s.resize(MIN_ROWS, MIN_COLS);

        s.adopt("web-1-first", 1);
        for i in 0..40 {
            s.process(format!("line {i}\n").as_bytes());
        }
        let before = s.raw.len();
        // Complete lines enough to overrun the budget, then a partial, in one
        // write -- so `retain` cuts while the tail is held rather than before
        // it exists.
        s.process(b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nheld tail");
        // The premise: that write really did trim. Without this the assertion
        // below is about a buffer that was never cut at all.
        assert!(
            s.raw.len() < before,
            "the write that left the tail did not trim: {before} bytes -> {}",
            s.raw.len()
        );

        s.adopt("web-1-second", 1);
        s.process(b"new container\n");

        assert_eq!(
            non_empty(&s),
            vec!["held tail", "new container"],
            "a trimmed buffer lost the break"
        );
    }

    /// The container ID is not the whole identity, which is #68. A container's
    /// log task can end while the container keeps running -- the event stream
    /// or the log stream dropping, a daemon hiccup -- and the next resync
    /// reattaches the same container from `since = ended_at`. `since` resolves
    /// to the second, so the replay can begin at or before the entry the
    /// emulator's cursor is part way along and deliver that entry whole. The ID
    /// has not changed, so nothing but the attach can see it.
    ///
    /// This is the exact shape #57 fixed in the fallback, where it printed
    /// `GET /oneGET /one 200`. Here the same fabricated line is rendered into
    /// the grid.
    #[test]
    fn a_reattach_of_the_same_container_does_not_continue_the_held_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1", 1);
        s.process(b"GET /one");
        s.adopt("web-1", 2);
        s.process(b"GET /one 200\n");

        assert_eq!(
            non_empty(&s),
            vec!["GET /one", "GET /one 200"],
            "the reattach's replay ran on into the row the first attach held"
        );
    }

    /// `adopt` compares the identity it was handed. It does not assume the ids
    /// only ever climb, and this is what says so.
    ///
    /// Monotonicity is real but it belongs to `AttachIds`, a module away: one
    /// counter, handed out by `LogSupervisor::attach`, and `plan_attachments`
    /// asks for a *second* id for a container only once that container's
    /// previous task has reported finished, so no two live attaches to it
    /// overlap. A container it has never attached is the other branch, and
    /// takes its first id from the same climbing counter.
    /// Resting on that would be the mistake `LineAssembler::adopt` explicitly
    /// refuses on the fallback side -- it compares both halves of the identity
    /// "so that the recreate guarantee does not come to rest on a stamp
    /// continuing to move, one line away in another module". This is the same
    /// refusal on this side, and it costs one test.
    ///
    /// No input reaches `LogStore` with a smaller id today: the supervisor and
    /// the `App` holding the stores are built together in `run_tui` and die
    /// together, so the counter cannot restart under a store that outlived it.
    /// That is why this is written as a statement about `adopt`'s contract
    /// rather than as a reproduction of anything -- an `adopt` comparing `>=`
    /// passes every other test in the suite.
    #[test]
    fn an_attach_id_that_goes_backwards_still_ends_the_row() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1", 2);
        s.process(b"GET /one");
        s.adopt("web-1", 1);
        s.process(b"GET /one 200\n");

        assert_eq!(
            non_empty(&s),
            vec!["GET /one", "GET /one 200"],
            "the row was held because the new attach id was not larger"
        );
    }

    /// The row break alone does not undo a half-written escape sequence, which
    /// is #72. A container that stopped after `\x1b[3` leaves vte in CSI
    /// parameter state; C0 controls execute from there without ending the
    /// sequence, so the break does end the row, and then the replacement's
    /// first byte lands as that sequence's final byte and disappears --
    /// `new line` rendered as `ew line`.
    ///
    /// A CSI in parameter state eats until a byte lands in `0x40..=0x7E`, so
    /// what it takes is not bounded at one character: a timestamped first line
    /// loses its whole date prefix, and a line of digits carries the sequence
    /// past its own newline into the line after.
    ///
    /// Three parser states rather than one, because [`HANDOVER`] claims all of
    /// them and one of them is not a CSI at all. A bare `ESC` and an `ESC` that
    /// has taken an intermediate are reached by a container dying one and two
    /// bytes into a sequence respectively, and they leave vte somewhere else --
    /// which is the point, since what recovers all three is the same
    /// anywhere-transition rather than anything specific to a CSI.
    #[test]
    fn a_recreate_does_not_leave_the_replacement_inside_a_half_written_sequence() {
        for (state, tail) in [
            ("a CSI part way through its parameters", &b"tail\x1b[3"[..]),
            ("a bare ESC", &b"tail\x1b"[..]),
            ("an ESC that has taken an intermediate", &b"tail\x1b("[..]),
        ] {
            let mut s = LogStore::new(DEFAULT_SCROLLBACK);
            s.resize(10, 40);

            s.adopt("web-1-first", 1);
            s.process(tail);
            s.adopt("web-1-second", 1);
            s.process(b"new line\n");

            assert_eq!(
                non_empty(&s),
                vec!["tail", "new line"],
                "{state}: the dead container's unfinished sequence ate the \
                 replacement's output"
            );
        }
    }

    /// The string sequences are the severe half of #72. OSC, DCS, APC and PM
    /// put vte in a state that *collects* C0 controls rather than executing
    /// them, so the row break is swallowed along with everything the
    /// replacement writes, until some byte happens to terminate the string --
    /// which may be nothing it ever writes. The pane stops updating and stays
    /// stopped.
    ///
    /// A title-setting OSC is the ordinary way to reach this: a service that
    /// announces itself in the window title and is killed part way through the
    /// write leaves exactly this. All four are exercised rather than the one,
    /// because vte reaches them by different routes -- `SosPmApcString` is a
    /// state of its own, and DCS passes through a hook the others do not -- and
    /// because [`HANDOVER`] names all four.
    ///
    /// Two lines are written afterwards, not one. A swallow is unbounded: what
    /// fails here is not a first line arriving damaged but output stopping, so
    /// the assertion has to be able to tell "the pane came back" from "the pane
    /// came back for one line".
    #[test]
    fn a_recreate_inside_a_string_sequence_does_not_swallow_the_replacement() {
        for (introducer, tail) in [
            ("OSC", &b"tail\x1b]0;serv"[..]),
            ("DCS", &b"tail\x1bPq#0"[..]),
            ("APC", &b"tail\x1b_data"[..]),
            ("PM", &b"tail\x1b^msg"[..]),
        ] {
            let mut s = LogStore::new(DEFAULT_SCROLLBACK);
            s.resize(10, 40);

            s.adopt("web-1-first", 1);
            s.process(tail);
            s.adopt("web-1-second", 1);
            s.process(b"new line\nand another\n");

            assert_eq!(
                non_empty(&s),
                vec!["tail", "new line", "and another"],
                "{introducer}: the dead container's unterminated string \
                 swallowed the replacement"
            );
        }
    }

    /// The pen is the half of #72 that needs no accident at all to bite. A
    /// container that sets a colour and exits cleanly on a newline leaves the
    /// pen set, and every line its replacement writes comes out in the dead
    /// container's colour -- a stopped service's red carried on to a healthy
    /// one's startup line.
    ///
    /// So this is also the case that pins the handover as unconditional. `raw`
    /// ends on a newline here, so there is no row to break and the #60
    /// mechanism writes nothing; a handover gated on the break would leave the
    /// pen exactly as it found it.
    ///
    /// The dead container's own row is asserted coloured first. Without that
    /// premise a fix that reset nothing and a screen that was never coloured
    /// look the same from the second assertion.
    #[test]
    fn a_recreate_does_not_render_the_replacement_in_the_dead_container_s_pen() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"\x1b[31mError: exiting\n");
        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        let dead = s.screen().cell(0, 0).expect("the dead container's row");
        assert_ne!(
            dead.fgcolor(),
            vt100::Color::Default,
            "the dead container never set a colour, so nothing here is under test"
        );
        let replacement = s.screen().cell(1, 0).expect("the replacement's row");
        assert_eq!(
            replacement.fgcolor(),
            vt100::Color::Default,
            "the replacement is rendering in the dead container's colour"
        );
    }

    /// The alternate screen is the third of the states #72 names, and the one
    /// whose symptom is a blank pane rather than a wrong line. A container that
    /// switched to it and died leaves the emulator showing an alternate grid,
    /// which has none of the service's history in it and no scrollback to reach
    /// it by; the replacement then writes into that same grid. Nothing brings
    /// the pane back, because `raw` replays the switch on every resize.
    ///
    /// The premise is asserted, because a `vt100` that ignored the switch would
    /// make the second assertion pass on a store that was never in the
    /// alternate screen at all.
    ///
    /// The blank row between the two is the accepted cost: `raw` ends on the
    /// switch's `h` rather than a newline, so the handover's break fires, and
    /// it fires against the primary grid the exit has just restored -- where
    /// the cursor was already at column 0. `non_empty` is what discards it, and
    /// the row it costs is far cheaper than the pane it buys back.
    #[test]
    fn a_recreate_brings_a_pane_back_from_the_alternate_screen() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"serving requests\n");
        s.process(b"\x1b[?1049h");
        assert!(
            non_empty(&s).is_empty(),
            "the switch to the alternate screen did nothing, so nothing is under test"
        );

        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["serving requests", "listening on 8080"],
            "the pane is still showing the dead container's alternate screen"
        );
    }

    /// `ESC 7` is taken after `CSI ? 47 l`, not before it. `saved_pos` lives on
    /// the `Grid` and vt100 reaches both halves through `grid_mut()`, so a save
    /// taken while the dead container's alternate grid is still selected is not
    /// the one the restore reads once `CSI ? 47 l` has switched back -- the
    /// restore finds the primary grid's `Pos::default()` and homes the cursor,
    /// which is the whole thing the save exists to prevent.
    ///
    /// `CSI ? 47 h` rather than the `CSI ? 1049 h` an application would really
    /// use, and that is the point of this test rather than an oversight.
    /// vt100's `1049` takes a `decsc` on the primary grid on the way in, so a
    /// save on the wrong grid is covered for by a `saved_pos` the entry left
    /// behind and the bug does not show; `47` writes no saved cursor at all.
    /// `a_recreate_brings_a_pane_back_from_the_alternate_screen` uses `1049`
    /// and passes either way.
    ///
    /// That is also why #79's `CSI ? 1049 h` `CSI ? 1049 l` pair is at the end
    /// of [`HANDOVER`] rather than straight after the `CSI ? 47 l`. Its
    /// `decsc` is the same primary-grid save, so a pair sitting ahead of the
    /// `ESC 7` covers for the misplaced save the same way an application's
    /// `1049` entry does, and this test stops failing on the move it exists to
    /// catch. Measured: the move fails five tests with the pair at the end and
    /// four with it ahead of the `CSI m`, and this is the one that drops out.
    ///
    /// Two rows of dead output, because a homed cursor and a restored one have
    /// to land somewhere different for the assertion to see them apart.
    #[test]
    fn the_handover_saves_the_cursor_on_the_grid_it_puts_it_back_on() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"serving requests\nstill serving\n");
        s.process(b"\x1b[?47h");
        assert!(
            non_empty(&s).is_empty(),
            "the switch to the alternate screen did nothing, so nothing is under test"
        );

        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["serving requests", "still serving", "listening on 8080"],
            "the handover saved the cursor on the alternate grid and restored it \
             on the primary one"
        );
    }

    /// A scroll region the dead container set is not a cosmetic inheritance:
    /// `Grid::scroll_up` pushes an evicted row into the scrollback only
    /// `if !self.scroll_region_active()`, so a successor writing inside a dead
    /// container's region scrolls within it and the pane accumulates no
    /// history at all. Everything that leaves the region is simply gone, and
    /// no amount of scrolling up reaches it. That is #75.
    ///
    /// The premise is asserted against the dead container's own output first.
    /// A `vt100` that ignored DECSTBM, or a region that did not bind at this
    /// pane size, would make the second half pass on a store that never had a
    /// scroll region to inherit.
    #[test]
    fn a_recreate_is_not_confined_to_the_dead_container_s_scroll_region() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"\x1b[1;4r");
        for i in 0..20 {
            s.process(format!("dead {i}\n").as_bytes());
        }
        s.scroll_to_top();
        assert_eq!(
            s.scroll_offset(),
            0,
            "the scroll region never bound, so nothing here is under test"
        );
        s.scroll_to_bottom();

        s.adopt("web-1-second", 1);
        for i in 0..20 {
            s.process(format!("live {i}\n").as_bytes());
        }

        s.scroll_to_top();
        assert!(
            s.scroll_offset() > 0,
            "the replacement is still scrolling inside the dead container's \
             region, so the pane retains no history"
        );
        assert!(
            s.all_text().contains("live 0"),
            "the replacement's earliest output fell out of the dead \
             container's region and was lost"
        );
    }

    /// The same fault as
    /// `a_recreate_is_not_confined_to_the_dead_container_s_scroll_region`, on
    /// the grid that fix cannot reach, which is #79. [`HANDOVER`]'s `CSI r`
    /// runs after its `CSI ? 47 l`, so it resets the *primary* grid's region
    /// and leaves a region the dead container set on the alternate grid
    /// exactly where it was. A successor that enters with bare `CSI ? 47 h`
    /// then writes inside it: vt100's `47` is `enter_alternate_grid()` alone,
    /// where `1049` clears the grid on the way in, so `47` on both sides is
    /// what makes the inheritance reachable at all.
    ///
    /// Compared against a control rather than a written-out expectation. What
    /// a successor renders inside a full-height alternate grid depends on the
    /// pane height and on how much it writes, and neither is what is under
    /// test -- the question is only whether the dead container changed it.
    ///
    /// The premise is the dead container's region binding on the alternate
    /// grid, asserted through the dead container's own output: without it the
    /// comparison would pass on a store that never had a region to inherit.
    /// It is asserted before the handover for the same reason the fixture
    /// exists -- afterwards, a passing test and a broken premise look alike.
    #[test]
    fn a_recreate_is_not_confined_to_a_region_on_the_alternate_grid() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"serving requests\n");
        s.process(b"\x1b[?47h");
        s.process(b"\x1b[3;5r");
        for i in 0..8 {
            if i > 0 {
                s.process(b"\r\n");
            }
            s.process(format!("dead {i}").as_bytes());
        }
        // Eight lines into a three-row region leaves three on screen. Without
        // the region binding there would be eight, and the comparison below
        // would be measuring a store that had nothing to inherit. No trailing
        // newline, so the count is the region's height rather than one less.
        assert_eq!(
            non_empty(&s),
            vec!["dead 5", "dead 6", "dead 7"],
            "the dead container's region did not bind on the alternate grid, \
             so there is nothing here to inherit"
        );

        s.adopt("web-1-second", 1);
        s.process(b"\x1b[?47h");
        for i in 0..12 {
            s.process(format!("live {i}\r\n").as_bytes());
        }

        let mut control = LogStore::new(DEFAULT_SCROLLBACK);
        control.resize(10, 40);
        control.adopt("web-1-second", 1);
        control.process(b"\x1b[?47h");
        for i in 0..12 {
            control.process(format!("live {i}\r\n").as_bytes());
        }

        assert_eq!(
            non_empty(&s),
            non_empty(&control),
            "the replacement is confined to a scroll region the dead container \
             left on the alternate grid"
        );
    }

    /// The other two things #79's alternate grid carries over: the dead
    /// container's cells, and the cursor sitting among them. The row break
    /// `adopt` writes cannot prevent either, which is the part worth having a
    /// test for -- the break is what #60 added to stop exactly this, and it
    /// lands on the primary grid because [`HANDOVER`]'s `CSI ? 47 l` has
    /// already left the alternate one.
    ///
    /// So the successor's first line continues a row the dead container wrote,
    /// which is the #60 symptom on a grid #60 never saw, and the rows above it
    /// are the dead container's. A control is the comparison for the same
    /// reason as in the region test above.
    ///
    /// `DEAD MID` is placed where the successor will start writing rather than
    /// anywhere on the grid, because a row the successor never reaches would
    /// show up as a leftover row and not as a joined one, and the joined row is
    /// the failure that costs a reader a line neither container wrote.
    #[test]
    fn a_recreate_does_not_continue_a_row_left_on_the_alternate_grid() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"serving requests\n");
        s.process(b"\x1b[?47h");
        s.process(b"\x1b[1;1HDEAD TOP");
        s.process(b"\x1b[2;1HDEAD MID");
        assert_eq!(
            non_empty(&s),
            vec!["DEAD TOP", "DEAD MID"],
            "the dead container wrote nothing to the alternate grid, so there \
             is nothing here to inherit"
        );

        s.adopt("web-1-second", 1);
        s.process(b"\x1b[?47h");
        s.process(b"live 1\r\nlive 2\r\n");

        let mut control = LogStore::new(DEFAULT_SCROLLBACK);
        control.resize(10, 40);
        control.adopt("web-1-second", 1);
        control.process(b"\x1b[?47h");
        control.process(b"live 1\r\nlive 2\r\n");

        assert_eq!(
            non_empty(&s),
            non_empty(&control),
            "the replacement inherited the dead container's alternate grid"
        );
    }

    /// The scroll offset is carried across the handover by hand, because
    /// [`HANDOVER`]'s `CSI ? 1049 h` moves it. `enter_alternate_grid` calls
    /// `grid_mut().set_scrollback(0)` before it sets `MODE_ALTERNATE_SCREEN`,
    /// so that reset lands on the primary grid, and vt100's `set_scrollback`
    /// is a scroll position rather than a capacity -- here, the reader's.
    ///
    /// `a_recreate_does_not_move_a_scrolled_up_reader` fails on the same
    /// mutation and is not replaced by this; it deliberately asserts on the
    /// rendered rows, because a break that evicts a row *should* move the
    /// offset while the reader holds still. This asserts the number instead,
    /// on the one fixture where the two questions cannot be confused: `raw`
    /// ends on a newline, so `adopt` writes no break, nothing evicts, and the
    /// offset the reader had is the offset they must still have. What that
    /// buys is a failure that says which of the two moved.
    #[test]
    fn a_recreate_leaves_a_scrolled_up_reader_s_offset_where_it_found_it() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.adopt("web-1-first", 1);
        for i in 0..50 {
            s.process(format!("line {i}\n").as_bytes());
        }
        s.scroll_up(20);
        let before = s.scroll_offset();
        // The premise: the reader really is up in the scrollback, not pinned at
        // the bottom where nothing could move them anyway.
        assert!(before > 0, "the reader never left the bottom");
        assert_eq!(
            s.raw.last().copied(),
            Some(b'\n'),
            "`raw` does not end on a newline, so the handover writes a break \
             and the offset is entitled to move"
        );

        s.adopt("web-1-second", 1);

        assert_eq!(
            s.scroll_offset(),
            before,
            "the handover moved a scrolled-up reader's offset"
        );
    }

    /// The region reset has to be in `raw`, not only on the grid, for the same
    /// reason the break and the rest of the handover do: a resize throws the
    /// grid away and replays `raw` from scratch, and the pane a store was
    /// created for is almost always a different size from the one it starts
    /// at, so that resize is the ordinary case rather than an unusual one. A
    /// reset applied to the live parser alone puts the dead container's region
    /// straight back the first time the pane changes size.
    ///
    /// The control is the premise, and it is worth having on its own: it is
    /// what shows that a `CSI r` in `raw` really does come back out of a
    /// replay at the new size, so the second half is measuring the handover
    /// rather than a region that had quietly stopped binding.
    #[test]
    fn the_scroll_region_reset_survives_a_replay() {
        let mut control = LogStore::new(DEFAULT_SCROLLBACK);
        control.resize(10, 80);
        control.adopt("web-1-first", 1);
        control.process(b"\x1b[1;4rdead output\n");
        control.resize(10, 40);
        for i in 0..20 {
            control.process(format!("live {i}\n").as_bytes());
        }
        control.scroll_to_top();
        assert_eq!(
            control.scroll_offset(),
            0,
            "the region did not survive the replay on its own, so nothing here \
             is under test"
        );

        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 80);
        s.adopt("web-1-first", 1);
        s.process(b"\x1b[1;4rdead output\n");
        s.adopt("web-1-second", 1);
        s.resize(10, 40);
        for i in 0..20 {
            s.process(format!("live {i}\n").as_bytes());
        }
        s.scroll_to_top();
        assert!(
            s.scroll_offset() > 0,
            "the replay put the dead container's scroll region back"
        );
    }

    /// Resetting the region must not cost the rows the reset exists to
    /// protect. `CSI r` on its own ends `Grid::set_scroll_region` with
    /// `self.pos.row = self.scroll_top; self.pos.col = 0`, homing the cursor
    /// to the top of the visible grid -- so the successor starts writing over
    /// rows the dead container's output is still on. #75 read that as the
    /// reason the region could not be reset at all; the `ESC 7` and `ESC 8`
    /// around it are what make it affordable.
    ///
    /// Three rows is more than this needs, and what it needs is not rows.
    /// `CSI r` resets the region to full height and ends by homing to
    /// `(scroll_top, 0)`, which is now `(0, 0)`, so the homed cursor and the
    /// restored one are told apart wherever the dead container's row is not row
    /// 0 -- not, as this said twice before, wherever it is off the top of the
    /// region the dead container had. The two coincide only for a region that
    /// starts at row 0, which is the one this fixture uses. A single `first\n`
    /// is enough: the trailing newline leaves the cursor on row 1. `first` with
    /// no newline is not -- the cursor is still on row 0, and `adopt`'s own
    /// break then moves the homed and the restored cursor to row 1 alike,
    /// column included, because the break carries a `\r`. Measured across one,
    /// two and three rows with and without the trailing newline, and again
    /// across regions that do not start at row 0: every shape that leaves the
    /// cursor off row 0 catches a bare `CSI r`, and every shape that does not,
    /// does not. Three rows are kept for reading, not for reach.
    #[test]
    fn resetting_the_scroll_region_does_not_move_the_cursor() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"\x1b[1;4rfirst\nsecond\nthird\n");

        s.adopt("web-1-second", 1);
        s.process(b"listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["first", "second", "third", "listening on 8080"],
            "the dead container's output did not survive the handover intact"
        );
    }

    /// [`HANDOVER`] resets the pen *before* it saves the cursor.
    /// `Screen::save_cursor` saves `self.attrs` alongside the grid position
    /// and `Screen::restore_cursor` puts both back, so an `ESC 7` taken ahead
    /// of the `CSI m` would park the dead container's pen in `saved_attrs` --
    /// where a successor's own `ESC 8` hands it straight back, which is the
    /// fault #72's `CSI m` exists to prevent, merely deferred.
    ///
    /// A successor that restores a cursor it never saved is not a contrivance
    /// worth building the test around on its own; what it is here is the only
    /// way to read `saved_attrs`, which `vt100` exposes no other way.
    ///
    /// The first assertion is not scaffolding either. Before the handover
    /// carried a save at all, `saved_pos` was `Pos::default()` and the same
    /// `ESC 8` homed the cursor onto the dead container's row.
    #[test]
    fn a_recreate_does_not_leave_the_dead_container_s_pen_where_a_restore_finds_it() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);
        s.process(b"\x1b[31mError: exiting\n");
        s.adopt("web-1-second", 1);
        s.process(b"\x1b8listening on 8080\n");

        assert_eq!(
            non_empty(&s),
            vec!["Error: exiting", "listening on 8080"],
            "the successor's restore put the cursor on the dead container's row"
        );
        let dead = s.screen().cell(0, 0).expect("the dead container's row");
        assert_ne!(
            dead.fgcolor(),
            vt100::Color::Default,
            "the dead container never set a colour, so nothing here is under test"
        );
        let replacement = s.screen().cell(1, 0).expect("the replacement's row");
        assert_eq!(
            replacement.fgcolor(),
            vt100::Color::Default,
            "the successor's restore handed back the dead container's pen"
        );
    }

    /// The byte-ceiling trim is the one cut that can land inside the handover.
    /// The line budget cannot: it cuts immediately after a `\n` and
    /// [`HANDOVER`] carries none, so it takes the whole thing or none of it.
    /// The ceiling fallback cuts at an arbitrary byte, and a cut between the
    /// `ESC 7` and the `ESC 8` leaves a restore with no matching save in the
    /// retained stream.
    ///
    /// It is harmless, and that is asserted rather than reasoned about. The
    /// fallback cuts at the *front* of what survives, so the orphaned restore
    /// runs against a parser that has read nothing but the carried pen: the
    /// cursor is at the origin and `saved_pos` is still `Pos::default()`, so
    /// the restore puts back the position the replay already has. The control
    /// is the same retained bytes with the orphan removed, which is the only
    /// comparison that can tell "did nothing" from "did something invisible
    /// at this size".
    ///
    /// `HANDOVER`'s second restore is not what is under test here, and the
    /// reason is worth writing down rather than leaving to be re-derived. The
    /// `CSI ? 1049 l` #79 added ends in the same `decrc`, but at this cut its
    /// `CSI ? 1049 h` survives with it, so that pair is matched and the
    /// `ESC 8` is the only orphan. A deeper cut, landing inside the
    /// `CSI ? 1049 h`, orphans the `1049 l` instead -- the `ESC 8` is gone by
    /// then rather than orphaned alongside it -- and lands in the same place:
    /// `exit_alternate_grid` on a parser not in the alternate screen does
    /// nothing, and the `decrc` behind it reads the same `Pos::default()` this
    /// one does. One `decrc` with no save in front of it is the whole of what
    /// either case is, which is why one fixture answers for both.
    #[test]
    fn a_trim_that_orphans_the_handover_s_cursor_restore_changes_nothing() {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.adopt("web-1-first", 1);
        s.process(b"\x1b[1;3rgoodbye\n");
        s.adopt("web-1-second", 1);

        // Sized so the ceiling cut lands 21 bytes from the end of the
        // handover, which is the `CSI r`, the `ESC 8` and the `CSI ? 1049 h`
        // `CSI ? 1049 l` pair behind it: the cut is measured back from the end
        // of `raw`, so the prefix length does not enter into it. The premise
        // assertion spells the whole surviving tail out rather than its first
        // bytes, so that any future change to `HANDOVER`'s length moves the
        // cut somewhere this refuses to run rather than somewhere it passes
        // vacuously.
        let blob = vec![b'X'; MAX_RAW_BYTES - 21];
        s.process(&blob);
        assert!(
            s.raw.starts_with(b"\x1b[r\x1b8\x1b[?1049h\x1b[?1049l"),
            "the cut did not orphan a cursor restore, so nothing is under test"
        );

        let tail = s.raw.clone();
        s.resize(5, 100);

        let mut control = LogStore::new(16);
        control.resize(5, 40);
        control.process(&tail[..3]);
        control.process(&tail[5..]);
        control.resize(5, 100);

        assert_eq!(
            non_empty(&s),
            non_empty(&control),
            "the orphaned cursor restore moved the replay's cursor"
        );
    }

    /// The handover has to be in `raw`, not only on the grid, for the same
    /// reason the break does: a resize throws the grid away and replays `raw`
    /// from scratch, so a reset applied to the live parser alone is undone the
    /// first time the pane changes size -- and the pane a store was created for
    /// is almost always a different size from the one it starts at.
    #[test]
    fn the_handover_a_recreate_makes_survives_a_replay() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 80);

        s.adopt("web-1-first", 1);
        s.process(b"\x1b[31mtail\x1b[3");
        s.adopt("web-1-second", 1);
        s.process(b"new line\n");

        s.resize(10, 40);

        assert_eq!(
            non_empty(&s),
            vec!["tail", "new line"],
            "the replay put the replacement back inside the dead container's sequence"
        );
        let replacement = s.screen().cell(1, 0).expect("the replacement's row");
        assert_eq!(
            replacement.fgcolor(),
            vt100::Color::Default,
            "the replay put the dead container's colour back on the replacement"
        );
    }

    /// The off-screen case, which is the one a live-parser reset misses
    /// entirely rather than merely temporarily. While a store is released (#58)
    /// `process` skips the parser altogether, so a recreate that happens with
    /// no pane open has only `raw` to record it in, and the reset has to still
    /// be there when someone looks.
    ///
    /// The premise is asserted, not assumed. `release` declines a store it
    /// could not rebuild, so if its conditions ever change this would quietly
    /// stop being about the released path and become a second copy of the
    /// replay test above.
    #[test]
    fn a_recreate_while_released_still_resets_the_emulator() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.adopt("web-1-first", 1);
        s.process(b"\x1b[31mtail\x1b[3");

        s.release();
        assert_eq!(
            s.released_grid().size(),
            (MIN_ROWS, MIN_COLS),
            "the store was never released, so this is only the replay test again"
        );

        s.adopt("web-1-second", 1);
        s.process(b"new line\n");
        s.resize(10, 40);

        assert_eq!(
            non_empty(&s),
            vec!["tail", "new line"],
            "a recreate that happened off-screen left the sequence open"
        );
        let replacement = s.screen().cell(1, 0).expect("the replacement's row");
        assert_eq!(
            replacement.fgcolor(),
            vt100::Color::Default,
            "a recreate that happened off-screen left the pen set"
        );
    }

    /// A store nothing has written to has no emulator state to reset and must
    /// not be given bytes for it. The handover would be the first bytes the
    /// store ever received, and `process` marks any write as output -- so the
    /// pane would swap its "waiting" placeholder for a blank screen before any
    /// container had written a byte.
    ///
    /// `the_first_container_to_write_gets_no_leading_break` does not catch
    /// this: it counts newlines in `raw`, and the handover carries none.
    #[test]
    fn adopting_a_store_nothing_has_written_to_writes_nothing() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);

        s.adopt("web-1-first", 1);

        assert!(
            s.raw.is_empty(),
            "the handover was written before any container had"
        );
        assert!(
            !s.has_output(),
            "the pane's waiting placeholder was replaced by a blank screen"
        );
    }

    /// A recreate is not a reason to move a reader who has scrolled up. The
    /// break adds a row, and `vt100` advances the scroll offset as rows evict
    /// into scrollback so that the same content stays in view -- the same
    /// mechanism `process` relies on for ordinary output, which `LogStore`
    /// deliberately does not compensate for by hand.
    ///
    /// Asserted on the rendered rows rather than on the offset, because it is
    /// the rows the reader is looking at, and the offset changing while they
    /// hold still is the correct outcome rather than the failure.
    #[test]
    fn a_recreate_does_not_move_a_scrolled_up_reader() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.adopt("web-1-first", 1);
        for i in 0..50 {
            s.process(format!("line {i}\n").as_bytes());
        }
        s.process(b"held tail");
        s.scroll_up(20);
        // The premise: the reader really is up in the scrollback, not pinned at
        // the bottom where nothing could move them anyway.
        assert!(s.scroll_offset() > 0, "the reader never left the bottom");

        let before = s.visible_lines();
        s.adopt("web-1-second", 1);

        assert_eq!(
            s.visible_lines(),
            before,
            "a recreate moved a scrolled-up reader's view"
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Visible rows with blank padding removed.
    fn text(store: &mut LogStore) -> String {
        store.all_text()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

        /// The invariant behind three separate defects: widening must never be
        /// an *additional* source of loss on top of ordinary eviction.
        ///
        /// Only widening. Retention is counted in rows, so narrowing legitimately
        /// evicts content -- the same text needs more rows once it wraps harder,
        /// and the oldest of it falls out of a fixed-row scrollback exactly as it
        /// would in a real terminal. The property test found that distinction
        /// before I did.
        ///
        /// Example-based tests could not cover this: the byte cost of a visual
        /// row depends on content an author has to think to write, which is how
        /// colour-dense output slipped past five hand-written cases.
        #[test]
        fn widening_never_drops_content_that_was_still_present(
            n_lines in 5usize..300,
            filler in 1usize..120,
            colourise in prop::bool::ANY,
            scrollback in 20usize..300,
            cols_before in 20u16..120,
            widen_by in 0u16..80,
        ) {
            let cols_after = cols_before.saturating_add(widen_by);
            let mut store = LogStore::new(scrollback);
            store.resize(24, cols_before);
            for i in 0..n_lines {
                let mut line = format!("MARK{i:05} ");
                for c in 0..filler {
                    if colourise {
                        line.push_str(if c % 2 == 0 { "\x1b[31m" } else { "\x1b[32m" });
                    }
                    line.push('x');
                }
                store.process(format!("{line}\n").as_bytes());
            }

            let before = text(&mut store);
            store.resize(24, cols_after);
            let after = text(&mut store);

            for i in 0..n_lines {
                let marker = format!("MARK{i:05}");
                if before.contains(&marker) {
                    prop_assert!(after.contains(&marker), "widening dropped {marker}");
                }
            }

            // Whichever way the pane moves, the newest line is never the one lost.
            let newest = format!("MARK{:05}", n_lines - 1);
            prop_assert!(after.contains(&newest), "newest line lost: {newest}");
        }

        /// The carried pen must reproduce every attribute the emulator tracks,
        /// compared as a whole rather than one the author remembered to check.
        /// This is what makes a missing attribute fail without anyone writing a
        /// test for that specific attribute.
        #[test]
        fn the_carried_pen_reproduces_every_attribute(
            codes in prop::collection::vec(0usize..11, 1..24),
            split_pct in 0usize..=100,
        ) {
            const SGR: [&str; 11] = [
                "\x1b[0m", "\x1b[1m", "\x1b[2m", "\x1b[3m", "\x1b[4m", "\x1b[7m",
                "\x1b[31m", "\x1b[42m", "\x1b[38;5;200m", "\x1b[48;2;10;20;30m", "\x1b[91m",
            ];
            fn attrs(cell: &vt100::Cell) -> (bool, bool, bool, bool, bool, vt100::Color, vt100::Color) {
                (
                    cell.bold(), cell.dim(), cell.italic(), cell.underline(),
                    cell.inverse(), cell.fgcolor(), cell.bgcolor(),
                )
            }

            let bytes: Vec<u8> = codes.iter().map(|i| SGR[*i]).collect::<String>().into_bytes();
            let split = bytes.len() * split_pct / 100;
            let (prefix, dropped) = bytes.split_at(split);

            let mut reference = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
            reference.process(prefix);
            reference.process(dropped);
            reference.process(b"x");
            let want = attrs(reference.screen().cell(0, 0).unwrap());

            let mut replayed = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
            replayed.process(&pen_after(prefix, dropped));
            replayed.process(b"x");
            let got = attrs(replayed.screen().cell(0, 0).unwrap());

            prop_assert_eq!(got, want);
        }

        /// Footprint stays bounded for arbitrary writes, rather than for the one
        /// multiplier a bug report happened to use.
        #[test]
        fn the_retained_buffer_stays_bounded(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..2000), 1..24),
            scrollback in 4usize..48,
            rows in 3u16..24,
            cols in 20u16..120,
        ) {
            let mut store = LogStore::new(scrollback);
            store.resize(rows, cols);
            for chunk in &chunks {
                store.process(chunk);
                prop_assert!(store.raw.len() <= MAX_RAW_BYTES);
                prop_assert!(
                    store.raw.capacity() <= MAX_RAW_BYTES.saturating_mul(2).max(4096),
                    "capacity {} grew unboundedly",
                    store.raw.capacity()
                );
                prop_assert!(
                    !store.raw.is_empty() || !store.has_output(),
                    "replay data was discarded wholesale"
                );
                prop_assert!(
                    store.lines <= store.keep_lines.saturating_mul(2),
                    "line budget exceeded: {} retained against a budget of {}",
                    store.lines,
                    store.keep_lines
                );
            }
        }

        /// Arbitrary bytes and arbitrary geometry, in any order, must not panic.
        /// Generalises a fixed list of widths that once guarded a vt100
        /// underflow.
        #[test]
        fn arbitrary_input_and_geometry_never_panics(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..600), 0..12),
            sizes in prop::collection::vec((0u16..200, 0u16..200), 1..8),
            scrollback in 0usize..64,
        ) {
            let mut store = LogStore::new(scrollback);
            for (rows, cols) in &sizes {
                store.resize(*rows, *cols);
                for chunk in &chunks {
                    store.process(chunk);
                }
                let _ = store.all_text();
            }
        }
    }
}
