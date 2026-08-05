//! Terminal UI: view state, key handling, and ratatui rendering.
//!
//! Holds which view is active (interleaved all-view or a single pane), scroll
//! position, follow-mode, and the timestamp display mode, and translates key
//! events into state changes. The ratatui render layer is intentionally thin;
//! all view/scroll/follow logic lives in plain, testable methods on
//! [`UiState`]. Rendering reads from a [`BufferSet`](crate::buffer::BufferSet)
//! and per-process [`Health`].
//!
//! This is the only module that knows about `jiff`: timestamps travel as plain
//! [`SystemTime`] and are converted to wall-clock fields here, at the
//! formatting boundary.

use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
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

/// How each line's arrival time is shown, cycled by `d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeDisplay {
    /// No timestamp prefix at all (the default; the UI looks as it always has).
    Off,
    /// Local-time ISO datetime, e.g. `2026-08-05T14:03:07`.
    Iso,
    /// Local time of day only, e.g. `14:03:07`.
    TimeOnly,
    /// Age relative to now in whole units, e.g. `12m ago`.
    Ago,
}

impl TimeDisplay {
    /// The next mode in the cycle: Off -> Iso -> TimeOnly -> Ago -> Off.
    fn next(self) -> TimeDisplay {
        match self {
            TimeDisplay::Off => TimeDisplay::Iso,
            TimeDisplay::Iso => TimeDisplay::TimeOnly,
            TimeDisplay::TimeOnly => TimeDisplay::Ago,
            TimeDisplay::Ago => TimeDisplay::Off,
        }
    }
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
    /// `d`: advance the timestamp display mode.
    CycleTimeDisplay,
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
        KeyCode::Char('d') => KeyCommand::CycleTimeDisplay,
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
                if cur + 1 >= count {
                    View::All
                } else {
                    View::Single(cur + 1)
                }
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
    /// How line arrival times are shown; cycled by `d`.
    time_display: TimeDisplay,
    /// The local timezone, resolved once at startup. Held rather than looked up
    /// per frame; it carries the full zone definition, so DST transitions during
    /// a long session are still handled correctly.
    tz: TimeZone,
}

impl UiState {
    /// Create initial state for the given per-process short names, starting in
    /// the all-view, following the tail, with timestamps off.
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
            time_display: TimeDisplay::Off,
            // Falls back to UTC if the system zone cannot be determined; never
            // fails.
            tz: TimeZone::system(),
        }
    }

    /// Current timestamp display mode.
    #[allow(dead_code)]
    pub fn time_display(&self) -> TimeDisplay {
        self.time_display
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
            // Purely presentational: deliberately leaves scroll and follow
            // state alone, so cycling never yanks the user back to the tail.
            KeyCommand::CycleTimeDisplay => self.time_display = self.time_display.next(),
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
    /// each already carrying its timestamp prefix (when enabled) and its
    /// per-process tag prefix (in the all-view). `now` is the frame's reference
    /// instant for relative times.
    fn content_lines(&self, buffers: &BufferSet, now: SystemTime) -> Vec<TuiLine<'static>> {
        let stamp =
            |sl: &StyledLine, with_tag| tagged_line(sl, with_tag, self.time_display, now, &self.tz);
        match self.view {
            View::All => buffers
                .interleaved()
                .into_iter()
                .map(|sl| stamp(sl, true))
                .collect(),
            View::Single(p) => {
                if p >= self.proc_count {
                    return Vec::new();
                }
                buffers
                    .buffer(p)
                    .iter()
                    .map(|sl| stamp(sl, false))
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

        // One clock reading per frame, so every relative time on screen is
        // measured against the same instant. Relative times refresh on the
        // existing 50 ms redraw tick; no extra timer is needed.
        let now = SystemTime::now();

        self.render_status_bar(frame, chunks[0]);
        self.render_body(frame, chunks[1], buffers, now);
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

    fn render_body(&mut self, frame: &mut Frame, area: Rect, buffers: &BufferSet, now: SystemTime) {
        let title = match self.view {
            View::All => " all ".to_string(),
            View::Single(p) => format!(" pane {} ", p + 1),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);

        let lines = self.content_lines(buffers, now);

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
        Health::ExitedOk => ("✔ exit 0".to_string(), Style::default().fg(Color::DarkGray)),
        Health::ExitedErr(status) => {
            let detail = match status {
                ExitStatus::Code(c) => format!("✖ exit {c}"),
                ExitStatus::Signal(s) => format!("✖ sig {s}"),
            };
            (detail, Style::default().fg(Color::Red))
        }
        Health::SpawnFailed => ("✖ spawn".to_string(), Style::default().fg(Color::Red)),
    }
}

// ---------------------------------------------------------------------------
// Timestamp formatting (pure, terminal- and host-timezone-independent)
// ---------------------------------------------------------------------------

/// Rendered when a timestamp lies outside the range jiff can represent, which
/// takes an absurdly skewed clock. Same width as a real stamp so columns stay
/// aligned, and formatting still never fails.
const ISO_UNKNOWN: &str = "????-??-??T??:??:??";
const TIME_UNKNOWN: &str = "??:??:??";

/// Convert an arrival time into local wall-clock fields, or `None` if it is not
/// representable.
fn local(at: SystemTime, tz: &TimeZone) -> Option<Zoned> {
    Timestamp::try_from(at)
        .ok()
        .map(|ts| ts.to_zoned(tz.clone()))
}

/// ISO datetime in `tz` to second precision, e.g. `2026-08-05T14:03:07`.
fn format_iso(at: SystemTime, tz: &TimeZone) -> String {
    match local(at, tz) {
        Some(z) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            z.year(),
            z.month(),
            z.day(),
            z.hour(),
            z.minute(),
            z.second()
        ),
        None => ISO_UNKNOWN.to_string(),
    }
}

/// Time of day in `tz`, e.g. `14:03:07`.
fn format_time_only(at: SystemTime, tz: &TimeZone) -> String {
    match local(at, tz) {
        Some(z) => format!("{:02}:{:02}:{:02}", z.hour(), z.minute(), z.second()),
        None => TIME_UNKNOWN.to_string(),
    }
}

/// Age of `at` as of `now`, in whole units of the largest fitting bucket:
/// `Ns ago` under a minute, `Nm ago` under an hour, `Nh ago` under a day,
/// `Nd ago` beyond. A line that appears newer than `now` (clock skew) reads as
/// `0s ago` rather than a negative age.
fn format_ago(at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(at).unwrap_or(Duration::ZERO).as_secs();
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 60 * 60 => format!("{}m ago", s / 60),
        s if s < 24 * 60 * 60 => format!("{}h ago", s / (60 * 60)),
        s => format!("{}d ago", s / (24 * 60 * 60)),
    }
}

/// The dim gray timestamp span prefixed to a line, or `None` when timestamps
/// are off. Includes the separating space, so the rendered prefix reads e.g.
/// `2026-08-05T14:03:07 `.
fn time_prefix(
    mode: TimeDisplay,
    at: SystemTime,
    now: SystemTime,
    tz: &TimeZone,
) -> Option<Span<'static>> {
    let stamp = match mode {
        TimeDisplay::Off => return None,
        TimeDisplay::Iso => format_iso(at, tz),
        TimeDisplay::TimeOnly => format_time_only(at, tz),
        TimeDisplay::Ago => format_ago(at, now),
    };
    Some(Span::styled(
        format!("{stamp} "),
        Style::default().fg(Color::DarkGray),
    ))
}

/// Build an owned rendered line from a stored [`StyledLine`]: an optional
/// timestamp prefix, then optionally a per-process colored tag (used in the
/// all-view). stderr lines get a dim red marker.
fn tagged_line(
    sl: &StyledLine,
    with_tag: bool,
    time_display: TimeDisplay,
    now: SystemTime,
    tz: &TimeZone,
) -> TuiLine<'static> {
    use crate::types::StreamTag;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(span) = time_prefix(time_display, sl.at, now, tz) {
        spans.push(span);
    }
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
            Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
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
    fn map_key_cycle_time_display() {
        assert_eq!(
            map_key(key(KeyCode::Char('d'))),
            KeyCommand::CycleTimeDisplay
        );
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

    // --- timestamp display ----------------------------------------------

    /// A fixed +02:00 zone, so assertions on the absolute formats do not depend
    /// on the host's timezone.
    fn fixed_tz() -> TimeZone {
        TimeZone::fixed(jiff::tz::Offset::constant(2))
    }

    /// A `SystemTime` `secs` after the Unix epoch.
    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn time_display_next_wraps() {
        assert_eq!(TimeDisplay::Off.next(), TimeDisplay::Iso);
        assert_eq!(TimeDisplay::Iso.next(), TimeDisplay::TimeOnly);
        assert_eq!(TimeDisplay::TimeOnly.next(), TimeDisplay::Ago);
        assert_eq!(TimeDisplay::Ago.next(), TimeDisplay::Off);
    }

    #[test]
    fn time_display_starts_off_and_cycles_on_d() {
        let mut s = ui(2);
        assert_eq!(s.time_display(), TimeDisplay::Off);
        for expected in [
            TimeDisplay::Iso,
            TimeDisplay::TimeOnly,
            TimeDisplay::Ago,
            TimeDisplay::Off,
        ] {
            s.handle_key(key(KeyCode::Char('d')));
            assert_eq!(s.time_display(), expected);
        }
    }

    #[test]
    fn cycling_time_display_keeps_scroll_and_view() {
        // The mode is purely presentational: a user reading scrollback must not
        // be snapped back to the tail for pressing `d`.
        let mut s = ui(3);
        s.handle_key(key(KeyCode::Char('2')));
        s.content_len = 100;
        s.viewport_height = 10;
        s.scroll_by(5);
        assert!(!s.following());
        s.handle_key(key(KeyCode::Char('d')));
        assert_eq!(s.time_display(), TimeDisplay::Iso);
        assert!(!s.following());
        assert_eq!(s.view(), View::Single(1));
    }

    #[test]
    fn iso_and_time_only_render_in_the_given_zone() {
        // 2026-08-05T12:03:07Z is 14:03:07 at +02:00.
        let t = at(1_785_931_387);
        assert_eq!(format_iso(t, &fixed_tz()), "2026-08-05T14:03:07");
        assert_eq!(format_time_only(t, &fixed_tz()), "14:03:07");
    }

    #[test]
    fn iso_pads_every_field() {
        // 2026-01-15T22:45:01Z is 2026-01-16T00:45:01 at +02:00: single-digit
        // month/day/hour must still be two digits, and the date must roll over.
        let t = at(1_768_517_101);
        assert_eq!(format_iso(t, &fixed_tz()), "2026-01-16T00:45:01");
        assert_eq!(format_time_only(t, &fixed_tz()), "00:45:01");
    }

    #[test]
    fn ago_uses_whole_unit_buckets() {
        let now = at(10_000_000);
        assert_eq!(format_ago(now, now), "0s ago");
        assert_eq!(format_ago(at(10_000_000 - 59), now), "59s ago");
        assert_eq!(format_ago(at(10_000_000 - 60), now), "1m ago");
        // Truncating, not rounding: 119 s is still one minute.
        assert_eq!(format_ago(at(10_000_000 - 119), now), "1m ago");
        assert_eq!(format_ago(at(10_000_000 - 3_599), now), "59m ago");
        assert_eq!(format_ago(at(10_000_000 - 3_600), now), "1h ago");
        assert_eq!(format_ago(at(10_000_000 - 86_399), now), "23h ago");
        assert_eq!(format_ago(at(10_000_000 - 86_400), now), "1d ago");
        assert_eq!(format_ago(at(10_000_000 - 9 * 86_400), now), "9d ago");
    }

    #[test]
    fn ago_clamps_a_line_from_the_future() {
        // Clock skew (or an NTP step) can leave a line stamped after `now`;
        // that must read as `0s ago`, never as a negative age or a panic.
        let now = at(10_000_000);
        assert_eq!(format_ago(at(10_000_060), now), "0s ago");
    }

    #[test]
    fn time_prefix_is_dim_and_separated() {
        let now = at(1_785_931_387);
        assert!(time_prefix(TimeDisplay::Off, now, now, &fixed_tz()).is_none());

        let span = time_prefix(TimeDisplay::Iso, now, now, &fixed_tz()).unwrap();
        assert_eq!(span.content.as_ref(), "2026-08-05T14:03:07 ");
        assert_eq!(span.style.fg, Some(Color::DarkGray));

        let span = time_prefix(TimeDisplay::TimeOnly, now, now, &fixed_tz()).unwrap();
        assert_eq!(span.content.as_ref(), "14:03:07 ");

        let span = time_prefix(TimeDisplay::Ago, now, now, &fixed_tz()).unwrap();
        assert_eq!(span.content.as_ref(), "0s ago ");
    }

    #[test]
    fn tagged_line_prepends_the_stamp_before_the_tag() {
        let now = at(1_785_931_387);
        let sl = StyledLine::parse(0, crate::types::StreamTag::Stdout, 0, now, b"hello");

        let plain = tagged_line(&sl, true, TimeDisplay::Off, now, &fixed_tz());
        let text: String = plain.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "1│ hello");

        let stamped = tagged_line(&sl, true, TimeDisplay::TimeOnly, now, &fixed_tz());
        let text: String = stamped.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "14:03:07 1│ hello");
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
