# krawatte

A full-screen terminal multi-tail: run several programs at once, follow their
output together or one at a time, and shut everything down cleanly with a
single Ctrl-C.

## Usage

Ad hoc — each argument is one command, run via `sh -c` in its own process
group:

```
krawatte "cargo watch -x check" "npm run dev" "python worker.py"
```

From a project — with no commands, krawatte looks for a `Krawattefile` in the
current directory or any parent and launches it:

```
krawatte
```

| Option | Meaning |
|---|---|
| `-t`, `--timeout <SECS>` | grace period between SIGTERM and SIGKILL on shutdown (overrides the file's `settings.timeout`; default `5`) |
| `-f`, `--file <PATH>` | launch this Krawattefile instead of searching for one; cannot be combined with commands |

Talk to an already running instance from anywhere in its project:

```
krawatte <subcommand>    # status, restart, kill, stop, start, run, quit, logs
```

See [Controlling a running krawatte](#controlling-a-running-krawatte).

## Keys

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | cycle forward / backward through the all-view and single panes |
| `1`–`9` | jump to pane N |
| `0` or `a` | interleaved all-view |
| `d` | cycle line timestamps: off → ISO datetime → time only → relative |
| `w` | toggle wrapping of over-wide lines onto continuation rows |
| `r` | restart the viewed pane's process (no-op in the all-view) |
| `k` | kill the viewed pane's process; it is restarted with its standard command, ending any `run` override (no-op in the all-view) |
| `PgUp`/`PgDn`/`↑`/`↓` | scroll (returning to the bottom resumes follow) |
| `q` or Ctrl-C | shut down all children and exit |

## Krawattefile

TOML. Every relative path is resolved against the directory containing the
file (the *project dir*), and a process without `cwd` runs there — never in
the directory krawatte happened to be started from.

```toml
[settings]
timeout = 5.0                 # optional; seconds of grace on shutdown

[[proc]]
name = "build"                # required; unique; letters, digits, `_`, `-`; not all digits; not `all`
cmd  = "cargo build -p app"   # required; run via `sh -c`

[[proc]]
name = "web"
cmd  = "npm run dev"
cwd  = "frontend"             # optional; relative to the project dir; must exist
env  = { PORT = "5174" }      # optional; added to the inherited environment
```

Unknown keys are errors. All problems in the file are reported together,
with line numbers, before the terminal is touched (exit code 2). Slots
appear in file order and the status bar shows their names.

### Restart on change

```toml
[settings]
debounce_ms = 100             # optional; quiet period before a change restarts a slot

[[proc]]
name  = "build"
cmd   = "cargo build -p app"
watch = ["src", "migrations"]  # paths, relative to the slot's working directory
ignore = ["*.snap"]            # globs added to the built-in ignore list

[[proc]]
name  = "server"
cmd   = "target/debug/app"
watch = "self"                 # the file named by the command's first token
```

When a watched path changes, the slot is torn down (TERM → grace → KILL) and
its command run again — the same thing `r` does, with `watch` as the
trigger in the marker block and a `changed:` line naming the paths. A slot
that is dead is simply started. Nothing is ever restarted because it
crashed; only a change does that.

- A directory is watched recursively; `.git`, `target`, `node_modules` and
  editor temp files (`*.swp`, `*~`, `.#*`, `4913`, …) are ignored and not
  descended into.
- A file is watched through its directory, so a tool that writes a temp
  file and renames it into place (cargo, most editors) is seen, and the
  file is complete when it is. The file may not exist yet at launch.
- `watch = "self"` is the only bare-string form; a file literally called
  `self` goes in an array: `watch = ["self"]`. `"self"` needs a path-like
  first token (`target/debug/app`, not `npm`).
- The status bar shows a dim `w` after a watched slot's name.

The two slots above are linked only through the filesystem: `build` reruns
on source edits and exits with the compiler's status; `server` restarts only
when the binary actually changes, so a failed build leaves the old server
running.

## Controlling a running krawatte

While krawatte runs it listens on a unix socket (under `$XDG_RUNTIME_DIR/krawatte/`,
keyed by the project directory). From anywhere in the project:

```
krawatte status                      # every slot: health, generation, pid, command
krawatte restart <SLOT|all> [--wait] # tear down, run the current command again
krawatte kill    <SLOT|all> [--wait] # tear down, run the standard command
krawatte stop    <SLOT|all> [--wait] # tear down, leave stopped
krawatte start   <SLOT|all> [--wait] # start a stopped slot
krawatte run     <SLOT> [--wait] (-- CMD... | --wrap PREFIX)
krawatte quit    [--wait]            # like pressing q
krawatte logs    [SLOT|all] [--tail N] [--since 5m] [--color]
```

`SLOT` is a name from the Krawattefile or a 1-based index. `--wait` returns
once the restart has completed and prints its marker block. `--json` on any
command prints the raw reply, for scripts and agents. `-f PATH` before the
subcommand addresses the instance launched from that Krawattefile.

`run` puts a one-shot **override** in a slot — `--wrap "perf record -g"`
prefixes the standard command, `-- cmd args` replaces it — in the slot's
working directory and environment. When it exits, the standard command
resumes; `k`/`kill` end it early; `r`/`restart` restart the override itself;
file watches leave it alone. The status bar marks an override slot with `*`.

The bar shows `CTRL` while the socket is up. If another krawatte already
serves this project, the new one runs with `NO CTRL`.

Exit codes: `0` ok, `1` refused (unknown slot, restart in flight), `2` usage,
`3` no instance running for this project.

On the wire each request is one JSON object per line with `"v": 1` and a
`"cmd"` tag; `run`'s argv travels under the key `"command"`, e.g.
`{"v":1,"cmd":"run","slot":"server","command":["perf","record"]}`.

## Behavior

- **Status bar** shows each process slot: index, command name, and health
  (`●` running, `↻` restarting, `✔ exit 0`, `✖ exit N`). A `*` after the name
  marks a slot running a `run` override; `CTRL` / `NO CTRL` at the right says
  whether this instance owns the control socket.
- **Interleaved view** merges all outputs in arrival order, each line prefixed
  with a colored per-process tag; single panes show one program in isolation.
- **Scrollback**: ~10,000 lines per process, ANSI colors preserved.
- **Timestamps** (`d`) prefix each line with its arrival time, in local time or
  as an age (`12m ago`) that keeps counting up. Off by default.
- **Wrapping** (`w`) is off by default: long lines are clipped at the right
  edge. Toggled on, they break hard at the last cell that fits and continue on
  the next row, indented under the content column so the timestamp/tag prefix
  stays its own column. The status bar shows `WRAP` while it is on, and
  toggling keeps the line you were reading at the bottom of the viewport.
- **A child exiting** marks its slot dead (exit code shown); the others keep
  running and its buffer stays viewable.
- **Restart** (`r`/`k` in a single pane) sends SIGTERM to that child's process
  group, waits out the grace period, SIGKILLs stragglers, then runs the same
  command again in the same slot. The UI stays live throughout; the slot shows
  `↻` while the old process is being torn down. The buffer is kept and a dim
  marker block records the transition — what triggered it (`key r`, `key k`
  or `watch`), generation numbers, pids, how the old process ended and how
  long it ran, and the command. Output that arrives late
  from the old process is discarded. A child that exits on its own is *not*
  restarted.
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
