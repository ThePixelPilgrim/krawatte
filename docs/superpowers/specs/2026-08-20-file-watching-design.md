# File watching (spec D)

Spec D of the [roadmap](2026-08-20-roadmap.md). Depends on A (the restart
primitive) and B (`watch`/`ignore` entries in the Krawattefile). This is the
spec that retires watchexec.

## Problem

Restart-on-change lives in watchexec, wrapped around each command. watchexec
puts its child in a process group krawatte does not own, so the child can
outlive krawatte; and `cargo run` under watchexec is a poor restart unit
(compile and serve in one process, see the roadmap).

## Goal

A slot with `watch` entries is restarted by krawatte itself when a watched
path changes: tear down the current generation, spawn the standard command.
Same primitive as `r`, different trigger.

## Behavior

### What is watched

`watch` is either the bare string `"self"` or an array of paths:

| Form | Meaning |
|---|---|
| `watch = "self"` | the file named by the first whitespace-separated token of `cmd` |
| `watch = ["…", …]` | each entry is a path (see below); `"self"` inside the array is a path like any other |
| `watch = "src"` (any other bare string) | error: use the array form — the bare-string form is reserved for the keyword, so the keyword can never shadow a file |

The disambiguation is the form, not the spelling: a project that really has
a file or directory called `self` watches it with `watch = ["self"]`.

Each array entry is one of:

| Entry | Meaning |
|---|---|
| a directory | watched recursively |
| a file | its parent directory is watched, events filtered to that file name — so a tool that writes a temp file and renames it into place (cargo, most editors) is still seen, and the file is always complete when it is |

Relative entries and the `self` token resolve against the slot's working
directory (`cwd` if set, else the project dir — spec B) — the same base the
command itself uses for relative paths.

Load-time validation (reported with the other config errors, exit 2):

- a directory entry must exist;
- a file entry's *parent* must exist; the file itself may be absent — a
  binary that has not been built yet is the normal first-launch case, and its
  first appearance is a change;
- `"self"` requires the command's first token to contain a `/`. A bare
  `npm` or `cargo` resolves through `$PATH` and is not what anyone means by
  "watch myself"; the error says so.

### What counts as a change

Create, write, remove and rename events on a path that is not ignored.
Attribute-only and access events are not changes.

`ignore` is a per-slot array of globs, matched against the path relative to
the watched root and against the bare file name. A built-in list applies to
every slot and cannot be disabled in this spec: `.git`, `target`,
`node_modules`, `*.swp`, `*.swx`, `*~`, `.#*`, `#*#`, `4913`, `.DS_Store`.
Ignored *directories* are also not descended into, which keeps the inotify
watch count sane for a slot that watches `.`.

### Debounce and restart

Events for a slot are coalesced: the restart fires once the slot has seen no
event for `settings.debounce_ms` (default `100`). A save that touches
twenty files produces one restart.

On fire, the main loop applies these rules in order:

1. A restart is already in flight for the slot → drop. The new generation
   has not spawned yet; when it does it reads the disk as it is then.
2. The slot is running an override (spec C) → drop. Overrides are pinned.
3. Otherwise `replace(slot, standard)`, health `↻`, exactly as `r`.

A dead slot (a build that failed, a server that crashed) is restarted by a
change like any other — that is the build slot's whole life cycle: it runs,
exits, and waits for the next edit. There is still no crash-restart; only a
change brings a dead slot back.

### What the user sees

The marker block (spec A) gains a trigger in its header and a line naming
what changed:

```
── restart · gen 2 → 3 · 14:02:11 · watch ──
── changed: platform/server/src/main.rs (+2 more) ──
── gen 2: pid 47105 · killed by signal 15 · ran 4m12s ──
── gen 3: pid 48213 ──
── cmd: target/debug/erhebimus ──
```

Paths are shown relative to the project dir; at most three are named, the
rest counted. Key-triggered restarts show `· key r` / `· key k` in the
header from now on, so every marker says why it exists.

A slot with watches shows a dim `w` after its name in the status bar
(`[1] build w ●`), so it is visible which slots restart on their own.

### The two-stage cargo setup, end to end

```toml
[[proc]]
name  = "build"
cmd   = "cargo build -p erhebimus"
watch = ["platform/server/src", "platform/server/migrations"]

[[proc]]
name  = "server"
cmd   = "target/debug/erhebimus"
watch = "self"
```

1. Launch: both spawn. `build` compiles; `server` runs the existing binary
   (or shows `✖ spawn` if there is none yet).
2. `build` finishes; cargo hard-links the new binary into `target/debug/` —
   a create event for `erhebimus` in that directory → `server` restarts.
3. Edit `src/main.rs` → `build` restarts (killing an in-progress compile if
   any; cargo tolerates that). Compile fails → `build` shows `✖ exit 101`,
   the binary is untouched, `server` keeps running the last good build.
4. Fix the edit → `build` exits 0 → binary replaced → `server` restarts.

watchexec is no longer involved anywhere.

## Design

### Module: `watch.rs` (new)

Three parts, the first two pure and unit-tested:

**Resolution** — `resolve(spec: &ProcSpec, project_dir) -> Result<Vec<WatchTarget>, ConfigError>`
where `WatchTarget` is `Dir(PathBuf)` or `File { dir: PathBuf, name: OsString }`.
Runs at load time, after `config::parse`, and feeds its errors into the same
error list.

**Debouncer** — a state machine driven by an explicit clock, like
`ShutdownMachine`:

```rust
pub struct Debouncer { quiet: Duration, pending: HashMap<ProcId, Pending> }
impl Debouncer {
    pub fn observe(&mut self, proc: ProcId, path: PathBuf, now: Instant);
    pub fn next_deadline(&self) -> Option<Instant>;
    pub fn due(&mut self, now: Instant) -> Vec<Changed>;   // Changed { proc, paths: Vec<PathBuf>, more: usize }
}
```

**Watcher thread** — owns one `notify::RecommendedWatcher` (inotify on
Linux). At startup it registers every distinct directory root (recursive for
`Dir`, non-recursive for a `File`'s parent) and builds a table from root →
the slots interested in it, plus each slot's filter (file name or ignore
set). On each raw event it filters, maps the path to slots, and calls
`observe`; it sleeps until `next_deadline` and sends `Event::Changed` for
each `due` entry on the existing mpsc channel. It is detached like the
reader threads: nothing waits on it, and it ends with the process.

### Changes elsewhere

- `types.rs`: `Event::Changed(Changed)`; `Trigger { Key(char), Watch(Vec<PathBuf>, usize) }`
  (spec C adds `Cli(..)` and `Resume`).
- `proc.rs`: `replace(proc, command, trigger)`; `Transition.trigger`.
  Overrides' pinning check lands with spec C; D's handler calls a
  `manager.is_override(proc)` that C introduces — until then D's handler
  only checks `is_restarting`.
- `marker.rs`: header trigger, `changed:` line.
- `ui.rs`: `w` status marker; `UiState::new` takes which slots are watched.
- `main.rs`: handle `Event::Changed` with the three rules above.
- `Cargo.toml`: `notify`, `globset`.

### Failure modes

- inotify watch limit reached (`ENOSPC` from `notify`): reported as a config
  error at launch naming the slot and suggesting
  `fs.inotify.max_user_watches`; krawatte does not start half-watched.
- A watched directory is deleted at runtime: the watch silently dies for that
  root (inotify semantics). Not handled in this spec; documented.
- Restart storms (a tool rewriting a watched tree continuously): bounded by
  the debounce plus the in-flight rule; a slot can restart at most once per
  teardown. No further rate limiting.

## Testing

- `resolve`: dir, file, `self` with and without `/`, relative to cwd vs
  project dir, missing dir error, missing file with present parent accepted,
  missing parent error; bare `watch = "src"` rejected; `watch = ["self"]`
  resolves to a path named `self`, not to the command.
- `Debouncer` with a virtual clock: one event fires after `quiet`; a burst
  fires once with all paths (and `more` counting beyond three); two slots
  fire independently; `next_deadline` is the earliest pending.
- Ignore matching: defaults and per-slot globs, root-relative and bare-name
  forms, ignored directory not descended.
- Watcher thread against a tempdir: touching a file under a watched dir
  yields `Event::Changed` for the right slot within 2 s; writing
  `tmp` then renaming it onto a watched *file* name yields a change for the
  file slot and nothing for an unrelated slot.
- `main` handler with crafted `Event::Changed`: in-flight → no `replace`;
  otherwise `replace` called and health `↻`.
- Acceptance: the erhebimus Krawattefile above, run by hand through steps
  1–4; `make dev-backend` and `dev-payment` switched to `krawatte` and the
  watchexec lines deleted.

## Out of scope

`.gitignore` awareness; polling fallback for filesystems without inotify;
re-registering deleted roots; per-slot debounce; a "restart on exit N" or
crash-restart policy.

## Decisions made in this spec (to confirm)

- Quiet-period debounce (restart after the burst ends) rather than a fixed
  throttle; default 100 ms, global setting not per slot.
- A change during an in-flight restart is dropped, not queued.
- Built-in ignore list is fixed; per-slot `ignore` only adds to it.
- `self` requires a path-like first token; `"self"` on `npm …` is an error.
- Bare string is only ever the `self` keyword; all paths go in an array.
- A missing watched *file* is allowed at launch (parent must exist).
- Status bar marks watched slots with a dim `w`.
- `notify` + `globset` as dependencies; debounce hand-rolled so it is
  testable with a virtual clock.
