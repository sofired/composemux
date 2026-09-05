//! Draws one frame.

use crate::tui::app::App;
use crate::tui::components::services_list::uptime_text;
use crate::tui::components::{countdown_popup, help_popup, log_pane, services_list, status_bar};
use crate::tui::focus::{Focus, MAX_PANES};
use crate::tui::layout_manager::{is_too_small, LayoutAreas};
use ratatui::Frame;

/// Computes this frame's geometry and the emulator size each pane needs.
///
/// Sizes are keyed by pane *slot*, not by rect position. The layout emits one
/// rect per occupied slot, so with only pane 2 pinned the single rect belongs to
/// slot 1 — reporting it as slot 0 left that pane's emulator at its default
/// size, wrapping at a width the pane never had.
pub fn layout_for(app: &App, area: ratatui::layout::Rect) -> (LayoutAreas, Vec<(usize, u16, u16)>) {
    let areas = app.layout().calculate(area, status_bar::HEIGHT);
    let sizes = areas
        .panes
        .iter()
        .zip(app.occupied_panes())
        .map(|(rect, slot)| {
            let (rows, cols) = log_pane::inner_size(*rect);
            (slot, rows, cols)
        })
        .collect();
    (areas, sizes)
}

/// Renders one frame: the service list, any open output panes, the status bar,
/// and whichever popup is on top.
///
/// Pane rects are matched to pane *slots* rather than to their own position,
/// because the layout emits one rect per occupied slot — with only pane 2
/// pinned there is a single rect, and it belongs to slot 1.
pub fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    if is_too_small(area) {
        services_list::render_too_small(&app.project, area, buf);
        return;
    }

    let areas = app.layout().calculate(area, status_bar::HEIGHT);

    if let Some(list_area) = areas.service_list {
        services_list::render(app, list_area, buf);
    }

    // The next pane `tab` would move to, so the hint appears in the right place.
    let next_tab_target = next_tab_pane(app);

    // One rect per *occupied* slot, so position and slot are not the same
    // number: pinning only to pane 2 leaves slot 0 empty and yields one rect.
    for (rect, idx) in areas.panes.iter().zip(app.occupied_panes()) {
        let key = app.pane_key(idx);
        // Resolved against every service, not the filtered rows: a pinned pane
        // keeps streaming a service the filter has hidden.
        let row = key.as_ref().and_then(|k| app.service_row(k));
        let (title, status, uptime) = match row {
            Some(r) => (
                r.display_name.clone(),
                r.service.status,
                Some(uptime_text(r)),
            ),
            None => (
                key.as_ref()
                    .map(|k| k.name.clone())
                    .unwrap_or_else(|| "—".to_string()),
                crate::model::ServiceStatus::NotStarted,
                None,
            ),
        };
        let pane = log_pane::PaneRender {
            title: &title,
            status,
            focused: app.focus() == Focus::Pane(idx),
            store: key.as_ref().and_then(|k| app.store(k)),
            uptime,
            throbber: app.throbber(),
            tab_hint: next_tab_target == Some(idx),
        };
        log_pane::render(&pane, *rect, buf);
    }

    if let Some(bar) = areas.status_bar {
        status_bar::render(app, bar, buf);
    }

    match app.focus() {
        Focus::HelpPopup => help_popup::render(&app.project, area, buf),
        Focus::CountdownPopup => {
            if let Some(remaining) = app.countdown_remaining() {
                countdown_popup::render(app, remaining, area, buf);
            }
        }
        _ => {}
    }
}

/// Which pane `tab` would focus next, for the pane title hint.
fn next_tab_pane(app: &App) -> Option<usize> {
    if !app.has_visible_panes() {
        return None;
    }
    match app.focus() {
        Focus::ServiceList => (0..MAX_PANES).find(|i| app.pane_key(*i).is_some()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Health, Service, ServiceStatus};
    use crate::tui::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// A service in a given state, with the fields tests do not care about
    /// left at their defaults.
    /// The first frame of the running throbber, and the glyph a pane shows
    /// when it cannot resolve its service at all.
    const RUNNING_THROBBER: char = '\u{280b}';
    const NOT_STARTED_GLYPH: char = '\u{00b7}';

    fn service(name: &str, status: ServiceStatus) -> Service {
        Service {
            name: name.to_string(),
            replica: 1,
            status,
            health: Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        }
    }

    /// An app with one running, one failed and one succeeded service, which
    /// exercises every status colour and glyph at once.
    fn app() -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![
            service("api", ServiceStatus::Running),
            service("worker", ServiceStatus::Failure),
            service("db", ServiceStatus::Success),
        ]);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            std::time::Instant::now(),
        );
    }

    /// Renders a frame and returns it as plain text, one string per row.
    fn render_to_lines(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol())
                            .unwrap_or(" ")
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect()
    }

    /// The rendered frame as a single string, for substring assertions.
    fn render_to_text(app: &App, width: u16, height: u16) -> String {
        render_to_lines(app, width, height).join("\n")
    }

    #[test]
    fn the_service_list_shows_every_service_and_the_project_badge() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains("api"), "missing api:\n{text}");
        assert!(text.contains("worker"));
        assert!(text.contains("db"));
        assert!(text.contains("DEMO"), "missing project badge:\n{text}");
    }

    #[test]
    fn the_status_bar_shows_key_hints() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains("help: ?"), "missing help hint:\n{text}");
        assert!(text.contains("quit: q"));
    }

    #[test]
    fn the_selected_row_is_marked_with_a_caret() {
        let lines = render_to_lines(&app(), 120, 40);
        let marked: Vec<_> = lines
            .iter()
            .filter(|l| l.trim_start().starts_with('>'))
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "exactly one row should be marked:\n{lines:#?}"
        );
    }

    #[test]
    fn status_glyphs_reflect_each_services_state() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains('✖'), "failed service needs a cross:\n{text}");
        assert!(
            text.contains('✔'),
            "succeeded service needs a tick:\n{text}"
        );
    }

    #[test]
    fn a_frame_below_the_minimum_shows_only_the_too_small_notice() {
        let text = render_to_text(&app(), 30, 8);
        assert!(text.contains("Terminal too small"), "got:\n{text}");
        assert!(!text.contains("help: ?"));
    }

    #[test]
    fn the_minimum_supported_frame_still_renders_the_ui() {
        let text = render_to_text(&app(), 40, 10);
        assert!(!text.contains("Terminal too small"), "got:\n{text}");
    }

    #[test]
    fn pinning_opens_a_bordered_pane_titled_with_the_service() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains('│'), "expected a pane border:\n{text}");
        // The service appears both in the list and as the pane title.
        assert!(text.matches("api").count() >= 2, "got:\n{text}");
    }

    #[test]
    fn a_wide_frame_places_the_pane_beside_the_list() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let lines = render_to_lines(&app, 160, 40);
        // Horizontal layout: the list is a third of the width, so a border
        // appears to the right of it on the same row.
        let border_row = lines
            .iter()
            .find(|l| l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        let x = border_row.find(['┌', '╭']).unwrap();
        assert!(x > 40, "the pane should start past the list, found at {x}");
    }

    #[test]
    fn a_narrow_frame_stacks_the_pane_below_the_list() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let lines = render_to_lines(&app, 60, 40);
        let border_y = lines
            .iter()
            .position(|l| l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        assert!(
            border_y > 5,
            "the pane should sit below the list, at {border_y}"
        );
    }

    #[test]
    fn pinning_only_to_pane_two_renders_that_service() {
        // One pin means one rect, but the service sits in slot 1 -- feeding that
        // rect slot 0 drew an empty placeholder pane that never filled.
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        app.ingest(
            crate::tui::app::ServiceKey::new("api", 1),
            b"output from api\n",
        );
        let (_, sizes) = layout_for(&app, Rect::new(0, 0, 160, 40));
        app.resize_panes(&sizes);
        let text = render_to_text(&app, 160, 40);

        assert!(
            !text.contains("Waiting for output"),
            "the pane should be showing the pinned service:\n{text}"
        );
        assert!(text.contains("output from api"), "got:\n{text}");
    }

    #[test]
    fn a_sparsely_pinned_pane_gets_its_emulator_sized() {
        // The content half of this was fixed by mapping rects to slots in draw;
        // layout_for reported the same rect as slot 0, so resize_panes skipped
        // the service in slot 1 and its emulator stayed at the default size,
        // wrapping at a width the pane never had.
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        let frame = Rect::new(0, 0, 160, 40);
        let (areas, sizes) = layout_for(&app, frame);
        app.resize_panes(&sizes);

        let key = crate::tui::app::ServiceKey::new("api", 1);
        app.ingest(key.clone(), b"x\n");
        let (rows, cols) = app
            .store(&key)
            .expect("the pinned service should have a buffer")
            .screen()
            .size();

        let (want_rows, want_cols) = log_pane::inner_size(areas.panes[0]);
        assert_eq!(
            (rows, cols),
            (want_rows, want_cols),
            "the emulator should match the pane it is drawn in"
        );
    }

    #[test]
    fn a_pinned_pane_keeps_its_header_when_the_filter_hides_the_service() {
        // The header used to be resolved through the visible rows, so filtering
        // the pinned service out degraded its title and uptime.
        let mut app = app();
        // Status and uptime are the only things that actually differ between
        // the two paths, so the service needs a running clock to assert on.
        // Two minutes is coarse enough that the assertion cannot race the
        // test: `duration()` measures a running service against `now`.
        let mut api = service("api", ServiceStatus::Running);
        api.started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(120));
        app.set_services(vec![
            api,
            service("worker", ServiceStatus::Failure),
            service("db", ServiceStatus::Success),
        ]);
        press(&mut app, KeyCode::Char('1'));
        app.ingest(crate::tui::app::ServiceKey::new("api", 1), b"still here\n");
        press(&mut app, KeyCode::Char('/'));
        for c in "zzz".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.rows().is_empty(), "the filter should match nothing");

        let (_, sizes) = layout_for(&app, Rect::new(0, 0, 160, 40));
        app.resize_panes(&sizes);
        let text = render_to_text(&app, 160, 40);
        // The title alone proves nothing: the degraded path falls back to the
        // pinned key's name, which is also "api". Status and uptime are what
        // separate the two.
        assert!(
            text.contains("api"),
            "the pane title should survive the filter:\n{text}"
        );
        assert!(text.contains("still here"));
        assert!(
            text.contains(RUNNING_THROBBER),
            "the running throbber should survive the filter:\n{text}"
        );
        assert!(
            !text.contains(NOT_STARTED_GLYPH),
            "the pane degraded to NotStarted:\n{text}"
        );
        assert!(
            text.contains("2m"),
            "the uptime should survive the filter:\n{text}"
        );
    }

    #[test]
    fn two_pinned_services_render_two_panes() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        let lines = render_to_lines(&app, 160, 40);
        let borders = lines
            .iter()
            .filter(|l| l.contains('┌') || l.contains('╭'))
            .count();
        assert_eq!(borders, 2, "expected two pane tops:\n{lines:#?}");
    }

    #[test]
    fn pin_indicators_appear_next_to_pinned_services() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("[1]"), "expected a pin indicator:\n{text}");
    }

    #[test]
    fn the_help_popup_lists_the_bindings() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("Help"), "got:\n{text}");
        assert!(text.contains("Pin service to output pane 1"));
    }

    #[test]
    fn the_filter_query_is_shown_while_typing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("/a"), "got:\n{text}");
        assert!(text.contains("filtered out"));
    }

    #[test]
    fn filtering_removes_non_matching_services_from_the_frame() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('w'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("worker"));
        assert!(!text.contains(" db "), "db should be filtered out:\n{text}");
    }

    #[test]
    fn log_output_is_visible_in_a_pane() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        app.ingest(
            crate::tui::app::ServiceKey::new("api", 1),
            b"hello from the container\r\n",
        );
        let (_, sizes) = layout_for(&app, Rect::new(0, 0, 160, 40));
        app.resize_panes(&sizes);
        let text = render_to_text(&app, 160, 40);
        assert!(text.contains("hello from the container"), "got:\n{text}");
    }

    #[test]
    fn widening_the_frame_rewraps_log_output() {
        // Exercises the real wiring: layout_for -> resize_panes ->
        // LogStore::resize -> render. A unit test on LogStore alone would not
        // catch the panes being sized from stale geometry.
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let key = crate::tui::app::ServiceKey::new("api", 1);
        let long = "X".repeat(150);
        app.ingest(key, format!("{long}\n").as_bytes());

        let narrow = Rect::new(0, 0, 90, 30);
        let (_, sizes) = layout_for(&app, narrow);
        app.resize_panes(&sizes);
        let before = render_to_lines(&app, 90, 30);
        let widest_before = before
            .iter()
            .filter(|l| l.contains("XXX"))
            .map(|l| l.chars().filter(|c| *c == 'X').count())
            .max()
            .unwrap_or(0);

        let wide = Rect::new(0, 0, 200, 30);
        let (_, sizes) = layout_for(&app, wide);
        app.resize_panes(&sizes);
        let after = render_to_lines(&app, 200, 30);
        let widest_after = after
            .iter()
            .filter(|l| l.contains("XXX"))
            .map(|l| l.chars().filter(|c| *c == 'X').count())
            .max()
            .unwrap_or(0);

        assert!(
            widest_after > widest_before,
            "widening should rewrap history to the new width: {widest_before} -> {widest_after}"
        );
    }

    #[test]
    fn log_output_starts_at_the_left_edge() {
        // Container logs are LF-terminated; without normalisation each line
        // would begin where the previous one ended.
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        app.ingest(
            crate::tui::app::ServiceKey::new("api", 1),
            b"alpha\nbravo\ncharlie\n",
        );
        let (_, sizes) = layout_for(&app, Rect::new(0, 0, 160, 40));
        app.resize_panes(&sizes);
        let lines = render_to_lines(&app, 160, 40);

        // Column, not byte offset: rows carry multi-byte glyphs (the throbber,
        // status marks, the pane border) so the two are not the same number.
        let col = |needle: &str| {
            lines
                .iter()
                .find_map(|l| l.find(needle).map(|byte| l[..byte].chars().count()))
                .expect("line should be rendered")
        };
        assert_eq!(col("alpha"), col("bravo"));
        assert_eq!(
            col("alpha"),
            col("charlie"),
            "every line should start in the same column"
        );
    }

    #[test]
    fn a_pane_with_no_output_yet_says_so() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 160, 40);
        assert!(text.contains("Waiting for output"), "got:\n{text}");
    }

    #[test]
    fn hiding_the_list_leaves_only_the_pane() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('b'));
        let lines = render_to_lines(&app, 160, 40);
        let border_row = lines
            .iter()
            .find(|l| l.contains('┏') || l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        let x = border_row.find(['┏', '┌', '╭']).unwrap();
        assert_eq!(x, 0, "with the list hidden the pane starts at column 0");
    }

    #[test]
    /// Rendered rather than asserted on state: the point of the mode is what
    /// reaches the frame, and only the frame can show the list and the sibling
    /// are actually gone.
    fn a_full_screen_pane_is_the_only_thing_on_the_frame() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        let lines = render_to_lines(&app, 160, 40);
        let tops: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains('┏') || l.contains('┌') || l.contains('╭'))
            .map(|(y, _)| y)
            .collect();
        assert_eq!(
            tops.len(),
            1,
            "only the full-screen pane should be drawn:\n{lines:#?}"
        );
        let border_row = &lines[tops[0]];
        assert_eq!(
            border_row.find(['┏', '┌', '╭']).unwrap(),
            0,
            "the pane should start at column 0"
        );
        let text = lines.join("\n");
        assert!(text.contains("api"), "got:\n{text}");
        assert!(
            !text.contains("worker"),
            "the second pane and the list should be gone:\n{text}"
        );
    }

    #[test]
    /// Full screen is the first state in which an occupied pane goes
    /// unrendered, so it stops being resized while it keeps ingesting. Shrink
    /// the terminal meanwhile and its emulator is a size behind; only the
    /// rebuild from raw bytes on the next resize puts that right.
    fn a_pane_hidden_by_full_screen_rewraps_when_it_comes_back() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        let wide = Rect::new(0, 0, 200, 40);
        let narrow = Rect::new(0, 0, 90, 40);
        let resize = |app: &mut App, frame: Rect| {
            let (_, sizes) = layout_for(app, frame);
            app.resize_panes(&sizes);
        };
        resize(&mut app, wide);

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        resize(&mut app, wide);
        assert!(
            !render_to_text(&app, 200, 40).contains("worker"),
            "the sibling pane should be off the frame while full screen"
        );

        // Output arrives, and the terminal shrinks, while the pane is hidden.
        let long = "W".repeat(120);
        app.ingest(
            crate::tui::app::ServiceKey::new("worker", 1),
            format!("{long}\n").as_bytes(),
        );
        resize(&mut app, narrow);

        press(&mut app, KeyCode::Esc);
        resize(&mut app, narrow);
        let lines = render_to_lines(&app, 90, 40);
        let rows = lines.iter().filter(|l| l.contains("WWW")).count();
        assert!(
            rows >= 2,
            "the hidden pane's output should be back and rewrapped to the \
             narrow frame, but it occupies {rows} row(s):\n{lines:#?}"
        );
    }

    #[test]
    fn the_countdown_popup_summarises_the_exited_stack() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![
            service("api", ServiceStatus::Success),
            service("db", ServiceStatus::Success),
        ]);
        app.tick(std::time::Instant::now());
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("All services exited"), "got:\n{text}");
        assert!(text.contains("Closing in"), "got:\n{text}");
        assert!(text.contains("api") && text.contains("db"));
    }

    #[test]
    fn a_crashed_stack_shows_no_countdown_popup() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![service("api", ServiceStatus::Failure)]);
        app.tick(std::time::Instant::now());
        let text = render_to_text(&app, 120, 40);
        assert!(
            !text.contains("Closing in"),
            "a failed stack must stay open:\n{text}"
        );
    }

    #[test]
    fn the_status_bar_warns_while_the_countdown_runs() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![service("api", ServiceStatus::Success)]);
        app.tick(std::time::Instant::now());
        // Wide enough that the middle slot isn't truncated.
        let lines = render_to_lines(&app, 200, 40);
        let bar = lines.last().expect("a status bar row");
        assert!(
            bar.contains("All services exited") && bar.contains("any key cancels"),
            "status bar was: {bar:?}"
        );
    }

    #[test]
    fn a_scrollbar_does_not_eat_the_last_column_of_a_value() {
        // The scrollbar used to be drawn over the rightmost column, turning
        // "683ms" into "683m" -- a wrong value that reads like a right one.
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);

        // Finished services, so both ends of the window are fixed and the
        // duration is exactly 683ms however long the render takes. Measuring a
        // *running* service against `now` put the fixture 317ms below the
        // boundary where `format_duration` switches to "1.0s" -- close enough
        // that a slow run would have failed here claiming the scrollbar had
        // truncated a value that was never rendered in milliseconds at all.
        let finished = chrono::Utc::now();
        let started = finished - chrono::Duration::milliseconds(683);
        let mut services = Vec::new();
        for i in 0..40 {
            let mut svc = service(&format!("svc{i:02}"), ServiceStatus::Success);
            svc.started_at = Some(started);
            svc.finished_at = Some(finished);
            svc.exit_code = Some(0);
            services.push(svc);
        }
        app.set_services(services);

        let lines = render_to_lines(&app, 120, 20);
        assert!(
            lines.iter().any(|l| l.contains("683ms")),
            "the uptime lost its unit to the scrollbar:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn the_project_badge_shows_the_whole_name_when_there_is_room() {
        let cfg = Config::default();
        let mut app = App::new("digital-university", &cfg);
        app.set_services(vec![service("api", ServiceStatus::Running)]);
        let text = render_to_text(&app, 140, 20);
        assert!(
            text.contains("DIGITAL-UNIVERSITY"),
            "the badge was clipped:\n{text}"
        );
    }

    #[test]
    fn an_empty_project_renders_without_panicking() {
        let cfg = Config::default();
        let app = App::new("empty", &cfg);
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("EMPTY"));
    }

    #[test]
    fn rendering_is_stable_across_a_range_of_sizes() {
        // Guards against panics from arithmetic on very small or odd frames.
        for (w, h) in [(40, 10), (41, 11), (60, 50), (80, 24), (200, 60), (120, 12)] {
            let mut app = app();
            press(&mut app, KeyCode::Char('1'));
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Char('2'));
            let _ = render_to_lines(&app, w, h);
        }
    }
}
