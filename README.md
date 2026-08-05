# krawatte

A full-screen terminal multi-tail: run several programs at once, follow their
output together or one at a time, and shut everything down cleanly with a
single Ctrl-C.

## Usage

```
krawatte "cargo watch -x check" "npm run dev" "python worker.py"
```

Each argument is one command, run via `sh -c` in its own process group.

| Option | Meaning |
|---|---|
| `-t`, `--timeout <SECS>` | grace period between SIGTERM and SIGKILL on shutdown (default `5`) |

## Keys

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | cycle forward / backward through the all-view and single panes |
| `1`–`9` | jump to pane N |
| `0` or `a` | interleaved all-view |
| `d` | cycle line timestamps: off → ISO datetime → time only → relative |
| `PgUp`/`PgDn`/`↑`/`↓` | scroll (returning to the bottom resumes follow) |
| `q` or Ctrl-C | shut down all children and exit |

## Behavior

- **Status bar** shows each process slot: index, command name, and health
  (`●` running, `✔ exit 0`, `✖ exit N`).
- **Interleaved view** merges all outputs in arrival order, each line prefixed
  with a colored per-process tag; single panes show one program in isolation.
- **Scrollback**: ~10,000 lines per process, ANSI colors preserved.
- **Timestamps** (`d`) prefix each line with its arrival time, in local time or
  as an age (`12m ago`) that keeps counting up. Off by default.
- **A child exiting** marks its slot dead (exit code shown); the others keep
  running and its buffer stays viewable.
- **Ctrl-C / `q`** sends SIGTERM to every child's process group, waits out the
  grace period (`--timeout`, default 5s), SIGKILLs stragglers, reaps
  everything, restores the terminal, and prints each child's final status.
  Groups are signalled even when the child that led them has already exited,
  so background jobs it left behind are shut down too rather than orphaned.
  Shutdown is bounded: it never waits on a child it cannot force to exit, so
  quitting always returns.

A child that puts itself in a new session (`setsid`, or a tool that
daemonizes) leaves krawatte's process group by design and so outlives it —
that is the daemon's intent, and no pgid can reach it.

Children write to pipes, not a TTY, so many tools disable color by default —
force it per tool if you want it (e.g. `cargo ... --color=always`,
`CLICOLOR_FORCE=1`).

## Building

```
cargo build --release
```

Linux/Unix only (uses process groups and POSIX signals).

## License

MIT — see [LICENSE](LICENSE).
