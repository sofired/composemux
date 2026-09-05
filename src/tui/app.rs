//! Application state and key dispatch.
//!
//! Ported from nx `packages/nx/src/native/tui/app.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! The pinning state machine and the order in which keys are dispatched are
//! reproduced deliberately: they are what make the tool feel like nx.

use crate::config::Config;
use crate::model::{LogStore, Service, ServiceStatus};
use crate::tui::filter::Filter;
use crate::tui::focus::{Focus, FocusStack, MAX_PANES};
use crate::tui::layout_manager::{LayoutManager, ListVisibility, PaneArrangement};
use crate::tui::scroll_momentum::{ScrollDirection, ScrollMomentum};
use crate::tui::utils::sort_services;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Fixed scroll step for `ctrl+u` / `ctrl+d`, as in nx.
const HALF_PAGE_LINES: u16 = 12;
/// How long a transient status message stays in the bar.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(3);

/// Identifies one container: a service, plus its replica index when scaled.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceKey {
    pub name: String,
    pub replica: u32,
}

impl ServiceKey {
    pub fn new(name: impl Into<String>, replica: u32) -> Self {
        Self {
            name: name.into(),
            replica,
        }
    }
}

/// How the selection survives the list being re-sorted or re-filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Keep the same row index.
    TrackByPosition,
    /// Keep the same service, wherever it moves to.
    TrackByName,
}

/// Work that needs resources the app doesn't own (the clipboard, the frame size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CopyOutput,
    ToggleLayout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Quit,
    Interrupt,
    AutoExit,
    /// Terminated by a signal, carrying its number so the exit status can
    /// follow the `128 + signo` convention a supervisor expects.
    Signal(i32),
}

impl ExitReason {
    /// Exit status handed back to the wrapping CLI.
    pub fn code(self) -> i32 {
        match self {
            // ctrl+c never reaches us as a signal -- raw mode suppresses ISIG,
            // so it arrives as a key -- but a caller still expects the status a
            // real SIGINT would have produced.
            ExitReason::Interrupt => 130,
            ExitReason::Signal(signo) => 128 + signo,
            ExitReason::Quit | ExitReason::AutoExit => 0,
        }
    }
}

/// A pane holding the whole frame, and the layout to put back when it lets go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullScreen {
    /// The pane slot holding the frame, not its position among the rendered
    /// rects -- pinning to slot two alone leaves slot one empty.
    pane: usize,
    /// The split to put back on the way out, so leaving restores what the
    /// user had rather than a default.
    arrangement: PaneArrangement,
    /// Whether the list was on before, so a list hidden with `b` stays hidden.
    visibility: ListVisibility,
}

/// One row of the service list.
#[derive(Debug, Clone)]
pub struct Row {
    pub key: ServiceKey,
    pub display_name: String,
    pub service: Service,
}

pub struct App {
    pub project: String,
    /// Every known container, unfiltered.
    all: Vec<Row>,
    /// The filtered, sorted rows actually on screen.
    visible: Vec<Row>,
    stores: HashMap<ServiceKey, LogStore>,
    scrollback: usize,
    selected: usize,
    panes: [Option<ServiceKey>; MAX_PANES],
    spacebar_mode: bool,
    selection_mode: SelectionMode,
    focus: FocusStack,
    filter: Filter,
    layout: LayoutManager,
    /// Set while one pane owns the whole frame; also the restore point, so
    /// re-entering must not overwrite it.
    full_screen: Option<FullScreen>,
    momentum: ScrollMomentum,
    throbber: usize,
    exit: Option<ExitReason>,
    /// Cleared once the user presses anything, cancelling auto-exit.
    user_interacted: bool,
    auto_exit_after: Option<Duration>,
    all_finished_since: Option<Instant>,
    status_message: Option<(String, Instant)>,
    /// Advanced by `tick` and `handle_key`, so countdown state is testable
    /// without sleeping.
    clock: Instant,
    startup_pins: Vec<String>,
    pending: Option<Action>,
    pub mouse_capture: bool,
}

impl App {
    /// A fresh app for `project`, with nothing pinned and nothing full screen.
    pub fn new(project: impl Into<String>, config: &Config) -> Self {
        Self {
            project: project.into(),
            all: Vec::new(),
            visible: Vec::new(),
            stores: HashMap::new(),
            scrollback: config.scrollback,
            selected: 0,
            panes: [None, None],
            spacebar_mode: false,
            selection_mode: SelectionMode::TrackByPosition,
            focus: FocusStack::default(),
            filter: Filter::default(),
            layout: LayoutManager::default(),
            full_screen: None,
            momentum: ScrollMomentum::default(),
            throbber: 0,
            exit: None,
            user_interacted: false,
            auto_exit_after: config.auto_exit.seconds().map(Duration::from_secs),
            all_finished_since: None,
            status_message: None,
            clock: Instant::now(),
            startup_pins: config.pinned.clone(),
            pending: None,
            mouse_capture: true,
        }
    }

    // ---- accessors -------------------------------------------------------

    pub fn rows(&self) -> &[Row] {
        &self.visible
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.visible.get(self.selected)
    }

    pub fn selected_key(&self) -> Option<ServiceKey> {
        self.selected_row().map(|r| r.key.clone())
    }

    pub fn focus(&self) -> Focus {
        self.focus.current()
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    pub fn layout(&self) -> &LayoutManager {
        &self.layout
    }

    /// The pane currently holding the whole frame, if any.
    pub fn full_screen_pane(&self) -> Option<usize> {
        self.full_screen.map(|f| f.pane)
    }

    pub fn throbber(&self) -> usize {
        self.throbber
    }

    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.exit
    }

    /// Takes any action queued by the last keypress.
    pub fn take_action(&mut self) -> Option<Action> {
        self.pending.take()
    }

    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_ref().map(|(m, _)| m.as_str())
    }

    /// The service shown in a pane. In spacebar mode pane 0 follows the
    /// selection rather than holding a pinned entry.
    pub fn pane_key(&self, idx: usize) -> Option<ServiceKey> {
        if self.spacebar_mode {
            return if idx == 0 { self.selected_key() } else { None };
        }
        self.panes.get(idx).and_then(|k| k.clone())
    }

    /// Pane indices a service is pinned to, for the `[1]` / `[2]` indicators.
    /// Empty in spacebar mode, matching nx.
    pub fn pane_indicators(&self, key: &ServiceKey) -> Vec<usize> {
        if self.spacebar_mode {
            return Vec::new();
        }
        (0..MAX_PANES)
            .filter(|i| self.panes[*i].as_ref() == Some(key))
            .collect()
    }

    pub fn store(&self, key: &ServiceKey) -> Option<&LogStore> {
        self.stores.get(key)
    }

    fn pinned_count(&self) -> usize {
        self.panes.iter().filter(|p| p.is_some()).count()
    }

    pub fn has_visible_panes(&self) -> bool {
        self.layout.arrangement() != PaneArrangement::None
    }

    /// Pins the services named in config to panes 1 and 2, once the list is
    /// first populated. Names that don't match any service are ignored.
    pub fn apply_startup_pins(&mut self) {
        let wanted = std::mem::take(&mut self.startup_pins);
        for (idx, name) in wanted.iter().take(MAX_PANES).enumerate() {
            let Some(row) = self
                .all
                .iter()
                .find(|r| &r.display_name == name || &r.key.name == name)
            else {
                continue;
            };
            self.panes[idx] = Some(row.key.clone());
        }
        let count = self.pinned_count();
        if count > 0 {
            self.spacebar_mode = false;
            self.selection_mode = SelectionMode::TrackByName;
            self.layout
                .set_arrangement(PaneArrangement::for_count(count));
        }
    }

    // ---- data ------------------------------------------------------------

    /// Feeds container output into the right service's buffer.
    pub fn ingest(&mut self, key: ServiceKey, bytes: &[u8]) {
        let scrollback = self.scrollback;
        self.stores
            .entry(key)
            .or_insert_with(|| LogStore::new(scrollback))
            .process(bytes);
    }

    /// Replaces the known service list, preserving selection and pins.
    pub fn set_services(&mut self, mut services: Vec<Service>) {
        sort_services(&mut services);

        // A service with more than one container is shown per replica.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for s in &services {
            *counts.entry(s.name.as_str()).or_default() += 1;
        }

        self.all = services
            .iter()
            .map(|s| {
                let display_name = if counts.get(s.name.as_str()).copied().unwrap_or(1) > 1 {
                    format!("{}-{}", s.name, s.replica)
                } else {
                    s.name.clone()
                };
                Row {
                    key: ServiceKey::new(&s.name, s.replica),
                    display_name,
                    service: s.clone(),
                }
            })
            .collect();

        self.reapply_filter();
        self.prune_stores();
        self.note_finished_state();
    }

    /// Drops log buffers for containers that no longer exist, so a long session
    /// with service churn doesn't accumulate them. Pinned panes keep theirs.
    fn prune_stores(&mut self) {
        let live: std::collections::HashSet<&ServiceKey> =
            self.all.iter().map(|r| &r.key).collect();
        let pinned: Vec<ServiceKey> = self.panes.iter().flatten().cloned().collect();
        self.stores
            .retain(|key, _| live.contains(key) || pinned.contains(key));
    }

    /// Recomputes the visible rows, keeping the selection anchored per the
    /// current selection mode.
    fn reapply_filter(&mut self) {
        let previous = self.selected_key();
        self.visible = self
            .all
            .iter()
            .filter(|r| self.filter.matches(&r.display_name))
            .cloned()
            .collect();

        match self.selection_mode {
            SelectionMode::TrackByName => {
                if let Some(prev) = previous {
                    if let Some(idx) = self.visible.iter().position(|r| r.key == prev) {
                        self.selected = idx;
                        return;
                    }
                }
                self.clamp_selection();
            }
            SelectionMode::TrackByPosition => self.clamp_selection(),
        }
    }

    fn clamp_selection(&mut self) {
        if self.visible.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.visible.len() - 1);
        }
    }

    /// Number of services hidden by the filter, for the status bar.
    pub fn hidden_count(&self) -> usize {
        self.all.len().saturating_sub(self.visible.len())
    }

    /// True when every service exited, and all of them exited cleanly.
    ///
    /// A failed service suppresses auto-exit deliberately. Nx auto-exits because
    /// finishing is the expected end of a run; here it means the stack fell
    /// over, and closing would let the wrapping CLI tear the project down before
    /// anyone read the crash.
    fn stack_exited_cleanly(&self) -> bool {
        !self.all.is_empty()
            && self.all.iter().all(|r| r.service.status.is_finished())
            && !self
                .all
                .iter()
                .any(|r| r.service.status == ServiceStatus::Failure)
    }

    /// Starts (or cancels) the auto-exit countdown when the stack goes down.
    fn note_finished_state(&mut self) {
        let all_finished = self.stack_exited_cleanly();
        match (all_finished, self.all_finished_since) {
            (true, None) => self.all_finished_since = Some(self.clock),
            (false, Some(_)) => {
                self.all_finished_since = None;
                if self.focus.popup() == Some(Focus::CountdownPopup) {
                    self.focus.close_popup();
                }
            }
            _ => {}
        }
    }

    /// Seconds left before auto-exit, or `None` when no countdown is running.
    pub fn countdown_remaining(&self) -> Option<u64> {
        if self.user_interacted {
            return None;
        }
        let after = self.auto_exit_after?;
        let since = self.all_finished_since?;
        let elapsed = self.clock.saturating_duration_since(since);
        Some(after.saturating_sub(elapsed).as_secs_f64().ceil() as u64)
    }

    /// Advances animation and time-based state. Called once per tick.
    pub fn tick(&mut self, now: Instant) {
        self.clock = now;
        self.throbber = self.throbber.wrapping_add(1);

        if let Some((_, at)) = &self.status_message {
            if now.saturating_duration_since(*at) > STATUS_MESSAGE_TTL {
                self.status_message = None;
            }
        }

        if !self.user_interacted {
            if let (Some(after), Some(since)) = (self.auto_exit_after, self.all_finished_since) {
                if self.focus.popup() != Some(Focus::CountdownPopup) {
                    self.focus.push_popup(Focus::CountdownPopup);
                }
                if now.saturating_duration_since(since) >= after {
                    self.exit = Some(ExitReason::AutoExit);
                }
            }
        }
    }

    pub fn set_status_message(&mut self, message: impl Into<String>) {
        self.status_message = Some((message.into(), self.clock));
    }

    // ---- selection -------------------------------------------------------

    pub fn select_next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.visible.len();
    }

    pub fn select_previous(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.visible.len() - 1
        } else {
            self.selected - 1
        };
    }

    // ---- panes -----------------------------------------------------------

    fn set_spacebar_mode(&mut self, enabled: bool, mode: Option<SelectionMode>) {
        self.spacebar_mode = enabled;
        self.selection_mode = mode.unwrap_or(if enabled {
            SelectionMode::TrackByName
        } else {
            SelectionMode::TrackByPosition
        });
        self.layout.set_arrangement(if enabled {
            PaneArrangement::Single
        } else {
            PaneArrangement::None
        });
    }

    pub fn clear_all_panes(&mut self) {
        self.panes = [None, None];
        self.spacebar_mode = false;
        self.selection_mode = SelectionMode::TrackByPosition;
        self.layout.set_arrangement(PaneArrangement::None);
        self.focus.set_base(Focus::ServiceList);
    }

    /// `1` / `2`. Unpins if the selection already occupies that pane, moves it
    /// if it sits in the other one, otherwise pins it there.
    pub fn toggle_pane(&mut self, idx: usize) {
        if idx >= MAX_PANES {
            return;
        }
        let Some(selection) = self.selected_key() else {
            return;
        };

        if self.spacebar_mode {
            self.panes = [None, None];
            self.spacebar_mode = false;
            self.selection_mode = SelectionMode::TrackByName;
            self.panes[idx] = Some(selection);
            self.layout.set_arrangement(PaneArrangement::Single);
            return;
        }

        if self.panes[idx].as_ref() == Some(&selection) {
            self.panes[idx] = None;
            let count = self.pinned_count();
            self.layout
                .set_arrangement(PaneArrangement::for_count(count));
            if count == 0 {
                self.focus.set_base(Focus::ServiceList);
                self.selection_mode = SelectionMode::TrackByPosition;
            } else if self.focus.base() == Focus::Pane(idx) {
                // Don't leave focus on a pane that no longer has content.
                self.focus.set_base(Focus::ServiceList);
            }
            return;
        }

        self.move_or_pin(selection, idx);
    }

    fn move_or_pin(&mut self, selection: ServiceKey, idx: usize) {
        let other = (idx + 1) % MAX_PANES;
        if self.panes[other].as_ref() == Some(&selection) {
            // Move between panes; the total stays at one.
            self.panes[other] = None;
            self.panes[idx] = Some(selection);
            self.layout.set_arrangement(PaneArrangement::Single);
            if self.focus.base() == Focus::Pane(other) {
                self.focus.set_base(Focus::Pane(idx));
            }
        } else {
            // Fresh pin, silently replacing whatever was there.
            self.panes[idx] = Some(selection);
            self.focus.set_base(Focus::ServiceList);
            self.spacebar_mode = false;
            self.selection_mode = SelectionMode::TrackByName;
            self.layout
                .set_arrangement(PaneArrangement::for_count(self.pinned_count()));
        }
    }

    /// `space`. Opens a single pane that follows the selection, or closes
    /// whatever panes are open.
    pub fn toggle_output_visibility(&mut self) {
        self.layout.set_visibility(ListVisibility::Visible);
        let Some(selection) = self.selected_key() else {
            return;
        };
        if self.has_visible_panes() {
            self.clear_all_panes();
        } else {
            self.panes = [Some(selection), None];
            self.set_spacebar_mode(true, None);
        }
    }

    /// `enter`. Focuses the selection's pane, opening one if needed.
    pub fn open_and_focus_selection(&mut self) {
        let Some(selection) = self.selected_key() else {
            return;
        };
        if let Some(idx) = (0..MAX_PANES).find(|i| self.panes[*i].as_ref() == Some(&selection)) {
            self.focus.set_base(Focus::Pane(idx));
            return;
        }
        self.move_or_pin(selection, 0);
        if self.has_visible_panes() {
            self.focus.set_base(Focus::Pane(0));
        }
    }

    /// `enter` on a focused pane, which nx's status bar calls "full screen".
    ///
    /// Nx gets there by swapping to a whole second view that renders one task
    /// and nothing else. There is no second view here, so the layout does the
    /// same job: this pane alone, with neither the list nor its sibling.
    fn full_screen_focused_pane(&mut self) {
        if self.full_screen.is_some() {
            return;
        }
        let Some(idx) = self.focus.current().pane_index() else {
            return;
        };
        // An empty slot would fill the frame with a pane that never draws.
        if self.pane_key(idx).is_none() {
            return;
        }
        self.full_screen = Some(FullScreen {
            pane: idx,
            arrangement: self.layout.arrangement(),
            visibility: self.layout.visibility(),
        });
        self.layout.set_arrangement(PaneArrangement::Single);
        self.layout.set_visibility(ListVisibility::Hidden);
    }

    /// Puts back the arrangement and list visibility full screen replaced.
    /// Reports whether there was a full screen to leave, so `esc` can fall
    /// through to its other meaning when there wasn't.
    fn leave_full_screen(&mut self) -> bool {
        let Some(saved) = self.full_screen.take() else {
            return false;
        };
        self.layout.set_arrangement(saved.arrangement);
        self.layout.set_visibility(saved.visibility);
        true
    }

    /// Pane slots holding a service, in order.
    ///
    /// The layout allocates one rect per occupied slot, so a renderer has to map
    /// rect position to slot through this rather than assuming they match.
    /// Pinning only to pane 2 leaves slot 0 empty and produces a single rect.
    pub fn occupied_panes(&self) -> Vec<usize> {
        self.panes_with_content()
    }

    /// Looks a service up regardless of the filter.
    ///
    /// A pinned pane keeps streaming a service the filter has hidden, so its
    /// header must not be resolved through the visible rows.
    pub fn service_row(&self, key: &ServiceKey) -> Option<&Row> {
        self.all.iter().find(|r| &r.key == key)
    }

    /// The pane slots the layout should be asked to place.
    fn panes_with_content(&self) -> Vec<usize> {
        // Full screen hides the sibling pane as well as the list, so the layout
        // has to be asked for one rect rather than two.
        if let Some(full) = self.full_screen {
            return vec![full.pane];
        }
        if self.spacebar_mode {
            return vec![0];
        }
        (0..MAX_PANES)
            .filter(|i| self.panes[*i].is_some())
            .collect()
    }

    pub fn focus_next(&mut self) {
        if !self.has_visible_panes() {
            return;
        }
        let occupied = self.panes_with_content();
        let list_visible = self.layout.visibility() == ListVisibility::Visible;
        let next = match self.focus.base() {
            Focus::ServiceList => occupied.first().copied().map(Focus::Pane),
            Focus::Pane(i) => match occupied.iter().find(|p| **p > i) {
                Some(p) => Some(Focus::Pane(*p)),
                None if list_visible => Some(Focus::ServiceList),
                None => occupied.first().copied().map(Focus::Pane),
            },
            _ => None,
        };
        if let Some(focus) = next {
            self.focus.set_base(focus);
        }
    }

    pub fn focus_previous(&mut self) {
        if !self.has_visible_panes() {
            return;
        }
        let occupied = self.panes_with_content();
        let list_visible = self.layout.visibility() == ListVisibility::Visible;
        let prev = match self.focus.base() {
            Focus::ServiceList => occupied.last().copied().map(Focus::Pane),
            Focus::Pane(i) => match occupied.iter().rev().find(|p| **p < i) {
                Some(p) => Some(Focus::Pane(*p)),
                None if list_visible => Some(Focus::ServiceList),
                None => occupied.last().copied().map(Focus::Pane),
            },
            _ => None,
        };
        if let Some(focus) = prev {
            self.focus.set_base(focus);
        }
    }

    /// `b`. No-op with no panes open, as hiding the list would leave a blank screen.
    pub fn toggle_list_visibility(&mut self) {
        if !self.has_visible_panes() {
            return;
        }
        self.layout.toggle_visibility();
        if self.layout.visibility() == ListVisibility::Hidden
            && self.focus.base() == Focus::ServiceList
        {
            if let Some(first) = self.panes_with_content().first() {
                self.focus.set_base(Focus::Pane(*first));
            }
        }
    }

    // ---- scrolling -------------------------------------------------------

    fn scroll_focused_pane(&mut self, direction: ScrollDirection, now: Instant) {
        let Some(idx) = self.focus.current().pane_index() else {
            return;
        };
        let lines = self.momentum.scroll(direction, now);
        if lines == 0 {
            return;
        }
        let Some(key) = self.pane_key(idx) else {
            return;
        };
        if let Some(store) = self.stores.get_mut(&key) {
            match direction {
                ScrollDirection::Up => store.scroll_up(lines),
                ScrollDirection::Down => store.scroll_down(lines),
            }
        }
    }

    fn scroll_focused_pane_by(&mut self, direction: ScrollDirection, lines: u16) {
        let Some(idx) = self.focus.current().pane_index() else {
            return;
        };
        let Some(key) = self.pane_key(idx) else {
            return;
        };
        if let Some(store) = self.stores.get_mut(&key) {
            match direction {
                ScrollDirection::Up => store.scroll_up(lines),
                ScrollDirection::Down => store.scroll_down(lines),
            }
        }
    }

    // ---- key handling ----------------------------------------------------

    /// Dispatches a key press. The ordering mirrors nx: the interrupt always
    /// wins, then the filter swallows text, then popups, then focus-specific
    /// bindings.
    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) {
        self.clock = now;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // 1. Interrupt, unconditionally.
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            self.exit = Some(ExitReason::Interrupt);
            return;
        }

        // Any key cancels a pending auto-exit and dismisses its popup.
        let had_countdown = self.focus.popup() == Some(Focus::CountdownPopup);
        if had_countdown {
            self.user_interacted = true;
            self.focus.close_popup();
            // nx dismisses the countdown and then lets the key act normally,
            // except for keys whose only job was to dismiss it.
            if matches!(key.code, KeyCode::Esc) {
                return;
            }
        }
        self.user_interacted = true;

        // 2. While typing a filter, printable keys are text, not commands.
        if self.filter.is_editing() && self.focus.current() == Focus::ServiceList {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.reapply_filter();
                }
                KeyCode::Enter => self.filter.persist(),
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.reapply_filter();
                }
                KeyCode::Up => self.select_previous(),
                KeyCode::Down => self.select_next(),
                KeyCode::Char(c) if !ctrl => {
                    self.filter.push(c);
                    self.reapply_filter();
                }
                _ => {}
            }
            return;
        }

        // 3. Help popup swallows everything except its own navigation -- and
        // the quit it advertises. It lists "q or <ctrl>+c = Quit", so treating
        // q as a dismiss key contradicted the popup's own text, and diverges
        // from upstream, which reaches its global quit before its popup branch.
        if self.focus.current() == Focus::HelpPopup {
            match key.code {
                KeyCode::Char('q') => self.exit = Some(ExitReason::Quit),
                KeyCode::Esc | KeyCode::Char('?') => self.focus.close_popup(),
                _ => {}
            }
            return;
        }

        // 4. Global bindings.
        match key.code {
            KeyCode::Char('?') => {
                self.focus.push_popup(Focus::HelpPopup);
                return;
            }
            KeyCode::Char('q') => {
                self.exit = Some(ExitReason::Quit);
                return;
            }
            KeyCode::F(10) => {
                self.mouse_capture = !self.mouse_capture;
                self.set_status_message(if self.mouse_capture {
                    "Mouse capture on"
                } else {
                    "Mouse capture off - drag to select text with your terminal"
                });
                return;
            }
            _ => {}
        }

        // 5. A focused pane takes scrolling before anything else.
        if self.focus.current().pane_index().is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_focused_pane(ScrollDirection::Up, now);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_focused_pane(ScrollDirection::Down, now);
                    return;
                }
                KeyCode::Char('u') if ctrl => {
                    self.scroll_focused_pane_by(ScrollDirection::Up, HALF_PAGE_LINES);
                    return;
                }
                KeyCode::Char('d') if ctrl => {
                    self.scroll_focused_pane_by(ScrollDirection::Down, HALF_PAGE_LINES);
                    return;
                }
                KeyCode::Home => {
                    self.with_focused_store(|s| s.scroll_to_top());
                    return;
                }
                KeyCode::End => {
                    self.with_focused_store(|s| s.scroll_to_bottom());
                    return;
                }
                KeyCode::Char('c') if !ctrl => {
                    self.pending = Some(Action::CopyOutput);
                    return;
                }
                KeyCode::Enter => {
                    self.full_screen_focused_pane();
                    return;
                }
                KeyCode::Esc => {
                    // Leaving full screen spends the press: nx returns from its
                    // single-task view to the panes without also giving up the
                    // focus that view was showing.
                    if !self.leave_full_screen()
                        && self.layout.visibility() == ListVisibility::Visible
                    {
                        self.focus.set_base(Focus::ServiceList);
                    }
                    return;
                }
                // Full screen is modal upstream: nx's single-task view answers
                // 1, 2, 0, space, b, m and tab with "This key is not handled by
                // the TUI", so swallowing them keeps the keys that would put the
                // list or a second pane back from contradicting the state the
                // user asked for. Copy survives because nx keeps it too.
                //
                // Scrolling surviving is a deviation: nx's view has no scroll
                // bindings at all, because it renders inline and leaves history
                // to the host terminal's own scrollback. composemux owns the
                // alternate screen, so a pane's history exists nowhere but its
                // own buffer -- taking the frame to read more of it and then
                // being unable to move would defeat the point of the binding.
                _ if self.full_screen.is_some() => return,
                _ => {}
            }
        }

        // 6. Focus-specific bindings.
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(),
            KeyCode::Char('1') => self.toggle_pane(0),
            KeyCode::Char('2') => self.toggle_pane(1),
            KeyCode::Char('0') => self.clear_all_panes(),
            KeyCode::Char(' ') => self.toggle_output_visibility(),
            KeyCode::Enter => self.open_and_focus_selection(),
            KeyCode::Char('b') => self.toggle_list_visibility(),
            KeyCode::Char('m') => self.pending = Some(Action::ToggleLayout),
            KeyCode::Tab => self.focus_next(),
            KeyCode::BackTab => self.focus_previous(),
            // Only meaningful with the list focused: the filter's key capture
            // is gated on the same condition, so opening it from a pane left the
            // filter "editing" while every key still went to the pane.
            KeyCode::Char('/') if self.focus.current() == Focus::ServiceList => {
                self.filter.enter_edit()
            }
            KeyCode::Esc => {
                self.filter.clear();
                self.reapply_filter();
            }
            _ => {}
        }
    }

    /// The focused pane's full buffer, for the clipboard.
    pub fn focused_output(&mut self) -> Option<String> {
        let idx = self.focus.current().pane_index()?;
        let key = self.pane_key(idx)?;
        self.stores.get_mut(&key).map(|s| s.all_text())
    }

    /// Resizes each visible pane's emulator to match its rendered area.
    pub fn resize_panes(&mut self, sizes: &[(usize, u16, u16)]) {
        for (idx, rows, cols) in sizes {
            let Some(key) = self.pane_key(*idx) else {
                continue;
            };
            let scrollback = self.scrollback;
            self.stores
                .entry(key)
                .or_insert_with(|| LogStore::new(scrollback))
                .resize(*rows, *cols);
        }
    }

    fn with_focused_store(&mut self, f: impl FnOnce(&mut LogStore)) {
        let Some(idx) = self.focus.current().pane_index() else {
            return;
        };
        let Some(key) = self.pane_key(idx) else {
            return;
        };
        if let Some(store) = self.stores.get_mut(&key) {
            f(store);
        }
    }

    /// `m`. Needs the frame to know what auto currently resolves to.
    pub fn toggle_layout_mode(&mut self, area: ratatui::layout::Rect) {
        self.layout.toggle_mode(area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_AUTO_EXIT_SECONDS;
    use crate::model::Health;

    fn svc(name: &str, status: ServiceStatus) -> Service {
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

    fn app_with(names: &[&str]) -> App {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(
            names
                .iter()
                .map(|n| svc(n, ServiceStatus::Running))
                .collect(),
        );
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    }

    fn press_ctrl(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL), Instant::now());
    }

    // ---- selection ----

    #[test]
    fn navigation_wraps_in_both_directions() {
        let mut app = app_with(&["a", "b", "c"]);
        assert_eq!(app.selected_index(), 0);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_index(), 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected_index(), 0);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(
            app.selected_index(),
            2,
            "up from the top wraps to the bottom"
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn navigating_an_empty_list_is_safe() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected_index(), 0);
        assert!(app.selected_key().is_none());
    }

    // ---- pinning ----

    #[test]
    fn pinning_opens_a_pane_without_stealing_focus() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("a", 1)));
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        assert_eq!(
            app.focus(),
            Focus::ServiceList,
            "pinning should leave focus on the list"
        );
    }

    #[test]
    fn pinning_the_same_service_again_unpins_it() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.pane_key(0), None);
        assert_eq!(app.layout().arrangement(), PaneArrangement::None);
    }

    #[test]
    fn two_services_can_be_pinned_at_once() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("a", 1)));
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("b", 1)));
        assert_eq!(app.layout().arrangement(), PaneArrangement::Double);
    }

    #[test]
    fn pinning_a_service_already_in_the_other_pane_moves_it() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        // Same selection, now pinned to pane 2: it should move, not duplicate.
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.pane_key(0), None);
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("a", 1)));
        assert_eq!(
            app.layout().arrangement(),
            PaneArrangement::Single,
            "moving must not leave two panes open"
        );
    }

    #[test]
    fn a_fresh_pin_silently_replaces_the_pane_contents() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("b", 1)));
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
    }

    #[test]
    fn zero_clears_every_pane() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('0'));
        assert_eq!(app.pane_key(0), None);
        assert_eq!(app.pane_key(1), None);
        assert_eq!(app.layout().arrangement(), PaneArrangement::None);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn pinning_switches_selection_tracking_to_follow_the_service() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.selection_mode, SelectionMode::TrackByName);
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(
            app.selection_mode,
            SelectionMode::TrackByPosition,
            "unpinning the last pane returns to positional tracking"
        );
    }

    // ---- spacebar mode ----

    #[test]
    fn space_opens_a_pane_that_follows_the_selection() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("a", 1)));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.pane_key(0),
            Some(ServiceKey::new("b", 1)),
            "the pane should follow the selection in spacebar mode"
        );
    }

    #[test]
    fn spacebar_mode_shows_no_pin_indicators() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char(' '));
        assert!(app.pane_indicators(&ServiceKey::new("a", 1)).is_empty());
    }

    #[test]
    fn space_again_closes_the_pane() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.layout().arrangement(), PaneArrangement::None);
        assert!(!app.spacebar_mode);
    }

    #[test]
    fn pinning_from_spacebar_mode_converts_to_a_real_pin() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        assert!(!app.spacebar_mode);
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("b", 1)));
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        assert_eq!(app.pane_indicators(&ServiceKey::new("b", 1)), vec![1]);
    }

    // ---- focus ----

    #[test]
    fn tab_cycles_list_to_pane_and_back() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn tab_visits_both_panes_in_order() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(1));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn shift_tab_cycles_the_other_way() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.focus(), Focus::Pane(1));
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.focus(), Focus::Pane(0));
    }

    #[test]
    fn tab_does_nothing_with_no_panes_open() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn enter_opens_and_focuses_the_pane() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Pane(0));
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("a", 1)));
    }

    #[test]
    fn enter_on_an_already_pinned_service_just_focuses_it() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Pane(1));
        assert_eq!(app.pane_key(0), None, "no second pane should be opened");
    }

    #[test]
    fn escape_from_a_pane_returns_to_the_list() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn unpinning_the_focused_pane_moves_focus_back_to_the_list() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(1));
        // Unpin the pane that currently has focus.
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    // ---- list visibility ----

    #[test]
    fn b_does_nothing_without_panes() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
    }

    #[test]
    /// Focus cannot stay on a list that is no longer drawn, so `b` has to
    /// move it somewhere -- the precondition the full-screen tests rely on.
    fn b_hides_the_list_and_moves_focus_to_a_pane() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
    }

    // ---- full screen ----

    /// Two services pinned, focus on pane 1, which is where `tab` lands.
    fn app_with_two_panes() -> App {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Tab);
        app
    }

    #[test]
    /// The binding itself: nx's `full screen: <enter>`, which here means one
    /// pane, no list and no sibling.
    fn enter_on_a_focused_pane_gives_it_the_whole_frame() {
        let mut app = app_with_two_panes();
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        assert_eq!(
            app.occupied_panes(),
            vec![0],
            "the other pane should go too, not just the list"
        );
        assert_eq!(app.focus(), Focus::Pane(0));
    }

    #[test]
    /// Leaving restores the split that was there, not a default one -- a
    /// two-pane layout has to come back as two panes.
    fn escape_puts_back_the_arrangement_full_screen_replaced() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Double);
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
        assert_eq!(app.occupied_panes(), vec![0, 1]);
        assert_eq!(
            app.focus(),
            Focus::Pane(0),
            "leaving full screen is not leaving the pane"
        );
    }

    #[test]
    /// `esc` keeps its old meaning once full screen is gone: the first press
    /// leaves the view, the second returns focus to the list.
    fn a_second_escape_hands_focus_back_to_the_list() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.focus(),
            Focus::Pane(0),
            "the first esc spends itself on the full screen"
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    /// Restoring means putting back what the user had, not what the default
    /// is: a list hidden with `b` stays hidden on the way out.
    fn full_screen_restores_a_list_that_was_already_hidden() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Double);
        assert_eq!(
            app.layout().visibility(),
            ListVisibility::Hidden,
            "esc should restore what b left, not force the list back on"
        );
    }

    #[test]
    /// Slot two with slot one empty is where pane geometry has gone wrong
    /// before (#14): the full-screen rect belongs to the pane's slot, not to
    /// its position among the rects.
    fn a_pane_pinned_only_to_slot_two_goes_full_screen_as_itself() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(1));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        assert_eq!(
            app.occupied_panes(),
            vec![1],
            "the frame belongs to the focused slot, not to slot 0"
        );
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("b", 1)));
    }

    #[test]
    /// The view is modal: anything that would put a second thing on the frame
    /// has to be swallowed while it is up.
    fn full_screen_ignores_the_keys_that_would_split_the_frame_again() {
        // One press per app, so a failure names the key that got through.
        // tab and shift+tab are here for the guarantee rather than the guard:
        // with one occupied pane they would be no-ops even unswallowed.
        for code in [
            KeyCode::Char('1'),
            KeyCode::Char('2'),
            KeyCode::Char('0'),
            KeyCode::Char(' '),
            KeyCode::Char('b'),
            KeyCode::Char('m'),
            KeyCode::Tab,
            KeyCode::BackTab,
        ] {
            let mut app = app_with_two_panes();
            press(&mut app, KeyCode::Enter);
            press(&mut app, code);
            assert_eq!(
                app.layout().arrangement(),
                PaneArrangement::Single,
                "{code:?} changed the arrangement"
            );
            assert_eq!(
                app.layout().visibility(),
                ListVisibility::Hidden,
                "{code:?} put the service list back"
            );
            assert_eq!(app.occupied_panes(), vec![0], "{code:?} put a pane back");
            assert_eq!(app.focus(), Focus::Pane(0), "{code:?} moved focus");
            assert_eq!(
                app.pane_key(1),
                Some(ServiceKey::new("b", 1)),
                "{code:?} disturbed the pin full screen is hiding"
            );
            assert!(
                app.take_action().is_none(),
                "{code:?} queued a deferred action"
            );
        }
    }

    #[test]
    /// nx's single-task view is left with `esc`; `enter` is only the way in.
    /// #9 asked for a toggle, and this is the deliberate divergence from it.
    fn enter_does_not_leave_full_screen() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Single);
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        assert_eq!(app.occupied_panes(), vec![0]);
    }

    #[test]
    /// Re-entering would save the full-screen layout as the thing to go back
    /// to, stranding the user in it -- `esc` would restore what it already is.
    fn a_second_enter_does_not_move_the_restore_point() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.layout().arrangement(), PaneArrangement::Double);
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
    }

    #[test]
    /// A spacebar pane follows the selection rather than a pin, so a stack
    /// that empties out leaves the focused pane with nothing behind it, and
    /// full-screening nothing would be a blank modal frame.
    fn enter_is_inert_when_the_focused_pane_has_lost_its_service() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(0));
        app.set_services(Vec::new());
        assert_eq!(app.pane_key(0), None);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.full_screen_pane(),
            None,
            "a pane that cannot draw should not be handed the frame"
        );
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
    }

    #[test]
    /// The countdown dismissal runs before the pane bindings, and an unhandled
    /// key is meant to act as well as dismiss -- as it already does for `j`.
    fn an_enter_that_dismisses_the_countdown_still_goes_full_screen() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        // Not via a keypress: any key marks the user present and cancels the
        // countdown before it can start.
        app.open_and_focus_selection();
        assert_eq!(app.focus(), Focus::Pane(0));
        app.tick(Instant::now());
        assert_eq!(app.focus(), Focus::CountdownPopup);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Pane(0), "popup dismissed");
        assert_eq!(app.full_screen_pane(), Some(0));
    }

    #[test]
    /// The modality is about layout, not about disabling the pane: reading is
    /// the reason to be here at all.
    fn full_screen_still_scrolls_copies_and_quits() {
        let mut app = app_with(&["a"]);
        let key = ServiceKey::new("a", 1);
        for i in 0..100 {
            app.ingest(key.clone(), format!("line {i}\r\n").as_bytes());
        }
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        app.resize_panes(&[(0, 10, 40)]);
        press_ctrl(&mut app, KeyCode::Char('u'));
        assert_eq!(app.store(&key).unwrap().scroll_offset(), 12);
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.take_action(), Some(Action::CopyOutput));
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Quit));
    }

    #[test]
    /// Help is drawn over the view rather than replacing it, so dismissing it
    /// returns to full screen instead of collapsing the layout.
    fn help_opens_over_full_screen_and_leaves_it_intact() {
        let mut app = app_with_two_panes();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.focus(), Focus::HelpPopup);
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.focus(),
            Focus::Pane(0),
            "esc closed the popup, not the full screen under it"
        );
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        assert_eq!(app.occupied_panes(), vec![0]);
    }

    #[test]
    /// Spacebar mode has no pin to restore from, so the round trip has to work
    /// off the follow-the-selection pane as well as a pinned one.
    fn a_spacebar_pane_can_go_full_screen_and_come_back() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), Focus::Pane(0));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.layout().visibility(), ListVisibility::Hidden);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.layout().visibility(), ListVisibility::Visible);
        assert_eq!(
            app.pane_key(0),
            app.selected_key(),
            "the pane should still be following the selection"
        );
    }

    // ---- filter ----

    #[test]
    fn slash_opens_the_filter_and_typing_narrows_the_list() {
        let mut app = app_with(&["api", "worker", "db"]);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.rows().len(), 1);
        assert_eq!(app.rows()[0].display_name, "api");
        assert_eq!(app.hidden_count(), 2);
    }

    #[test]
    fn while_filtering_command_keys_are_literal_text() {
        let mut app = app_with(&["q1", "b2"]);
        press(&mut app, KeyCode::Char('/'));
        for c in ['q', '1'] {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.filter().query(), "q1");
        assert!(
            app.exit_reason().is_none(),
            "q must not quit while filtering"
        );
        assert_eq!(app.pane_key(0), None, "1 must not pin while filtering");
    }

    #[test]
    fn a_slash_typed_into_the_filter_is_literal() {
        let mut app = app_with(&["api/v2"]);
        press(&mut app, KeyCode::Char('/'));
        for c in "api/".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.filter().query(), "api/");
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn enter_persists_the_filter_and_hands_keys_back() {
        let mut app = app_with(&["api", "worker"]);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.filter().state(),
            crate::tui::filter::FilterState::Persisted
        );
        assert_eq!(app.rows().len(), 1, "the filter stays applied");
        // q now quits instead of typing.
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Quit));
    }

    #[test]
    fn slash_does_not_open_the_filter_from_a_focused_pane() {
        // Regression: the filter only captures keys while the list has focus,
        // so opening it from a pane stranded the app in a state where the
        // filter was "editing" but every key went to the pane instead.
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('/'));
        assert!(!app.filter().is_editing());
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(
            app.take_action(),
            Some(Action::CopyOutput),
            "pane bindings should still work after /"
        );
    }

    #[test]
    fn escape_clears_the_filter() {
        let mut app = app_with(&["api", "worker"]);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.rows().len(), 2);
        assert_eq!(app.filter().query(), "");
    }

    #[test]
    fn arrows_still_navigate_while_filtering() {
        let mut app = app_with(&["api", "apex"]);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.rows().len(), 2);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected_index(), 1);
    }

    #[test]
    fn filtering_keeps_the_selection_on_the_same_service_when_pinned() {
        let mut app = app_with(&["api", "worker"]);
        press(&mut app, KeyCode::Char('j')); // select worker
        press(&mut app, KeyCode::Char('1')); // pin -> TrackByName
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.selected_row().unwrap().display_name, "worker");
    }

    #[test]
    fn pins_survive_the_filter_hiding_the_service() {
        let mut app = app_with(&["api", "worker"]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(
            app.pane_key(0),
            Some(ServiceKey::new("api", 1)),
            "a pinned pane keeps its service even when filtered out of the list"
        );
    }

    // ---- quitting ----

    #[test]
    fn q_quits_with_status_zero() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Quit));
        assert_eq!(app.exit_reason().unwrap().code(), 0);
    }

    #[test]
    fn a_signal_exit_follows_the_128_plus_signo_convention() {
        assert_eq!(ExitReason::Signal(2).code(), 130); // SIGINT
        assert_eq!(ExitReason::Signal(15).code(), 143); // SIGTERM
        assert_eq!(ExitReason::Signal(1).code(), 129); // SIGHUP
    }

    #[test]
    fn ctrl_c_quits_with_status_one_hundred_thirty() {
        let mut app = app_with(&["a"]);
        press_ctrl(&mut app, KeyCode::Char('c'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Interrupt));
        assert_eq!(app.exit_reason().unwrap().code(), 130);
    }

    #[test]
    fn ctrl_c_interrupts_even_while_filtering() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('/'));
        press_ctrl(&mut app, KeyCode::Char('c'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Interrupt));
    }

    // ---- help ----

    #[test]
    fn question_mark_toggles_the_help_popup() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.focus(), Focus::HelpPopup);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn q_quits_from_the_help_popup_as_the_popup_says_it_does() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.focus(), Focus::HelpPopup);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.exit_reason(), Some(ExitReason::Quit));
    }

    #[test]
    fn escape_still_only_closes_the_help_popup() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::ServiceList);
        assert!(
            app.exit_reason().is_none(),
            "esc dismisses, it does not quit"
        );
    }

    #[test]
    fn the_help_popup_swallows_other_keys() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.selected_index(),
            0,
            "navigation should not leak through"
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::ServiceList);
    }

    #[test]
    fn help_restores_the_pane_that_had_focus() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::Pane(0));
    }

    // ---- scrolling ----

    #[test]
    fn scrolling_a_focused_pane_moves_its_buffer() {
        let mut app = app_with(&["a"]);
        let key = ServiceKey::new("a", 1);
        for i in 0..100 {
            app.ingest(key.clone(), format!("line {i}\r\n").as_bytes());
        }
        press(&mut app, KeyCode::Enter);
        app.resize_panes(&[(0, 10, 40)]);
        assert_eq!(app.store(&key).unwrap().scroll_offset(), 0);
        press_ctrl(&mut app, KeyCode::Char('u'));
        assert_eq!(app.store(&key).unwrap().scroll_offset(), 12);
        press_ctrl(&mut app, KeyCode::Char('d'));
        assert_eq!(app.store(&key).unwrap().scroll_offset(), 0);
    }

    #[test]
    fn home_and_end_jump_to_the_extremes() {
        let mut app = app_with(&["a"]);
        let key = ServiceKey::new("a", 1);
        for i in 0..100 {
            app.ingest(key.clone(), format!("line {i}\r\n").as_bytes());
        }
        press(&mut app, KeyCode::Enter);
        app.resize_panes(&[(0, 10, 40)]);
        press(&mut app, KeyCode::Home);
        assert!(app.store(&key).unwrap().scroll_offset() > 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.store(&key).unwrap().scroll_offset(), 0);
    }

    #[test]
    fn navigation_keys_scroll_the_pane_rather_than_the_list_when_it_has_focus() {
        let mut app = app_with(&["a", "b"]);
        press(&mut app, KeyCode::Enter);
        let before = app.selected_index();
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.selected_index(),
            before,
            "j should scroll the pane, not move the selection"
        );
    }

    // ---- auto exit ----

    #[test]
    fn the_countdown_starts_once_every_service_has_exited_cleanly() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Running)]);
        assert!(app.countdown_remaining().is_none());

        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        assert!(app.countdown_remaining().is_some());
    }

    #[test]
    fn a_failed_service_suppresses_auto_exit() {
        // Closing here would let the wrapping CLI tear the project down before
        // anyone read the crash, so a failure keeps the UI open.
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![
            svc("a", ServiceStatus::Success),
            svc("b", ServiceStatus::Failure),
        ]);
        assert!(app.countdown_remaining().is_none());

        let t0 = Instant::now();
        app.tick(t0 + Duration::from_secs(60));
        assert_eq!(
            app.exit_reason(),
            None,
            "a crashed stack must not auto-exit"
        );
    }

    #[test]
    fn auto_exit_fires_once_the_countdown_elapses() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        let t0 = Instant::now();
        app.tick(t0);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);

        app.tick(t0 + Duration::from_millis(500));
        assert_eq!(app.exit_reason(), None, "still counting down");
        assert_eq!(app.focus(), Focus::CountdownPopup, "the popup should be up");

        app.tick(t0 + Duration::from_secs(DEFAULT_AUTO_EXIT_SECONDS));
        assert_eq!(app.exit_reason(), Some(ExitReason::AutoExit));
        assert_eq!(app.exit_reason().unwrap().code(), 0);
    }

    #[test]
    fn the_countdown_counts_down() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        let t0 = Instant::now();
        app.tick(t0);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        assert_eq!(app.countdown_remaining(), Some(DEFAULT_AUTO_EXIT_SECONDS));
        app.tick(t0 + Duration::from_millis(1500));
        assert_eq!(app.countdown_remaining(), Some(2));
    }

    #[test]
    fn a_keypress_cancels_the_countdown() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        assert!(app.countdown_remaining().is_some());
        press(&mut app, KeyCode::Char('j'));
        assert!(
            app.countdown_remaining().is_none(),
            "interacting should cancel auto-exit"
        );
    }

    #[test]
    fn a_key_dismisses_the_countdown_popup_and_still_acts() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![
            svc("a", ServiceStatus::Success),
            svc("b", ServiceStatus::Success),
        ]);
        let t0 = Instant::now();
        app.tick(t0);
        assert_eq!(app.focus(), Focus::CountdownPopup);

        press(&mut app, KeyCode::Char('j'));
        assert_ne!(app.focus(), Focus::CountdownPopup, "popup dismissed");
        assert_eq!(
            app.selected_index(),
            1,
            "and the key still moved the selection"
        );
    }

    #[test]
    fn escape_dismisses_the_countdown_without_acting() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        app.tick(Instant::now());
        assert_eq!(app.focus(), Focus::CountdownPopup);

        press(&mut app, KeyCode::Esc);
        assert_ne!(app.focus(), Focus::CountdownPopup, "the popup closes");
        assert!(app.exit_reason().is_none(), "and Esc does not quit");
    }

    #[test]
    fn a_service_coming_back_up_cancels_the_countdown() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        assert!(app.countdown_remaining().is_some());
        app.set_services(vec![svc("a", ServiceStatus::Running)]);
        assert!(app.countdown_remaining().is_none());
    }

    #[test]
    fn auto_exit_can_be_disabled() {
        let cfg = Config {
            auto_exit: crate::config::AutoExit::Enabled(false),
            ..Config::default()
        };
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Success)]);
        assert!(app.countdown_remaining().is_none());
        app.tick(Instant::now() + Duration::from_secs(60));
        assert!(app.exit_reason().is_none());
    }

    #[test]
    fn an_empty_stack_does_not_trigger_auto_exit() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        app.set_services(vec![]);
        assert!(app.countdown_remaining().is_none());
    }

    // ---- ingest ----

    #[test]
    fn output_is_routed_to_the_matching_service() {
        let mut app = app_with(&["a", "b"]);
        app.ingest(ServiceKey::new("a", 1), b"hello from a\r\n");
        let store = app.store(&ServiceKey::new("a", 1)).unwrap();
        assert!(store
            .visible_lines()
            .iter()
            .any(|l| l.contains("hello from a")));
        assert!(app.store(&ServiceKey::new("b", 1)).is_none());
    }

    #[test]
    fn configured_pins_are_applied_once_services_arrive() {
        let cfg = Config {
            pinned: vec!["b".to_string(), "a".to_string()],
            ..Config::default()
        };
        let mut app = App::new("test", &cfg);
        app.set_services(vec![
            svc("a", ServiceStatus::Running),
            svc("b", ServiceStatus::Running),
        ]);
        app.apply_startup_pins();
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("b", 1)));
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("a", 1)));
        assert_eq!(app.layout().arrangement(), PaneArrangement::Double);
    }

    #[test]
    fn unknown_pinned_names_are_ignored() {
        let cfg = Config {
            pinned: vec!["nope".to_string()],
            ..Config::default()
        };
        let mut app = App::new("test", &cfg);
        app.set_services(vec![svc("a", ServiceStatus::Running)]);
        app.apply_startup_pins();
        assert_eq!(app.pane_key(0), None);
        assert_eq!(app.layout().arrangement(), PaneArrangement::None);
    }

    #[test]
    fn only_the_first_two_configured_pins_are_used() {
        let cfg = Config {
            pinned: vec!["a".into(), "b".into(), "c".into()],
            ..Config::default()
        };
        let mut app = App::new("test", &cfg);
        app.set_services(vec![
            svc("a", ServiceStatus::Running),
            svc("b", ServiceStatus::Running),
            svc("c", ServiceStatus::Running),
        ]);
        app.apply_startup_pins();
        assert_eq!(app.pane_key(0), Some(ServiceKey::new("a", 1)));
        assert_eq!(app.pane_key(1), Some(ServiceKey::new("b", 1)));
    }

    #[test]
    fn c_queues_a_copy_only_when_a_pane_has_focus() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.take_action(), None, "no pane focused, nothing to copy");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.take_action(), Some(Action::CopyOutput));
    }

    #[test]
    fn m_queues_a_layout_toggle() {
        let mut app = app_with(&["a"]);
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.take_action(), Some(Action::ToggleLayout));
        assert_eq!(app.take_action(), None, "the action is consumed once");
    }

    #[test]
    fn focused_output_returns_the_panes_buffer() {
        let mut app = app_with(&["a"]);
        app.ingest(ServiceKey::new("a", 1), b"hello\r\n");
        press(&mut app, KeyCode::Enter);
        let text = app.focused_output().expect("a pane is focused");
        assert!(text.contains("hello"), "got {text:?}");
    }

    #[test]
    fn replicas_are_listed_separately_with_suffixed_names() {
        let cfg = Config::default();
        let mut app = App::new("test", &cfg);
        let mut a1 = svc("api", ServiceStatus::Running);
        let mut a2 = svc("api", ServiceStatus::Running);
        a1.replica = 1;
        a2.replica = 2;
        app.set_services(vec![a1, a2, svc("db", ServiceStatus::Running)]);
        let names: Vec<_> = app.rows().iter().map(|r| r.display_name.as_str()).collect();
        assert!(names.contains(&"api-1"));
        assert!(names.contains(&"api-2"));
        assert!(names.contains(&"db"), "unscaled services keep a plain name");
    }
}
