# Line wrap toggle (`w` hotkey)

## Problem

Lines longer than the body viewport are clipped at the right edge. The tail of
a long line — a stack frame, a URL, a JSON payload — is simply unreachable:
there is no horizontal scroll, and no way to see it at all.

## Goal

A runtime toggle that wraps long lines onto continuation rows. Off by default,
so the UI behaves exactly as it does today unless asked.

## Behavior

- `w` toggles wrapping. It starts off; there is no CLI flag.
- When on, a line too wide for the viewport continues on the next row.
- Continuation rows are indented to the width of the line's prefix (timestamp,
  process tag, stderr marker), so wrapped text forms a block under the content
  column and the prefix column stays unambiguous:

  ```
  14:03:07 1│ this is a very long log line that
              continues here and here
  14:03:08 2│ short line
  ```

- Breaks are **hard**, at the last cell that fits — not at word boundaries.
  Log output is mostly machine output (paths, JSON, base64, stack traces);
  reflowing it at spaces produces ragged gaps and hides where the real breaks
  are. Nothing is ever dropped or re-ordered.
- The status bar shows a dim `WRAP` marker beside `FOLLOW`/`SCROLL` while
  wrapping is on, so the mode is discoverable without pressing a key.

### Scroll position across the toggle

- Following the tail: stays following. Pressing `w` never leaves the tail.
- Scrolled up: the logical line currently at the **bottom** of the viewport
  stays at the bottom after the mode change. This matches the viewport's
  existing bottom-anchored model.

## Design

The one real hazard is that `scroll_offset`, `content_len`, `max_offset` and
paging are all in units of *rows*, and today rows and logical lines are the
same thing — `render_body` hands `Paragraph::scroll((top, 0))` a logical line
index and it happens to be correct.

Enabling ratatui's `Wrap` would break that silently: ratatui counts its scroll
offset in rendered rows while `content_len` would still count logical lines, so
the view would drift arbitrarily in a long buffer. Re-deriving ratatui's
word-wrap row counts ourselves to compensate would mean guessing at its
algorithm.

**So krawatte does the wrapping itself**, producing one `TuiLine` per visual
row before the `Paragraph` ever sees it. "One line = one row" stays true, and
every existing line of scroll logic keeps working unchanged. Hard wrapping
makes the row count plain arithmetic, and the wrap itself a pure function.

### Components (all in `src/ui.rs`)

- `UiState.wrap: bool`, default `false`.
- `KeyCommand::ToggleWrap`, bound to `w` in `map_key`.
- `tagged_line` also returns the prefix width in cells (timestamp + tag +
  stderr marker) — that is what the continuation indent must match.
- `wrap_line(line, prefix_width, width) -> Vec<TuiLine<'static>>`: splits span
  content at grapheme cluster boundaries by display width, so wide characters
  are never sliced mid-cell, and preserves each span's `Style` across a break.
  Continuation rows are prefixed with `Span::raw(" ".repeat(indent))`.
  Guards: a `width` of 0 and a prefix wider than half the viewport both fall
  back to an indent of 0; the content width available on a row is never less
  than one cell, so the function always terminates.
- `render_body` maps content lines through `wrap_line` when `wrap` is on;
  `content_len` becomes the visual row count. `apply_scroll`, `max_offset` and
  the paging keys are untouched.

### Anchoring

Render caches `line_starts: Vec<usize>` — the first visual row of each logical
line. Toggling `w` while following does nothing to the scroll state. Otherwise
it records the logical index of the bottom visible line as a pending anchor;
the next render recomputes `scroll_offset` so that line's last row is the
bottom visible row again, clamped into range.

Terminal resizes are out of scope: the wrap is recomputed from the current
width every frame, but the offset is left as-is, so the view may shift a
little — the same as today.

### Dependencies

`unicode-width` and `unicode-segmentation` become direct dependencies. Both are
already in the tree via ratatui.

## Testing

Pure-function tests for `wrap_line`:

- a line that fits produces exactly one row, unchanged
- a line of exactly the viewport width does not spill a blank second row
- continuation rows carry the expected indent
- a span's style survives a break in the middle of it
- a wide (double-width) character is never split across rows
- an empty line still yields one row
- degenerate widths (0, narrower than the prefix) terminate and stay sane

State tests:

- wrapping is off initially; `w` toggles it
- toggling while following leaves the view following
- toggling while scrolled keeps the bottom logical line at the bottom

The existing UI tests must pass unmodified — that is the evidence that the
scroll model did not change.

## Out of scope

- Horizontal scrolling as an alternative to wrapping.
- A CLI flag or persisted preference.
- Word wrapping.
- Preserving position across terminal resize.
