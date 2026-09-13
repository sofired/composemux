#![allow(clippy::missing_docs_in_private_items)] // 11 left to document
//! An output pane showing one service's logs.
//!
//! Ported from nx `packages/nx/src/native/tui/components/terminal_pane.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! The emulator screen is blitted cell by cell rather than rendered as text, so
//! the container's own colours survive and per-cell highlighting stays possible.

use crate::model::{LogStore, ServiceStatus};
use crate::tui::status_icons::status_char;
use crate::tui::theme::THEME;
use crate::tui::utils::status_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, StatefulWidget, Widget,
};

/// Border plus the 2/1 padding nx uses inside a pane.
const H_CHROME: u16 = 2 + 4;
const V_CHROME: u16 = 2 + 2;
/// The one row of grid the pane is given beyond what it draws.
///
/// nx has no use for it: its pane is a pty, so the row the cursor rests on
/// holds a prompt or the tail of a line that has not ended yet, and showing it
/// is the point. A log pane is fed container output, which ends every line in
/// `\n`, so the cursor comes to rest on an empty row after every write. Sized
/// to the rows the pane draws, that row is one of them, and the pane shows a
/// blank line above its bottom padding for ever -- 1 row of padding above the
/// content against 2 below, and a row per pane that can never hold a log line.
/// One row of slack is what lets `blit_screen` leave the cursor's row out of
/// the frame without leaving a hole in the pane.
const CURSOR_ROW: u16 = 1;
/// Below this the pane is too small to show anything useful.
const MIN_PANE: u16 = 5;

pub struct PaneRender<'a> {
    pub title: &'a str,
    pub status: ServiceStatus,
    pub focused: bool,
    pub store: Option<&'a LogStore>,
    pub uptime: Option<String>,
    pub throbber: usize,
    /// Shown when this pane is the next `tab` target.
    pub tab_hint: bool,
}

/// Rows of a pane of this size that `blit_screen` draws into.
fn drawn_rows(area: Rect) -> u16 {
    area.height.saturating_sub(V_CHROME).max(3)
}

/// Emulator grid size for a pane of this size.
///
/// One row taller than the pane draws, which is [`CURSOR_ROW`]. The columns
/// are the pane's own, since nothing is held back horizontally.
pub fn emulator_size(area: Rect) -> (u16, u16) {
    let rows = drawn_rows(area).saturating_add(CURSOR_ROW);
    let cols = area.width.saturating_sub(H_CHROME).max(20);
    (rows, cols)
}

pub fn render(pane: &PaneRender, area: Rect, buf: &mut Buffer) {
    if area.width < MIN_PANE || area.height < MIN_PANE {
        Widget::render(
            Paragraph::new("...").style(Style::default().fg(THEME.secondary_fg)),
            area,
            buf,
        );
        return;
    }

    // The border carries the service's status colour, dimmed when unfocused.
    let base = status_style(pane.status);
    let border_style = if pane.focused {
        base
    } else {
        base.add_modifier(Modifier::DIM)
    };

    let name_style = if pane.focused {
        Style::default()
            .fg(THEME.primary_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(THEME.secondary_fg)
    };

    let mut title = vec![
        Span::styled(
            format!(" {} ", status_char(pane.status, pane.throbber)),
            base.add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}  ", pane.title), name_style),
    ];
    if pane.tab_hint && area.width > 40 {
        title.push(Span::styled(
            "Press <tab> to focus output",
            Style::default().fg(THEME.secondary_fg),
        ));
    }

    let mut block = Block::default()
        .title(Line::from(title))
        .borders(Borders::ALL)
        .border_type(if pane.focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style)
        .padding(Padding::new(2, 2, 1, 1));

    if let Some(uptime) = &pane.uptime {
        if area.width > 20 {
            block = block.title_top(
                Line::from(Span::styled(
                    format!("  {uptime}  "),
                    Style::default().fg(THEME.secondary_fg),
                ))
                .right_aligned(),
            );
        }
    }

    let inner = block.inner(area);
    Widget::render(block, area, buf);

    // A service can have an attached-but-silent stream, so an empty buffer is
    // reported the same way as no buffer at all.
    let store = match pane.store {
        Some(store) if store.has_output() => store,
        _ => {
            Widget::render(
                Paragraph::new("Waiting for output...")
                    .style(Style::default().fg(THEME.secondary_fg)),
                inner,
                buf,
            );
            return;
        }
    };

    blit_screen(store, inner, buf);
    render_scrollbar(store, area, inner, buf, border_style);
}

/// Copies the emulator's visible cells into the frame buffer.
///
/// The grid is [`CURSOR_ROW`] taller than the pane draws, so one row of it is
/// always left out, and `first_drawn_row` picks which.
fn blit_screen(store: &LogStore, inner: Rect, buf: &mut Buffer) {
    let screen = store.screen();
    let (rows, cols) = screen.size();
    let first = first_drawn_row(rows, inner.height, store.tail_row_blank());
    for row in 0..inner.height.min(rows.saturating_sub(first)) {
        for col in 0..cols.min(inner.width) {
            let Some(src) = screen.cell(first + row, col) else {
                continue;
            };
            let Some(dst) = buf.cell_mut((inner.x + col, inner.y + row)) else {
                continue;
            };
            let contents = src.contents();
            if contents.is_empty() {
                dst.set_char(' ');
            } else {
                dst.set_symbol(contents);
            }
            dst.set_style(cell_style(src));
        }
    }
}

/// The emulator row the pane's top line shows.
///
/// The window is anchored to the *bottom* of the grid, one row short of it
/// while the last row is blank. That is the whole fix: the cursor's row falls
/// off the end of the window instead of occupying a line of the pane, and the
/// row that would have scrolled out of the top takes its place.
///
/// When a container writes without a trailing newline -- a `\r` progress bar
/// part way through a redraw -- the last row is its live line, and the window
/// moves down to keep it on screen at the cost of the oldest row. That is what
/// a terminal does when a line appears anyway, and it is what makes the
/// transition invisible: the newline that ends the line scrolls the grid by
/// one and the window moves back up by one, so the same rows stay put.
///
/// This is a *visible* row index, and the anchor is what makes that sound: at
/// scroll offset `k` the emulator's visible rows start `k` rows earlier, so a
/// window measured back from the grid's bottom is the same window moved back
/// by `k`, which is what scrolling up is meant to do. Anchoring to the top
/// would instead have moved the window twice per scroll step.
///
/// Saturating throughout for the panes small enough that the grid's own floor
/// binds: below about seven rows the grid is taller than the pane's inner
/// area by more than [`CURSOR_ROW`], and the surplus comes off the top.
fn first_drawn_row(rows: u16, height: u16, tail_blank: bool) -> u16 {
    rows.saturating_sub(height.saturating_add(u16::from(tail_blank)))
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(convert_color(cell.fgcolor()))
        .bg(convert_color(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// vt100 colours map straight onto ratatui's, keeping indexed colours indexed
/// so they follow the user's terminal palette.
fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_scrollbar(store: &LogStore, area: Rect, inner: Rect, buf: &mut Buffer, style: Style) {
    let offset = store.scroll_offset();
    // Row count comes from the emulator's own geometry; materialising the text
    // just to measure it would allocate a String per row on every frame.
    // Less `CURSOR_ROW`, because the grid is that much taller than the window
    // the track is measuring: what is left is the rows the pane draws plus the
    // scrollback behind them. Subtracting unconditionally rather than only
    // while the last row is blank keeps the track still -- a live progress bar
    // hides the grid's top row for as long as it is being redrawn, and a
    // scrollbar that blinked in and out with it would cost more than the row
    // of precision it bought.
    let total = (store.screen().size().0 as usize).saturating_sub(CURSOR_ROW as usize) + offset;
    let scrollable = total.saturating_sub(inner.height as usize);
    if scrollable == 0 {
        return;
    }
    // The offset counts up from the bottom, so invert it for the track.
    let mut state = ScrollbarState::default()
        .content_length(scrollable)
        .viewport_content_length(inner.height as usize)
        .position(scrollable.saturating_sub(offset));
    let bar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .style(style);
    StatefulWidget::render(bar, area, buf, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_emulator_is_the_pane_plus_the_cursor_row() {
        let (rows, cols) = emulator_size(Rect::new(0, 0, 100, 40));
        assert_eq!(cols, 100 - 6);
        assert_eq!(
            rows,
            40 - 4 + 1,
            "the grid is the drawn rows plus the row the cursor rests on"
        );
    }

    #[test]
    fn emulator_size_never_goes_below_the_minimum() {
        let (rows, cols) = emulator_size(Rect::new(0, 0, 6, 4));
        assert_eq!(rows, 3 + 1);
        assert_eq!(cols, 20);
    }

    #[test]
    fn a_blank_last_row_falls_off_the_bottom_of_the_window() {
        // The grid a 40-row pane gets: 36 drawn rows and the cursor's.
        assert_eq!(
            first_drawn_row(37, 36, true),
            0,
            "with nothing on the last row the window ends above it"
        );
        assert_eq!(
            first_drawn_row(37, 36, false),
            1,
            "a line still being written pulls the window down onto it"
        );
    }

    #[test]
    fn the_window_stays_anchored_to_the_bottom_of_a_grid_it_cannot_fill() {
        // Panes this small are floored at three rows by `drawn_rows` while
        // ratatui gives their inner area one, so the grid runs ahead of the
        // window by more than the cursor row and the surplus comes off the top.
        assert_eq!(first_drawn_row(4, 1, true), 2);
        assert_eq!(first_drawn_row(4, 1, false), 3);
    }

    #[test]
    fn a_window_taller_than_the_grid_starts_at_the_top() {
        assert_eq!(first_drawn_row(4, 40, true), 0);
        assert_eq!(first_drawn_row(4, 40, false), 0);
    }

    #[test]
    fn colours_pass_through_by_kind() {
        assert_eq!(convert_color(vt100::Color::Default), Color::Reset);
        assert_eq!(convert_color(vt100::Color::Idx(1)), Color::Indexed(1));
        assert_eq!(
            convert_color(vt100::Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );
    }
}
