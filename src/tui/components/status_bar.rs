//! The bottom bar: progress on the left, context in the middle, keys on the right.
//!
//! Ported from nx `packages/nx/src/native/tui/components/status_bar.rs` and
//! `help_text.rs` (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::model::ServiceStatus;
use crate::tui::app::App;
use crate::tui::filter::FilterState;
use crate::tui::focus::Focus;
use crate::tui::theme::THEME;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const MIN_HELP_WIDTH: u16 = 16;
const BOTTOM_SPACING: u16 = 4;
const RIGHT_MARGIN: u16 = 1;
const STATUS_MIN_WIDTH: u16 = 25;

pub const HEIGHT: u16 = 1;

/// Right-hand key hints, in drop order: the leftmost goes first as space runs out.
fn help_items(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.focus() {
        // The two labels are nx's own: "full screen: <enter>" on a focused
        // pane, "exit: esc" once that pane has the frame. nx shows the exit
        // hint on its own, but only because its full screen is a separate
        // stripped-down app; here the rest of the row is still true.
        Focus::Pane(_) if app.full_screen_pane().is_some() => vec![
            ("scroll: ", "↑ ↓"),
            ("copy: ", "c"),
            ("exit: ", "esc"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
        Focus::Pane(_) => vec![
            ("scroll: ", "↑ ↓"),
            ("copy: ", "c"),
            ("full screen: ", "<enter>"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
        _ => vec![
            ("pin output: ", "1 or 2"),
            ("show output: ", "<enter>"),
            ("filter: ", "/"),
            ("navigate: ", "↑ ↓"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
    }
}

/// Builds the hint line, dropping leading items until it fits.
fn help_line(app: &App, available: u16) -> Line<'static> {
    let items = help_items(app);
    for start in 0..items.len() {
        let spans = render_items(&items[start..]);
        let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if width as u16 <= available {
            return Line::from(spans);
        }
    }
    Line::from("")
}

fn render_items(items: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (label, key)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *label,
            Style::default().fg(THEME.secondary_fg),
        ));
        spans.push(Span::styled(*key, Style::default().fg(THEME.info)));
    }
    spans
}

/// Left slot: how many services are up, and the project name.
fn status_line(app: &App) -> Line<'static> {
    let total = app.rows().len();
    if total == 0 {
        return Line::from("");
    }
    let running = app
        .rows()
        .iter()
        .filter(|r| {
            matches!(
                r.service.status,
                ServiceStatus::Running | ServiceStatus::Unhealthy
            )
        })
        .count();
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{running}/{total}"),
            Style::default().fg(THEME.secondary_fg),
        ),
        Span::styled(" up", Style::default().add_modifier(Modifier::DIM)),
    ])
}

/// Middle slot, in precedence order: transient message, countdown, then filter.
fn context_line(app: &App) -> Line<'static> {
    if let Some(message) = app.status_message() {
        return Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(THEME.info),
        ));
    }
    if let Some(secs) = app.countdown_remaining() {
        return Line::from(Span::styled(
            format!("All services exited - closing in {secs}s (any key cancels)"),
            Style::default().fg(THEME.warning),
        ));
    }
    match app.filter().state() {
        FilterState::Editing => Line::from(vec![
            Span::styled(
                format!("/{}", app.filter().query()),
                Style::default().fg(THEME.info),
            ),
            Span::styled(
                format!(
                    "  {} filtered out   <enter> confirm, <esc> cancel",
                    app.hidden_count()
                ),
                Style::default().fg(THEME.secondary_fg),
            ),
        ]),
        FilterState::Persisted => Line::from(vec![
            Span::styled(
                format!("/{}", app.filter().query()),
                Style::default().fg(THEME.info),
            ),
            Span::styled(
                format!("  {} hidden (/ to edit)", app.hidden_count()),
                Style::default().fg(THEME.secondary_fg),
            ),
        ]),
        FilterState::Off => Line::from(""),
    }
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let status = status_line(app);
    let status_width = (status.width() as u16).clamp(1, STATUS_MIN_WIDTH);
    // Floor the budget at the minimum hint width, but never above the space
    // that actually exists, or the hints would overflow instead of degrading.
    let available = area
        .width
        .saturating_sub(status_width + BOTTOM_SPACING + RIGHT_MARGIN);
    let help_budget = available.max(MIN_HELP_WIDTH).min(available.max(1));
    let help = help_line(app, help_budget);
    let help_width = help.width() as u16;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(status_width),
            Constraint::Length(BOTTOM_SPACING),
            Constraint::Fill(1),
            Constraint::Length(help_width + RIGHT_MARGIN),
        ])
        .split(area);

    Widget::render(Paragraph::new(status), chunks[0], buf);
    Widget::render(Paragraph::new(context_line(app)), chunks[2], buf);
    Widget::render(Paragraph::new(help).right_aligned(), chunks[3], buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Health, Service};

    fn app_with(names: &[&str]) -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(
            names
                .iter()
                .map(|n| Service {
                    name: n.to_string(),
                    replica: 1,
                    status: ServiceStatus::Running,
                    health: Health::None,
                    exit_code: None,
                    started_at: None,
                    finished_at: None,
                })
                .collect(),
        );
        app
    }

    #[test]
    fn the_hint_line_drops_items_from_the_left_when_cramped() {
        let app = app_with(&["a"]);
        let wide = help_line(&app, 200);
        let narrow = help_line(&app, 20);
        assert!(narrow.width() < wide.width());
        let text: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("help: ?"), "the last items survive: {text}");
    }

    #[test]
    fn an_impossibly_narrow_bar_renders_nothing_rather_than_overflowing() {
        let app = app_with(&["a"]);
        assert_eq!(help_line(&app, 1).width(), 0);
    }

    #[test]
    fn pane_focus_shows_pane_specific_hints() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("scroll"));
        assert!(text.contains("copy"));
        assert!(!text.contains("pin output"));
    }

    #[test]
    /// The status bar is where the binding is found in passing, so it has to
    /// carry nx's wording rather than our own.
    fn a_focused_pane_advertises_full_screen() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("full screen: <enter>"), "got: {text}");
    }

    #[test]
    /// The bar follows the mode: once inside, the useful hint is how to leave.
    fn a_full_screen_pane_advertises_the_way_out() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("exit: esc"), "got: {text}");
        assert!(
            !text.contains("full screen: <enter>"),
            "the pane is already full screen: {text}"
        );
    }

    #[test]
    fn the_status_slot_counts_running_services() {
        let app = app_with(&["a", "b"]);
        let text: String = status_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2/2 up"), "got {text}");
    }

    #[test]
    fn an_empty_stack_shows_no_status() {
        let cfg = Config::default();
        let app = App::new("demo", &cfg);
        assert_eq!(status_line(&app).width(), 0);
    }

    #[test]
    fn the_filter_query_appears_in_the_context_slot() {
        let mut app = app_with(&["api", "worker"]);
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('/'),
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.starts_with("/a"), "got {text}");
        assert!(text.contains("filtered out"));
    }

    #[test]
    fn a_transient_message_outranks_the_filter() {
        let mut app = app_with(&["a"]);
        app.set_status_message("Output copied");
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "Output copied");
    }
}
