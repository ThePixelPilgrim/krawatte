//! Terminal UI: view state, key handling, and ratatui rendering.
//!
//! Holds which view is active (interleaved all-view or a single pane), scroll
//! position, and follow-mode, and translates key events into state changes. The
//! ratatui render layer is intentionally thin; all view/scroll/follow logic
//! lives in plain, testable methods on [`UiState`]. Rendering reads from a
//! [`BufferSet`](crate::buffer::BufferSet) and per-process [`Health`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::buffer::{BufferSet, StyledLine};
use crate::types::{ExitStatus, Health, ProcId};

/// Which body view is currently shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Interleaved all-view merging every process buffer in arrival order.
    All,
    /// A single process's buffer in isolation.
    Single(ProcId),
}

/// Result of handling a key: whether the app should begin shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Continue running.
    Continue,
    /// User requested quit (`q` or Ctrl-C); begin shutdown.
    Quit,
}

/// Palette used to give each process a stable, distinct tag color. Indexed by
/// `proc % PALETTE.len()`.
const PALETTE: [Color; 6] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
];

/// Stable tag color for a process.
fn proc_color(proc: ProcId) -> Color {
    PALETTE[proc % PALETTE.len()]
}

/// A pure command decoded from a key event, independent of any state. Decoding
/// is separated from application so the mapping can be unit-tested without
/// constructing a [`UiState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    /// `q` or Ctrl-C: begin shutdown.
    Quit,
    /// `Tab`: next single pane.
    NextPane,
    /// `Shift-Tab`: previous single pane.
    PrevPane,
    /// `1`..=`9`: jump to that pane (1-based in the key, 0-based here).
    JumpPane(ProcId),
    /// `0` or `a`: interleaved all-view.
    AllView,
    /// Scroll one line up (toward older lines).
    LineUp,
    /// Scroll one line down (toward the tail).
    LineDown,
    /// Scroll one page up.
    PageUp,
    /// Scroll one page down.
    PageDown,
    /// No mapped action.
    None,
}

/// Decode a key event into a [`KeyCommand`]. Pure; no state required.
pub fn map_key(key: KeyEvent) -> KeyCommand {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => KeyCommand::Quit,
        KeyCode::Char('q') => KeyCommand::Quit,
        KeyCode::BackTab => KeyCommand::PrevPane,
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => KeyCommand::PrevPane,
        KeyCode::Tab => KeyCommand::NextPane,
        KeyCode::Char('a') => KeyCommand::AllView,
        KeyCode::Char('0') => KeyCommand::AllView,
        KeyCode::Char(c @ '1'..='9') => {
            // '1' -> pane 0
            KeyCommand::JumpPane((c as usize) - ('1' as usize))
        }
        KeyCode::Up => KeyCommand::LineUp,
        KeyCode::Down => KeyCommand::LineDown,
        KeyCode::PageUp => KeyCommand::PageUp,
        KeyCode::PageDown => KeyCommand::PageDown,
        _ => KeyCommand::None,
    }
}

/// Result of a scroll computation: the new offset (lines up from the bottom)
/// and whether the view is now following the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollResult {
    offset: usize,
    following: bool,
}

/// Apply a signed scroll delta (positive = toward older lines) to a bottom-anchored
/// offset, clamping to `[0, max_offset]`. Reaching offset 0 means following.
fn apply_scroll(offset: usize, delta_up: isize, max_offset: usize) -> ScrollResult {
    let raw = offset as isize + delta_up;
    let clamped = raw.clamp(0, max_offset as isize) as usize;
    ScrollResult {
        offset: clamped,
        following: clamped == 0,
    }
}

/// Cycle through the views: All -> pane 0 -> ... -> pane N-1 -> All (and the
/// reverse when `forward` is false), so the interleaved view is reachable by
/// cycling alone.
fn cycle_pane(view: View, count: usize, forward: bool) -> View {
    if count == 0 {
        return View::All;
    }
    match view {
        View::All => {
            if forward {
                View::Single(0)
            } else {
                View::Single(count - 1)
            }
        }
        View::Single(cur) => {
            if forward {
                if cur + 1 >= count { View::All } else { View::Single(cur + 1) }
            } else if cur == 0 {
                View::All
            } else {
                View::Single(cur - 1)
            }
        }
    }
}

/// All UI state that is independent of the terminal backend.
#[derive(Debug)]
pub struct UiState {
    proc_count: usize,
    /// Short display name per process, shown in the status bar.
    names: Vec<String>,
    view: View,
    health: Vec<Health>,
    /// Number of lines scrolled up from the bottom. `0` == following the tail.
    scroll_offset: usize,
    /// Body viewport height (content rows) cached from the last render, used so
    /// key-driven paging and offset clamping match what is on screen.
    viewport_height: usize,
    /// Number of content lines in the current view, cached from the last render.
    content_len: usize,
}

impl UiState {
    /// Create initial state for the given per-process short names, starting in
    /// the all-view, following the tail.
    pub fn new(names: Vec<String>) -> UiState {
        let proc_count = names.len();
        UiState {
            proc_count,
            names,
            view: View::All,
            health: vec![Health::Running; proc_count],
            scroll_offset: 0,
            viewport_height: 0,
            content_len: 0,
        }
    }

    /// Current view.
    #[allow(dead_code)]
    pub fn view(&self) -> View {
        self.view
    }

    /// True if the view is auto-following the tail (as opposed to scrolled up).
    pub fn following(&self) -> bool {
        self.scroll_offset == 0
    }

    /// Update the cached health of a process (driven by
    /// [`Event`](crate::types::Event) handling in the main loop).
    pub fn set_health(&mut self, proc: ProcId, health: Health) {
        if let Some(slot) = self.health.get_mut(proc) {
            *slot = health;
        }
    }

    /// Maximum scroll offset for a content of `content_len` lines within the
    /// current viewport.
    fn max_offset(&self) -> usize {
        self.content_len.saturating_sub(self.viewport_height)
    }

    /// Switch the active view, resetting scroll to follow the tail.
    fn switch_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.scroll_offset = 0;
        }
    }

    /// Apply a key event, mutating view/scroll/follow state and returning
    /// whether to continue or quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        match map_key(key) {
            KeyCommand::Quit => return Action::Quit,
            KeyCommand::NextPane => {
                let v = cycle_pane(self.view, self.proc_count, true);
                self.switch_view(v);
            }
            KeyCommand::PrevPane => {
                let v = cycle_pane(self.view, self.proc_count, false);
                self.switch_view(v);
            }
            KeyCommand::JumpPane(p) => {
                if p < self.proc_count {
                    self.switch_view(View::Single(p));
                }
            }
            KeyCommand::AllView => self.switch_view(View::All),
            KeyCommand::LineUp => self.scroll_by(1),
            KeyCommand::LineDown => self.scroll_by(-1),
            KeyCommand::PageUp => self.scroll_by(self.viewport_height.max(1) as isize),
            KeyCommand::PageDown => self.scroll_by(-(self.viewport_height.max(1) as isize)),
            KeyCommand::None => {}
        }
        Action::Continue
    }

    /// Scroll by `delta_up` lines (positive toward older lines), clamped.
    fn scroll_by(&mut self, delta_up: isize) {
        let res = apply_scroll(self.scroll_offset, delta_up, self.max_offset());
        self.scroll_offset = res.offset;
    }

    /// Collect the content lines for the current view as owned rendered lines,
    /// each already carrying its per-process tag prefix (in the all-view).
    fn content_lines(&self, buffers: &BufferSet) -> Vec<TuiLine<'static>> {
        match self.view {
            View::All => buffers
                .interleaved()
                .into_iter()
                .map(|sl| tagged_line(sl, true))
                .collect(),
            View::Single(p) => {
                if p >= self.proc_count {
                    return Vec::new();
                }
                buffers
                    .buffer(p)
                    .iter()
                    .map(|sl| tagged_line(sl, false))
                    .collect()
            }
        }
    }

    /// Render the full frame (status bar + body) for the current state.
    pub fn render(&mut self, frame: &mut Frame, buffers: &BufferSet) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);

        self.render_status_bar(frame, chunks[0]);
        self.render_body(frame, chunks[1], buffers);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for p in 0..self.proc_count {
            if p > 0 {
                spans.push(Span::raw("  "));
            }
            let idx_style = if matches!(self.view, View::Single(sel) if sel == p) {
                Style::default()
                    .fg(proc_color(p))
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(proc_color(p))
                    .add_modifier(Modifier::BOLD)
            };
            let health = self.health.get(p).copied().unwrap_or(Health::Running);
            let (glyph, gstyle) = health_glyph(health);
            spans.push(Span::styled(format!("[{}]", p + 1), idx_style));
            spans.push(Span::raw(" "));
            if let Some(name) = self.names.get(p) {
                spans.push(Span::styled(
                    name.clone(),
                    Style::default().fg(proc_color(p)),
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(glyph, gstyle));
        }
        let follow_marker = if self.following() {
            Span::styled(" FOLLOW", Style::default().fg(Color::Green))
        } else {
            Span::styled(" SCROLL", Style::default().fg(Color::Yellow))
        };
        spans.push(follow_marker);
        let bar = Paragraph::new(TuiLine::from(spans))
            .style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(bar, area);
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect, buffers: &BufferSet) {
        let title = match self.view {
            View::All => " all ".to_string(),
            View::Single(p) => format!(" pane {} ", p + 1),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);

        let lines = self.content_lines(buffers);

        // Cache viewport + content metrics so key handling clamps correctly.
        self.viewport_height = inner.height as usize;
        self.content_len = lines.len();

        // Re-clamp the offset in case the content shrank since the last key.
        let max_off = self.max_offset();
        if self.scroll_offset > max_off {
            self.scroll_offset = max_off;
        }

        // Bottom-anchored: top line index = content_len - viewport - offset.
        let top = self
            .content_len
            .saturating_sub(self.viewport_height)
            .saturating_sub(self.scroll_offset);

        // ratatui's scroll offset is a u16; clamp rather than cast so a very
        // large interleaved buffer (e.g. many chatty processes summing past
        // 65_535 lines) cannot wrap modulo 65_536 into a jumbled position.
        let top = top.min(u16::MAX as usize) as u16;
        let para = Paragraph::new(lines).block(block).scroll((top, 0));
        frame.render_widget(para, area);
    }
}

/// Glyph + style for a health value per the design spec:
/// `●` running (green), `✔ exit 0` (gray), `✖ exit N` (red).
fn health_glyph(health: Health) -> (String, Style) {
    match health {
        Health::Running => ("●".to_string(), Style::default().fg(Color::Green)),
        Health::ExitedOk => (
            "✔ exit 0".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Health::ExitedErr(status) => {
            let detail = match status {
                ExitStatus::Code(c) => format!("✖ exit {c}"),
                ExitStatus::Signal(s) => format!("✖ sig {s}"),
            };
            (detail, Style::default().fg(Color::Red))
        }
        Health::SpawnFailed => (
            "✖ spawn".to_string(),
            Style::default().fg(Color::Red),
        ),
    }
}

/// Build an owned rendered line from a stored [`StyledLine`], optionally
/// prefixing a per-process colored tag (used in the all-view). stderr lines get
/// a dim red marker.
fn tagged_line(sl: &StyledLine, with_tag: bool) -> TuiLine<'static> {
    use crate::types::StreamTag;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if with_tag {
        spans.push(Span::styled(
            format!("{}│", sl.proc + 1),
            Style::default()
                .fg(proc_color(sl.proc))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    if sl.stream == StreamTag::Stderr {
        spans.push(Span::styled(
            "!",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::DIM),
        ));
        spans.push(Span::raw(" "));
    }
    // Clone the parsed content spans into the new owned line.
    spans.extend(sl.content.spans.iter().cloned());
    TuiLine::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ui(n: usize) -> UiState {
        UiState::new((0..n).map(|i| format!("p{i}")).collect())
    }
    fn key_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn map_key_quit_variants() {
        assert_eq!(map_key(key(KeyCode::Char('q'))), KeyCommand::Quit);
        assert_eq!(
            map_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyCommand::Quit
        );
    }

    #[test]
    fn map_key_plain_c_is_not_quit() {
        assert_eq!(map_key(key(KeyCode::Char('c'))), KeyCommand::None);
    }

    #[test]
    fn map_key_pane_navigation() {
        assert_eq!(map_key(key(KeyCode::Tab)), KeyCommand::NextPane);
        assert_eq!(map_key(key(KeyCode::BackTab)), KeyCommand::PrevPane);
        assert_eq!(
            map_key(key_mod(KeyCode::Tab, KeyModifiers::SHIFT)),
            KeyCommand::PrevPane
        );
    }

    #[test]
    fn map_key_jump_and_all() {
        assert_eq!(map_key(key(KeyCode::Char('1'))), KeyCommand::JumpPane(0));
        assert_eq!(map_key(key(KeyCode::Char('9'))), KeyCommand::JumpPane(8));
        assert_eq!(map_key(key(KeyCode::Char('0'))), KeyCommand::AllView);
        assert_eq!(map_key(key(KeyCode::Char('a'))), KeyCommand::AllView);
    }

    #[test]
    fn map_key_scroll() {
        assert_eq!(map_key(key(KeyCode::Up)), KeyCommand::LineUp);
        assert_eq!(map_key(key(KeyCode::Down)), KeyCommand::LineDown);
        assert_eq!(map_key(key(KeyCode::PageUp)), KeyCommand::PageUp);
        assert_eq!(map_key(key(KeyCode::PageDown)), KeyCommand::PageDown);
    }

    #[test]
    fn apply_scroll_clamps_and_reports_follow() {
        // scroll up from bottom
        let r = apply_scroll(0, 5, 100);
        assert_eq!(r.offset, 5);
        assert!(!r.following);
        // clamp at max
        let r = apply_scroll(98, 10, 100);
        assert_eq!(r.offset, 100);
        assert!(!r.following);
        // scroll back to bottom resumes follow
        let r = apply_scroll(3, -10, 100);
        assert_eq!(r.offset, 0);
        assert!(r.following);
    }

    #[test]
    fn cycle_pane_forward_and_back() {
        assert_eq!(cycle_pane(View::All, 3, true), View::Single(0));
        assert_eq!(cycle_pane(View::All, 3, false), View::Single(2));
        assert_eq!(cycle_pane(View::Single(0), 3, true), View::Single(1));
        // The all-view is part of the cycle: last pane wraps forward to All,
        // first pane wraps backward to All.
        assert_eq!(cycle_pane(View::Single(2), 3, true), View::All);
        assert_eq!(cycle_pane(View::Single(0), 3, false), View::All);
        assert_eq!(cycle_pane(View::Single(2), 3, false), View::Single(1));
    }

    #[test]
    fn cycle_pane_zero_count() {
        assert_eq!(cycle_pane(View::All, 0, true), View::All);
    }

    #[test]
    fn initial_state_is_all_view_following() {
        let s = ui(3);
        assert_eq!(s.view(), View::All);
        assert!(s.following());
    }

    #[test]
    fn handle_key_quit() {
        let mut s = ui(2);
        assert_eq!(s.handle_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn handle_key_jump_out_of_range_ignored() {
        let mut s = ui(2);
        // pane 5 (key '5') does not exist -> stays in All
        s.handle_key(key(KeyCode::Char('5')));
        assert_eq!(s.view(), View::All);
        // pane 2 (key '2') exists
        s.handle_key(key(KeyCode::Char('2')));
        assert_eq!(s.view(), View::Single(1));
    }

    #[test]
    fn handle_key_tab_cycles() {
        let mut s = ui(3);
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.view(), View::Single(0));
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.view(), View::Single(1));
        s.handle_key(key(KeyCode::BackTab));
        assert_eq!(s.view(), View::Single(0));
    }

    #[test]
    fn switching_view_resets_follow() {
        let mut s = ui(3);
        // pretend we scrolled up
        s.content_len = 100;
        s.viewport_height = 10;
        s.scroll_by(5);
        assert!(!s.following());
        s.handle_key(key(KeyCode::Char('2')));
        assert!(s.following());
    }

    #[test]
    fn scroll_up_then_back_to_bottom_resumes_follow() {
        let mut s = ui(1);
        s.content_len = 50;
        s.viewport_height = 10;
        s.scroll_by(20);
        assert!(!s.following());
        s.scroll_by(-100);
        assert!(s.following());
    }

    #[test]
    fn set_health_out_of_range_is_noop() {
        let mut s = ui(1);
        s.set_health(99, Health::ExitedOk); // must not panic
        assert!(s.following());
    }

    #[test]
    fn health_glyph_variants() {
        assert!(health_glyph(Health::Running).0.contains('●'));
        assert_eq!(health_glyph(Health::ExitedOk).0, "✔ exit 0");
        assert_eq!(
            health_glyph(Health::ExitedErr(ExitStatus::Code(2))).0,
            "✖ exit 2"
        );
        assert_eq!(
            health_glyph(Health::ExitedErr(ExitStatus::Signal(9))).0,
            "✖ sig 9"
        );
        assert_eq!(health_glyph(Health::SpawnFailed).0, "✖ spawn");
    }

    // Keep KeyEventKind import used across crossterm versions where KeyEvent::new
    // sets kind = Press by default.
    #[allow(dead_code)]
    fn _kind_marker() -> KeyEventKind {
        KeyEventKind::Press
    }
}
