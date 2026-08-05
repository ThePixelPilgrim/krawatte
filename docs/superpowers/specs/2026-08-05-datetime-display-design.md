# Datetime display cycle (`d` hotkey) — design

## Goal

Pressing `d` cycles a per-line timestamp prefix through four states:

```
Off (default) → ISO datetime → time only → relative ("ago") → Off
```

The timestamp is the line's arrival time. Ordering of lines is untouched
(still governed by `Seq`); timestamps are purely presentational.

## Requirements

- `d` cycles the display mode globally (applies to the all-view and every
  single pane alike).
- Default is Off: the UI looks exactly as today until `d` is pressed.
- Representations, each rendered as a dim gray prefix span before the
  process tag:
  - ISO datetime, local time: `2026-08-05T14:03:07 `
  - Time only, local time: `14:03:07 `
  - Relative: whole-unit buckets by age — under 60 s: `Ns ago`; under
    60 min: `Nm ago`; under 24 h: `Nh ago`; otherwise `Nd ago` (integer
    division, no compound units). A clock skew that makes a line appear
    newer than "now" renders as `0s ago`. Relative times update live via the existing
    50 ms render tick — no new timer.
- Formatting must never panic.

## Architecture

**Timestamp capture (proc.rs).** `emit()` stamps each line with
`std::time::SystemTime::now()` at the moment it assigns `seq`, in the
reader thread. This records true arrival time, unaffected by the UI
thread's 50 ms batching.

**Transport and storage (types.rs, buffer.rs).** `Event::Line` gains
`at: SystemTime`; `StyledLine` stores it. `types.rs` remains std-only —
its "no dependencies" contract is preserved by carrying the raw
`SystemTime` and converting to jiff types only at the formatting
boundary.

**UI state and key handling (ui.rs).**

```rust
pub enum TimeDisplay { Off, Iso, TimeOnly, Ago }
impl TimeDisplay { fn next(self) -> Self { /* Off → Iso → TimeOnly → Ago → Off */ } }
```

- `UiState` gains `time_display: TimeDisplay`, initialized to `Off`.
- `map_key`: `KeyCode::Char('d')` → new `KeyCommand::CycleTimeDisplay`.
- `handle_key`: advances `time_display` via `next()`. Cycling does not
  reset scroll or follow state.

**Rendering (ui.rs).** `render` captures `now` once per frame.
`tagged_line` receives the mode and `now` and, when the mode is not
`Off`, prepends one `Span` styled dim gray
(`Style::default().fg(Color::DarkGray)`), before the per-process tag.
Formatting lives in pure functions taking `(at: SystemTime, now:
SystemTime)` (plus a `jiff::tz::TimeZone` for the absolute forms) and
returning `String`, so they are unit-testable without a terminal and
independent of the host timezone.

**Dependency.** `jiff` is added to `Cargo.toml`, used only inside
`ui.rs` for local-timezone ISO/time formatting. Chosen over `chrono`
(heavier, legacy API) and over hand-rolled epoch math (UTC-only, wrong
wall-clock display); jiff's timezone handling is safe under krawatte's
multithreading.

## Rejected alternatives

- **Capture at drain in the main loop**: every line drained in one batch
  shares a slightly-late timestamp; no simpler in exchange.
- **Pre-format at arrival**: relative times change as time passes and
  cannot be pre-formatted; rendering already rebuilds all spans each
  frame, so per-frame `format!` matches the existing performance
  profile.

## Testing

Following the existing test style in `ui.rs`/`buffer.rs`:

- `map_key(Char('d'))` → `KeyCommand::CycleTimeDisplay`.
- `TimeDisplay::next` wraps: Off → Iso → TimeOnly → Ago → Off.
- Format functions: ISO and time-only against a fixed-offset timezone;
  relative buckets (seconds/minutes/hours/days); `at > now` yields
  `0s ago`.
- Existing test helpers (`line(...)` constructors) gain the new field;
  no existing assertion changes meaning.
