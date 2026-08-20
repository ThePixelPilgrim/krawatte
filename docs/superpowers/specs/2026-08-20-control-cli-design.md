# Control socket and CLI (spec C)

Spec C of the [roadmap](2026-08-20-roadmap.md). Depends on A (restart
primitive, marker block), B (names, project dir) and D (trigger field). Adds
the one-shot override.

## Problem

Everything krawatte knows — which slot is up, what it printed, the ability
to restart it — is reachable only through a full-screen TUI. A coding agent
working in the same checkout has no terminal to press `r` in and no way to
read a slot's output.

## Goal

A running krawatte listens on a unix socket. `krawatte <subcommand>` from
anywhere in the project talks to it: status, restart, kill, one-shot
override, and log retrieval. Output is human-readable by default and JSON on
request.

## Behavior

### CLI

```
krawatte status            [--json]
krawatte restart <SLOT>    [--wait] [--json]
krawatte kill    <SLOT>    [--wait] [--json]
krawatte run     <SLOT>    [--wait] [--json] (-- <CMD>... | --wrap <PREFIX>)
krawatte logs    [SLOT]    [--tail N] [--since DUR] [--color] [--json]
```

- `SLOT` is a slot name (Krawattefile) or a 1-based index (works in ad-hoc
  mode too). `logs` with no slot, or `all`, interleaves every slot in
  arrival order with a `name│` prefix, as the all-view does.
- `restart`/`kill`/`run` return as soon as the request is accepted (exit 0,
  printing the slot and the generation that is being torn down). `--wait`
  blocks until the transition completes and prints the marker block — so an
  agent can `krawatte restart server --wait && krawatte logs server --since 10s`.
  The CLI gives up after the grace period plus ten seconds.
- `run` spawns a **one-shot override** in the slot: `-- <CMD>...` is the
  full command (joined with spaces, run via `sh -c` in the slot's cwd/env);
  `--wrap <PREFIX>` runs `<PREFIX> <standard command>`
  (`--wrap "perf record -g"`). When the override exits on its own, the
  standard command is respawned (marker header `resume`). `k` or
  `krawatte kill` ends it early; `r` or `krawatte restart` restarts the
  override itself; watch events leave it alone (spec D rule 2).
- `logs`: `--tail N` (default 100) newest lines; `--since DUR` (`30s`, `5m`,
  `1h`) instead of or in addition to `--tail` (both: whichever is smaller).
  ANSI is stripped by default; `--color` emits the stored bytes verbatim.
  Marker lines are included. Each line is `HH:MM:SS text` (with `name│`
  before `text` for `all`); `--json` gives
  `{"seq","at_ms","gen","proc","name","stream","text"}` per line, stripped
  unless `--color`.
- `status` human form:

  ```
  krawatte 48001 · /home/c/Projects/erhebimus
  [1] build    ✔ exit 0   gen 4   12s      cargo build -p erhebimus
  [2] server*  ● pid 48213   gen 3   4m12s   perf record -g target/debug/erhebimus
  [3] web      ● pid 47222   gen 0   31m     npm run dev:debug
  ```

  `*` marks an override; the last column is the *current* command.

Exit codes: `0` ok; `1` the instance refused (unknown slot, restart already
in flight, bad request); `2` usage; `3` no instance is running for this
project (`krawatte: no krawatte running for /home/c/Projects/erhebimus`).

Instance discovery mirrors launch (spec B): walk up to a `Krawattefile`, else
use the cwd (an ad-hoc instance launched there). `-f PATH` points at an
explicit file.

### TUI side

- The status bar shows `*` after an override slot's name and a dim `CTRL`
  marker at the far right while the socket is listening; `NO CTRL` if it
  could not be bound (see below).
- Marker headers show the trigger: `cli restart`, `cli kill`, `cli run`,
  `resume`, alongside D's `key r`/`key k`/`watch`.
- The override is a property of the *current generation*: `r` restarts it
  (same command, still an override); `k` and self-exit return the slot to the
  standard command; a new `run` while an override is running replaces the
  override (subject to the in-flight rule).

### Socket

- Path: `$XDG_RUNTIME_DIR/krawatte/<hash>.sock`, falling back to
  `/tmp/krawatte-<uid>/<hash>.sock`; directory created `0700`, socket `0600`.
  `<hash>` is the 64-bit FNV-1a of the canonical project dir, hex. Keeping
  the socket out of the project tree means nothing to gitignore and no stale
  files in the checkout; hashing keeps the path under the 108-byte
  `sockaddr_un` limit.
- Bind at startup, before entering the alternate screen. If the path is
  taken: try connecting; success means another instance owns this project —
  run without a socket, show `NO CTRL`, print a notice after exit. Failure
  means a stale socket from a crashed instance — unlink and bind.
- Unlinked on shutdown, including the panic path (a drop guard next to
  `TerminalGuard`).
- Protocol: one request per connection, one JSON object per line each way.
  Requests carry `"v": 1`. Unknown version or malformed JSON gets
  `{"ok":false,"error":"..."}` and the connection closes.

```
→ {"v":1,"cmd":"restart","slot":"server","wait":true}
← {"ok":true,"proc":1,"name":"server","from_gen":2,"to_gen":3,"marker":["── restart · gen 2 → 3 · 14:02:11 · cli restart ──", ...]}

→ {"v":1,"cmd":"logs","slot":null,"tail":100,"since_ms":300000,"color":false}
← {"ok":true,"lines":[{"seq":8812,"at_ms":1755691331000,"gen":3,"proc":1,"name":"server","stream":"stdout","text":"listening on :8080"}, ...]}

→ {"v":1,"cmd":"status"}
← {"ok":true,"pid":48001,"dir":"/home/c/Projects/erhebimus","procs":[{"index":1,"name":"build","health":"exit 0","gen":4,"pid":null,"command":"cargo build -p erhebimus","standard":"cargo build -p erhebimus","override":false,"since_ms":12000}, ...]}
```

## Design

### Module: `control.rs` (new)

- `Request` / `Response` enums with `serde` (`#[serde(tag = "cmd")]`).
- `socket_path(project_dir) -> PathBuf` (pure given `XDG_RUNTIME_DIR`/uid).
- `Listener::bind(path) -> Result<Listener, BindOutcome>` with the
  stale/live distinction; `Listener` owns the `UnixListener` and the accept
  thread; `Drop` unlinks.
- Accept thread: per connection, read one line, parse, send
  `Event::Control { request, reply: Sender<Response> }` on the main channel,
  then block on `reply.recv()` with a timeout and write the line. Threads are
  cheap here and keep the socket code free of any shared state.
- `handle(request, &mut ProcManager, &BufferSet, &UiState) -> Handled`
  where `Handled::Now(Response)` or `Handled::AfterTransition { proc, reply }`.
  Pure given the manager; this is the unit-test surface.

### `main.rs`

- `Event::Control` → `control::handle`; `Now` replies immediately,
  `AfterTransition` is parked in a `Vec<(ProcId, Sender<Response>)>` that
  `apply_transition` drains for the matching slot, sending the marker block.
- Self-exit of an override: on `Event::Exited` for the current generation of
  an override slot, `replace(p, standard, Trigger::Resume)`.

### `proc.rs`

- `Proc.kind: GenKind { Standard, Override }` for the current generation;
  `replace_with(proc, command, kind, trigger)`; `replace` keeps the current
  kind, `kill` sets `Standard`. `is_override(proc)`.
- `status()` snapshot: per slot name, health-ish state, gen, pid, command,
  standard, kind, started-at.

### `buffer.rs`

`StyledLine` keeps `raw: Vec<u8>` alongside the parsed spans so `--color`
can return exactly what the process wrote. Stripped text is the
concatenated span contents, which the parser already produced. Marker lines
have `raw == text`.

### CLI client (`cli.rs`, new)

clap subcommands on the existing `Cli` struct (`#[command(subcommand)]`
optional, so bare `krawatte` and positional commands keep working). The
client is synchronous std `UnixStream`: connect, write one line, read one
line, render. No async runtime.

### Dependencies

`serde_json` (serde already arrives with B). Nothing else.

## Testing

- `socket_path` stable for a given dir and env; distinct for distinct dirs;
  under 100 bytes for a long project path.
- `Listener::bind`: fresh path; stale socket file (created, no listener) is
  replaced; live socket (a test listener) yields the "another instance"
  outcome.
- `control::handle` with a real manager and buffers: `status` shape;
  `restart` by name and by index, unknown slot, in-flight → error; `logs`
  tail/since/all ordering and stripping; `run --wrap` builds the right
  command and marks the slot an override; `kill` on an override returns to
  standard.
- Override self-exit: a `run` of `sh -c 'sleep 0.2'` is followed by a
  `resume` transition back to the standard command, with `kind == Standard`.
- Watch pinning (D's rule 2): crafted `Event::Changed` for an override slot
  does not call `replace`.
- End to end: bind a listener on a tempdir path, spawn the accept thread
  with a channel, connect with `UnixStream`, send `status`, receive the
  reply; malformed JSON gets an error reply.
- CLI parsing: each subcommand's args; `run` requires exactly one of `--`
  or `--wrap`; `--since` duration parsing (`30s`, `5m`, `1h30m`, rejects
  `5`).
- Manual: with erhebimus running, `krawatte restart server --wait`,
  `krawatte logs server --since 10s`, `krawatte run server --wrap "perf record -g"`,
  wait, `krawatte status` shows `*`, `krawatte kill server`, status shows no `*`.

## Out of scope

`logs --follow` (streaming); starting a cluster without a TTY; multiple
clients sharing one connection; authentication beyond filesystem
permissions (same-uid only, which is the threat model of a dev tool);
`krawatte stop` for the whole instance (press `q`).

## Decisions made in this spec (to confirm)

- Socket lives under `$XDG_RUNTIME_DIR`, keyed by a hash of the project dir,
  not inside the project.
- A second instance for the same project runs without control rather than
  refusing to start.
- One request per connection, line-delimited JSON, version field.
- `--wait` is opt-in; default returns on acceptance.
- `logs` default is the newest 100 lines, stripped; `--color` returns raw
  bytes, which requires storing the raw line (memory cost: the raw bytes of
  ≤10k lines per slot).
- Override is a property of the current generation; `r` keeps it, `k` and
  self-exit drop it; a second `run` replaces it.
- `logs --follow` deferred; agents poll with `--since`.
- Status bar shows `*` for overrides and `CTRL`/`NO CTRL` for the socket.
