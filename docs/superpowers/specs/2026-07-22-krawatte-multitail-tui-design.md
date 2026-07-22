# krawatte — multi-process tail TUI

**Date:** 2026-07-22
**Status:** Approved design

## Purpose

A full-screen terminal program that spawns multiple child programs, follows
their output simultaneously, and manages their lifecycle. Ctrl-C shuts down
all children, verifies they are gone, restores the terminal, and exits.

## Scope (v1)

- Rust binary, no async runtime: threads + `mpsc` channels.
- Dependencies: `ratatui`, `crossterm`, `ansi-to-tui`, `nix` (or `libc`) for
  signals/process groups.
- Children run through pipes (not PTYs). ANSI colors in output are parsed and
  rendered; forcing children to emit color is out of scope (user-side).
- PTY-per-pane terminal emulation is an explicit v2 candidate, not v1.

## CLI

```
krawatte "cargo watch -x check" "npm run dev" "python worker.py"
```

Each positional argument is one command, executed via `sh -c <arg>`.
No config file in v1.

## Architecture

### Process manager
- Each child is spawned in its own process group (`setpgid`) so signals reach
  the whole child tree.
- stdout and stderr are piped separately; one reader thread per stream sends
  `(process_id, stream_tag, line_bytes)` messages over a shared `mpsc` channel.
- Child exit is detected by a per-child waiter thread (or reader EOF + `try_wait`)
  and reported on the same channel.

### Shutdown sequence (Ctrl-C or `q`)
1. Send SIGTERM to every live child's process group.
2. Wait up to a 5-second grace period, polling for exits.
3. SIGKILL any process group still alive.
4. Reap all children (`wait`), record exit statuses.
5. Restore the terminal (leave alternate screen, disable raw mode).
6. Print each child's final status to the normal screen, exit.

Terminal restore and child cleanup are wrapped in a drop guard so a panic in
the UI never leaves a raw terminal or orphaned children.

### Per-process buffer
- Ring buffer of ~10,000 styled lines per process.
- Each incoming line is ANSI-parsed once on arrival into styled spans;
  stderr lines are tagged (rendered dim/red-tinted marker).
- The interleaved view is produced by merging buffers in arrival order
  (a shared global sequence number per line).

### UI
- **Top status bar:** one slot per process — index, short command name, and
  health: `●` running (green) / `✖ exit N` (red) / `✔ exit 0` (gray).
- **Body:** either
  - *All view:* interleaved lines, each prefixed with a per-process colored tag, or
  - *Single pane:* one process's buffer in isolation.
- **Scrolling:** PgUp/PgDn/arrow keys scroll within the current view; a scrolled
  view stops auto-following; scrolling back to the bottom resumes follow.

### Keybindings
| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | cycle forward / backward through all-view and single panes |
| `1`–`9` | jump to pane N |
| `0` or `a` | interleaved all-view |
| `PgUp`/`PgDn`/`↑`/`↓` | scroll (bottom resumes follow) |
| `q` or Ctrl-C | shut down all children and exit |

Ctrl-C is received as a key event (raw mode), not a signal, so it enters the
same orderly shutdown path as `q`.

## Failure handling
- A child exiting on its own marks its slot dead (exit code shown in the
  status bar); the other children keep running. Its buffer stays viewable.
- Spawn failure of one command is shown as an immediately-dead slot.
- No automatic restart in v1.

## Testing
- Unit tests: ring buffer behavior, ANSI parse-to-spans, interleave ordering,
  shutdown state machine (TERM → grace → KILL sequencing, using stub signals).
- The ratatui render layer stays thin and mostly untested; logic lives in
  plain testable modules.
