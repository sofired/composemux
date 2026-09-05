//! The `?` popup listing every binding.
//!
//! Ported from nx `packages/nx/src/native/tui/components/help_popup.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::tui::theme::THEME;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget};

/// Key/description pairs. An empty pair renders as a spacer, as in nx.
pub const BINDINGS: &[(&str, &str)] = &[
    ("?", "Toggle this popup"),
    ("q or <ctrl>+c", "Quit"),
    ("", ""),
    ("↑ or k", "Navigate/scroll output up"),
    ("↓ or j", "Navigate/scroll output down"),
    ("<ctrl>+u", "Scroll output up"),
    ("<ctrl>+d", "Scroll output down"),
    ("Home or End", "Jump to the start or end of output"),
    ("", ""),
    ("/", "Filter services based on search term"),
    ("<esc>", "Clear filter"),
    ("", ""),
    ("<enter>", "Open and focus output for service"),
    ("<enter>", "Full screen the focused output pane"),
    ("<esc>", "Leave full screen"),
    ("<esc>", "Set focus back to service list"),
    ("<space>", "Quick toggle a single output pane"),
    ("b", "Toggle service list visibility"),
    ("m", "Toggle between vertical and horizontal layouts"),
    ("1", "Pin service to output pane 1"),
    ("2", "Pin service to output pane 2"),
    ("0", "Clear all output panes"),
    ("<tab>", "Move focus between list and output panes"),
    ("c", "Copy output to clipboard"),
    ("F10", "Toggle mouse capture"),
];

/// Centres a box of the given proportions inside `area`.
pub fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub fn render(project: &str, area: Rect, buf: &mut Buffer) {
    let popup = centered(70, 85, area);
    Widget::render(Clear, popup, buf);

    let title = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(" {} ", project.to_uppercase()),
            Style::reset()
                .add_modifier(Modifier::BOLD)
                .bg(THEME.info)
                .fg(THEME.primary_fg),
        ),
        Span::styled("  Help  ", Style::default().fg(THEME.primary_fg)),
    ]);
    let dismiss = Line::from(vec![
        Span::styled(" (esc) ", Style::default().fg(THEME.secondary_fg)),
        Span::styled("✕  ", Style::default().fg(THEME.info)),
    ])
    .right_aligned();

    let block = Block::default()
        .title(title)
        .title_top(dismiss)
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(THEME.info))
        .padding(Padding::proportional(1));

    let lines: Vec<Line> = BINDINGS.iter().map(|(k, d)| binding_line(k, d)).collect();
    Widget::render(Paragraph::new(lines).block(block), popup, buf);
}

/// `<key>` in the accent colour, padded to a column, then `=` and the description.
fn binding_line<'a>(key: &'a str, description: &'a str) -> Line<'a> {
    if key.is_empty() && description.is_empty() {
        return Line::from("");
    }
    let mut spans = Vec::new();
    let mut visible = 0usize;
    for (i, part) in key.split(" or ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " or ",
                Style::default().fg(THEME.secondary_fg),
            ));
            visible += 4;
        }
        spans.push(Span::styled(
            part.to_string(),
            Style::default().fg(THEME.info),
        ));
        visible += part.chars().count();
    }
    spans.push(Span::raw(" ".repeat(14usize.saturating_sub(visible))));
    spans.push(Span::styled(
        "=   ",
        Style::default().fg(THEME.secondary_fg),
    ));
    spans.push(Span::styled(
        description,
        Style::default().fg(THEME.primary_fg),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_popup_is_centred() {
        let area = Rect::new(0, 0, 100, 100);
        let popup = centered(70, 85, area);
        assert_eq!(popup.width, 70);
        assert_eq!(popup.height, 85);
        assert_eq!(popup.x, 15);
    }

    #[test]
    fn spacer_entries_render_as_blank_lines() {
        assert_eq!(binding_line("", "").width(), 0);
    }

    #[test]
    fn alternative_keys_are_split_on_or() {
        let line = binding_line("↑ or k", "Up");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("↑ or k"));
        assert!(text.ends_with("Up"));
    }

    #[test]
    /// A binding users cannot discover is not shipped, and one with no
    /// advertised way out is worse than absent.
    fn the_full_screen_binding_and_its_exit_are_both_documented() {
        assert!(
            BINDINGS
                .iter()
                .any(|(k, d)| *k == "<enter>" && d.contains("Full screen")),
            "the popup should list how to full screen a pane"
        );
        assert!(
            BINDINGS
                .iter()
                .any(|(k, d)| *k == "<esc>" && d.contains("Leave full screen")),
            "a binding with no advertised way out is a trap"
        );
    }

    #[test]
    fn every_binding_has_a_description() {
        for (key, desc) in BINDINGS {
            assert_eq!(
                key.is_empty(),
                desc.is_empty(),
                "binding {key:?} is half-populated"
            );
        }
    }
}
