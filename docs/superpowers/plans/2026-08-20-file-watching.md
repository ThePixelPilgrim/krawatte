# File Watching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Red–green TDD is mandatory** (see Global Constraints): test first, watch it fail, then implement. Use superpowers:test-driven-development for every task.

**Goal:** A slot with `watch` entries is restarted by krawatte itself when a watched path changes — same primitive as `r`, different trigger — so watchexec is no longer needed.

**Architecture:** A `Trigger` travels with every restart and lands in the marker header. A new `watch.rs` has three parts: pure *resolution* of `Watch` entries into directory/file targets with an ignore `GlobSet`; a pure, virtual-clock *`Debouncer`*; and a detached *watcher thread* owning a `notify` watcher and a directory registry that descends trees itself (skipping ignored directories, so `node_modules`/`target` never inflate the inotify count) and emits `Event::Changed` on the existing channel. `main` applies two rules — drop if a restart is in flight, else `replace(standard)` — and the status bar marks watched slots.

**Tech Stack:** Rust 2024, `notify` 8 (inotify backend), `globset` 0.4 (new); `tempfile` (dev) already present.

**Spec:** `docs/superpowers/specs/2026-08-20-file-watching-design.md`. Roadmap: `docs/superpowers/specs/2026-08-20-roadmap.md`.

## Global Constraints

- **Red–green TDD is mandatory.** Test first; run it and *observe the expected failure*; minimal implementation; green run; refactor under green. Never write implementation before the red run. Do not reorder or batch steps across tasks.
- `gen` is a reserved keyword in edition 2024; the codebase spells it `r#gen`.
- Baseline: `cargo test -q` → 108 passed, `cargo clippy --all-targets -q` silent, `cargo fmt --check` clean. All three stay clean after every task; `cargo fmt` before committing.
- Nothing waits unboundedly: the watcher thread is detached like the reader threads; `start` registers watches synchronously and fails fast (exit 2 path), never half-watched.
- Rules on a change: (1) restart in flight → drop; (2) *override → drop* is spec C's and is not implemented here; (3) otherwise `replace(slot, standard command)` with `Trigger::Watch`.
- No crash-restart. A dead slot is revived by a change like any other.
- Marker header always names the trigger: `key r`, `key k`, `watch`. Watch markers add `── changed: … ──` with ≤3 project-relative paths and `(+N more)`.
- Ignore defaults are fixed: `.git`, `target`, `node_modules`, `*.swp`, `*.swx`, `*~`, `.#*`, `#*#`, `4913`, `.DS_Store`; per-slot `ignore` adds to them. File targets are not subject to ignore.
- Commit after every task, imperative messages. Do not push.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `src/types.rs` | `Trigger`, `Changed`, `Event::Changed`. |
| `src/proc.rs` | `replace`/`kill` take a `Trigger`; `Transition.trigger`; `standard_command`. |
| `src/marker.rs` | trigger in the header; `changed:` line. |
| `src/config.rs` | `settings.debounce_ms` → `Krawattefile.debounce`. |
| `src/watch.rs` (new) | `WatchTarget`, `SlotWatch`, `resolve_all`; `Debouncer`; `start` (registry + thread). |
| `src/ui.rs` | `set_watched`, dim `w` after watched names; `slot_label` factored out of the status bar. |
| `src/main.rs` | `Launch` struct, watcher started before the terminal, `Event::Changed` rules. |
| `README.md` | watch/ignore docs with the two-stage cargo example. |

---

### Task 1: `Trigger` on every restart, shown in the marker header

**Files:**
- Modify: `src/types.rs`, `src/proc.rs`, `src/marker.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  ```rust
  // types.rs
  pub enum Trigger { Key(char), Watch { paths: Vec<PathBuf>, more: usize } }
  // proc.rs
  pub fn replace(&mut self, proc: ProcId, command: String, trigger: Trigger) -> bool;
  pub fn kill(&mut self, proc: ProcId, trigger: Trigger) -> bool;
  pub fn standard_command(&self, proc: ProcId) -> &str;
  pub struct Transition { pub proc, pub old, pub new, pub trigger: Trigger }
  ```

- [ ] **Step 1: Write the failing tests**

`src/marker.rs` tests — change the existing `Transition { … }` literals to include `trigger: Trigger::Key('r')` and update the header expectations. The first test becomes:

```rust
    #[test]
    fn restart_block_lists_header_old_new_and_command() {
        let t = Transition {
            proc: 0,
            old: Some(old(2, Outcome::Exited(ExitStatus::Signal(15)), 252)),
            new: new(3, Ok(48213)),
            trigger: Trigger::Key('r'),
        };
        assert_eq!(
            restart_block(&t, "14:02:11"),
            vec![
                "── restart · gen 2 → 3 · 14:02:11 · key r ──",
                "── gen 2: pid 47105 · killed by signal 15 · ran 4m12s ──",
                "── gen 3: pid 48213 ──",
                "── cmd: target/debug/erhebimus ──",
            ]
        );
    }

    #[test]
    fn watch_trigger_adds_a_changed_line_with_overflow_count() {
        let t = Transition {
            proc: 0,
            old: Some(old(2, Outcome::Exited(ExitStatus::Signal(15)), 252)),
            new: new(3, Ok(48213)),
            trigger: Trigger::Watch {
                paths: vec![
                    PathBuf::from("platform/server/src/main.rs"),
                    PathBuf::from("platform/server/src/lib.rs"),
                ],
                more: 2,
            },
        };
        let lines = restart_block(&t, "14:02:11");
        assert_eq!(lines[0], "── restart · gen 2 → 3 · 14:02:11 · watch ──");
        assert_eq!(
            lines[1],
            "── changed: platform/server/src/main.rs, platform/server/src/lib.rs (+2 more) ──"
        );
        assert_eq!(lines.len(), 5);

        let one = Transition {
            trigger: Trigger::Watch {
                paths: vec![PathBuf::from("target/debug/erhebimus")],
                more: 0,
            },
            ..t
        };
        assert_eq!(restart_block(&one, "x")[1], "── changed: target/debug/erhebimus ──");
    }
```

In the other marker tests (`restart_block_covers_every_old_outcome`, `restart_block_reports_spawn_failure`) add `trigger: Trigger::Key('r'),` to each literal; the `never started` test's header expectation becomes `"── start · gen 1 · x · key r ──"`. Add `use crate::types::Trigger; use std::path::PathBuf;` to the test module.

`src/proc.rs` tests — every `mgr.replace(0, X)` becomes `mgr.replace(0, X, Trigger::Key('r'))` and every `mgr.kill(0)` becomes `mgr.kill(0, Trigger::Key('k'))`. In `restart_of_live_slot_spawns_a_new_pid_in_the_same_slot` add after the `t.new` assertions:

```rust
        assert_eq!(t.trigger, Trigger::Key('r'));
```

and add a new test:

```rust
    #[test]
    fn standard_command_is_the_configured_one_even_while_running_something_else() {
        let (tx, _rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["sleep 30".to_string()], &short_grace(), tx);
        assert!(mgr.replace(0, "sleep 31".to_string(), Trigger::Key('r')));
        tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(mgr.current_command(0), "sleep 31");
        assert_eq!(mgr.standard_command(0), "sleep 30");
        shutdown_within(mgr, Duration::from_secs(5));
    }
```

`src/main.rs` test `stale_generation_events_are_dropped_and_transitions_write_markers`: `manager.replace(0, "sleep 30".to_string(), Trigger::Key('r'))`, and add `use crate::types::Trigger;` to the test imports.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q marker::`
Expected: compile errors — no `Trigger`, no field `trigger` on `Transition`.

- [ ] **Step 3: Implement**

`src/types.rs` (add `use std::path::PathBuf;`):

```rust
/// Why a slot transition happened. Recorded in the marker block so the buffer
/// says not just *that* a generation was replaced but what asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A hotkey in the TUI (`r` or `k`).
    Key(char),
    /// A watched path changed. `paths` are project-relative and capped for
    /// display; `more` counts the ones not listed.
    Watch { paths: Vec<PathBuf>, more: usize },
}
```

`src/proc.rs`:

- `struct Restart` gains `trigger: Trigger`; `Transition` gains `pub trigger: Trigger`.
- `replace(&mut self, proc, command: String, trigger: Trigger) -> bool` stores it in `Restart { …, trigger }`.
- `kill(&mut self, proc, trigger: Trigger) -> bool`:
  ```rust
      pub fn kill(&mut self, proc: ProcId, trigger: Trigger) -> bool {
          let standard = self.procs[proc].spec.command.clone();
          self.replace(proc, standard, trigger)
      }
  ```
- `complete` ends with `Transition { proc, old, new, trigger: restart.trigger }` (move `trigger` out before `restart.next` is moved, or destructure `let Restart { machine, next, trigger } = restart;` at the top).
- New accessor after `current_command`:
  ```rust
      /// The slot's configured command, regardless of what its current
      /// generation runs.
      pub fn standard_command(&self, proc: ProcId) -> &str {
          &self.procs[proc].spec.command
      }
  ```
- Import `Trigger` from `crate::types`.

`src/marker.rs` — header and changed line:

```rust
pub fn restart_block(t: &Transition, clock: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(5);

    let header = match &t.old {
        Some(o) => format!("restart · gen {} → {}", o.r#gen, t.new.r#gen),
        None => format!("start · gen {}", t.new.r#gen),
    };
    lines.push(rule(&format!("{header} · {clock} · {}", trigger_label(&t.trigger))));

    if let Trigger::Watch { paths, more } = &t.trigger {
        let listed: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let suffix = if *more > 0 { format!(" (+{more} more)") } else { String::new() };
        lines.push(rule(&format!("changed: {}{suffix}", listed.join(", "))));
    }
    // … old / new / cmd lines unchanged …
```

```rust
/// Short trigger text for the header: `key r`, `key k`, `watch`.
fn trigger_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Key(c) => format!("key {c}"),
        Trigger::Watch { .. } => "watch".to_string(),
    }
}
```

Import `Trigger` from `crate::types`.

`src/main.rs` `event_loop`: `manager.replace(p, command, Trigger::Key('r'))` and `manager.kill(p, Trigger::Key('k'))`; import `Trigger`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 110 passed, no warnings. (`standard_command` has no non-test caller until Task 5; add `#[allow(dead_code)] // wired in by a later task` if clippy insists, remove in Task 5.)

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/types.rs src/proc.rs src/marker.rs src/main.rs
git commit -m "Record the trigger of every restart in the marker header"
```

---

### Task 2: Resolve `watch` entries and the ignore set; `settings.debounce_ms`

**Files:**
- Create: `src/watch.rs`
- Modify: `src/config.rs`, `src/main.rs` (`mod watch;`), `Cargo.toml`

**Interfaces:**
- Produces:
  ```rust
  // config.rs
  pub struct Krawattefile { …, pub debounce: Option<Duration> }
  // watch.rs
  pub enum WatchTarget { Dir(PathBuf), File { dir: PathBuf, name: OsString } }
  pub struct SlotWatch { pub proc: ProcId, pub targets: Vec<WatchTarget>, pub ignore: GlobSet }
  impl SlotWatch { pub fn matches(&self, path: &Path) -> bool }
  pub fn resolve_all(file: &Krawattefile) -> Result<Vec<SlotWatch>, Vec<ConfigError>>;
  pub fn ignored(set: &GlobSet, relative: &Path) -> bool;
  pub const DEFAULT_IGNORE: &[&str];
  ```

- [ ] **Step 1: Add dependencies**

`Cargo.toml` `[dependencies]`: `globset = "0.4"` and `notify = "8"` (notify is used in Task 4; adding it now keeps one lock-file change). `cargo build -q`.

- [ ] **Step 2: Write the failing tests**

`src/config.rs` tests:

```rust
    #[test]
    fn debounce_ms_is_parsed_into_a_duration() {
        let dir = tempfile::tempdir().unwrap();
        let kf = parse_in(dir.path(), "[settings]\ndebounce_ms = 250\n[[proc]]\nname = \"a\"\ncmd = \"x\"\n").unwrap();
        assert_eq!(kf.debounce, Some(Duration::from_millis(250)));
        let kf = parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncmd = \"x\"\n").unwrap();
        assert_eq!(kf.debounce, None);
    }
```

Create `src/watch.rs`:

```rust
//! Restart-on-change: resolving `watch` entries, debouncing filesystem
//! events, and the watcher thread that turns them into [`Event::Changed`].
//!
//! Resolution and debouncing are pure and unit-tested; only [`start`] talks
//! to `notify`.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{ConfigError, Krawattefile, ProcSpec, Watch};
use crate::types::{Changed, Event, ProcId};

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, command: &str, cwd: &Path, watch: Watch, ignore: &[&str]) -> ProcSpec {
        ProcSpec {
            name: name.to_string(),
            command: command.to_string(),
            cwd: Some(cwd.to_path_buf()),
            env: Vec::new(),
            watch,
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn file_with(project: &Path, procs: Vec<ProcSpec>) -> Krawattefile {
        Krawattefile {
            path: project.join("Krawattefile"),
            project_dir: project.to_path_buf(),
            timeout: None,
            debounce: None,
            procs,
        }
    }

    fn paths(v: &[&str]) -> Watch {
        Watch::Paths(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn directories_files_and_absent_files_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        let kf = file_with(
            &root,
            vec![
                spec("a", "x", &root, paths(&["src", "config.toml", "target/debug/app"]), &[]),
                spec("b", "y", &root, Watch::None, &[]),
            ],
        );
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(slots.len(), 1, "slots without watch are skipped");
        assert_eq!(slots[0].proc, 0);
        assert_eq!(
            slots[0].targets,
            vec![
                WatchTarget::Dir(root.join("src")),
                WatchTarget::File { dir: root.clone(), name: "config.toml".into() },
                WatchTarget::File { dir: root.join("target/debug"), name: "app".into() },
            ]
        );
    }

    #[test]
    fn entries_resolve_against_the_slots_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("frontend/src")).unwrap();
        let kf = file_with(&root, vec![spec("web", "npm run dev", &root.join("frontend"), paths(&["src"]), &[])]);
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(slots[0].targets, vec![WatchTarget::Dir(root.join("frontend/src"))]);
    }

    #[test]
    fn self_watches_the_commands_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let kf = file_with(&root, vec![spec("server", "target/debug/app --port 1", &root, Watch::SelfBinary, &[])]);
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(
            slots[0].targets,
            vec![WatchTarget::File { dir: root.join("target/debug"), name: "app".into() }]
        );

        let abs = file_with(&root, vec![spec("s", &format!("{}/target/debug/app", root.display()), &root, Watch::SelfBinary, &[])]);
        assert_eq!(resolve_all(&abs).unwrap()[0].targets, slots[0].targets);
    }

    #[test]
    fn self_requires_a_path_like_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let kf = file_with(&root, vec![spec("web", "npm run dev", &root, Watch::SelfBinary, &[])]);
        let errs = resolve_all(&kf).unwrap_err();
        assert_eq!(errs.len(), 1);
        let msg = errs[0].to_string();
        assert!(msg.contains("proc \"web\""), "{msg}");
        assert!(msg.contains("\"npm\""), "{msg}");
        assert!(msg.contains("$PATH"), "{msg}");
    }

    #[test]
    fn missing_parent_and_bad_glob_are_errors_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let kf = file_with(&root, vec![spec("a", "x", &root, paths(&["nope/deeper/file"]), &["[unclosed"])]);
        let errs = resolve_all(&kf).unwrap_err();
        assert_eq!(errs.len(), 2, "{errs:#?}");
        assert!(errs[0].to_string().contains("neither does its parent directory"), "{}", errs[0]);
        assert!(errs[1].to_string().contains("ignore pattern"), "{}", errs[1]);
    }

    #[test]
    fn default_and_custom_ignores_match_components_and_names() {
        let set = ignore_set(&["*.log".to_string()]).unwrap();
        assert!(ignored(&set, Path::new("node_modules/x/index.js")));
        assert!(ignored(&set, Path::new("src/.git/HEAD")));
        assert!(ignored(&set, Path::new("src/main.rs.swp")));
        assert!(ignored(&set, Path::new("src/4913")));
        assert!(ignored(&set, Path::new("src/.#main.rs")));
        assert!(ignored(&set, Path::new("logs/app.log")));
        assert!(!ignored(&set, Path::new("src/main.rs")));
        assert!(!ignored(&set, Path::new("targets/main.rs")), "only whole components match");
    }

    #[test]
    fn slot_matches_dir_descendants_unless_ignored_and_exact_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let kf = file_with(&root, vec![spec("a", "x", &root, paths(&["src", "target/debug/app"]), &[])]);
        let slot = resolve_all(&kf).unwrap().remove(0);
        assert!(slot.matches(&root.join("src/a/b.rs")));
        assert!(!slot.matches(&root.join("src/x.swp")));
        assert!(!slot.matches(&root.join("srcs/x.rs")));
        assert!(slot.matches(&root.join("target/debug/app")));
        assert!(!slot.matches(&root.join("target/debug/app.d")));
        assert!(!slot.matches(&root.join("target/debug/deps/app")));
    }
}
```

Add `mod watch;` to `src/main.rs`. `Changed`/`Event::Changed` are defined in Task 3; for this task the `use crate::types::{Changed, Event, ProcId};` line must compile, so add to `src/types.rs` now:

```rust
/// A debounced batch of filesystem changes for one slot, sent by the watcher
/// thread. `paths` are project-relative and capped for display; `more` counts
/// the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub proc: ProcId,
    pub paths: Vec<PathBuf>,
    pub more: usize,
}
```

and the variant `Event::Changed(Changed)` (doc: `/// Watched paths of a slot changed; see [`Changed`].`). `drain_events` in `main.rs` needs an arm for now: `Event::Changed(_) => {}` with a comment `// handled in a later task`.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -q watch::`
Expected: compile errors — `resolve_all`, `WatchTarget`, `ignored`, `ignore_set` not found; `Krawattefile` has no field `debounce`.

- [ ] **Step 4: Implement**

`src/config.rs`: `RawSettings` gains `debounce_ms: Option<u64>`; `Krawattefile` gains `pub debounce: Option<Duration>` (doc: `/// settings.debounce_ms, if given.`), filled with `raw.settings.as_ref().and_then(|s| s.debounce_ms).map(Duration::from_millis)` — read `settings` once into a local before the timeout match so both fields come from it.

`src/watch.rs`, between imports and tests:

```rust
/// Patterns every slot ignores. Editor temp files, VCS and build trees: the
/// things that change constantly and never mean "restart me". Directory
/// names here are also not descended into by the watcher.
pub const DEFAULT_IGNORE: &[&str] = &[
    ".git", "target", "node_modules", "*.swp", "*.swx", "*~", ".#*", "#*#", "4913", ".DS_Store",
];

/// How much of the filesystem one `watch` entry covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTarget {
    /// A directory, watched recursively (minus ignored subtrees).
    Dir(PathBuf),
    /// One file, observed through its parent directory so that a write-then-
    /// rename into place is seen and the file is complete when it is.
    File { dir: PathBuf, name: OsString },
}

/// Everything one slot watches.
#[derive(Debug, Clone)]
pub struct SlotWatch {
    pub proc: ProcId,
    pub targets: Vec<WatchTarget>,
    pub ignore: GlobSet,
}

impl SlotWatch {
    /// Whether a change at `path` (absolute) concerns this slot. Directory
    /// targets cover descendants that are not ignored; file targets match
    /// exactly that file and are never ignored.
    pub fn matches(&self, path: &Path) -> bool {
        self.targets.iter().any(|t| match t {
            WatchTarget::Dir(root) => path
                .strip_prefix(root)
                .is_ok_and(|rel| !ignored(&self.ignore, rel)),
            WatchTarget::File { dir, name } => {
                path.parent() == Some(dir.as_path()) && path.file_name() == Some(name.as_os_str())
            }
        })
    }
}

/// True if the relative path, or any single component of it, matches the
/// set. Matching components makes `target` mean "a directory called target
/// anywhere", as `.gitignore` users expect, without matching `targets`.
pub fn ignored(set: &GlobSet, relative: &Path) -> bool {
    set.is_match(relative)
        || relative
            .components()
            .any(|c| set.is_match(Path::new(c.as_os_str())))
}

/// The default patterns plus a slot's own.
fn ignore_set(extra: &[String]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for pat in DEFAULT_IGNORE.iter().map(|s| s.to_string()).chain(extra.iter().cloned()) {
        let glob = Glob::new(&pat).map_err(|e| format!("ignore pattern {pat:?}: {e}"))?;
        b.add(glob);
    }
    b.build().map_err(|e| e.to_string())
}

/// Resolve every slot's `watch` entries against its working directory.
/// Slots without `watch` are skipped. All errors are returned together so
/// they can join the other config errors.
pub fn resolve_all(file: &Krawattefile) -> Result<Vec<SlotWatch>, Vec<ConfigError>> {
    let mut slots = Vec::new();
    let mut errors = Vec::new();
    let err = |message: String| ConfigError {
        path: file.path.clone(),
        line: None,
        message,
    };
    for (proc, spec) in file.procs.iter().enumerate() {
        let base = spec.cwd.clone().unwrap_or_else(|| file.project_dir.clone());
        let entries: Vec<PathBuf> = match &spec.watch {
            Watch::None => continue,
            Watch::SelfBinary => match self_target(spec) {
                Ok(p) => vec![p],
                Err(m) => {
                    errors.push(err(m));
                    Vec::new()
                }
            },
            Watch::Paths(paths) => paths.iter().map(PathBuf::from).collect(),
        };
        let mut targets = Vec::with_capacity(entries.len());
        for entry in entries {
            let abs = base.join(&entry);
            if abs.is_dir() {
                targets.push(WatchTarget::Dir(abs));
            } else if let (Some(dir), Some(name)) = (abs.parent(), abs.file_name())
                && dir.is_dir()
            {
                targets.push(WatchTarget::File {
                    dir: dir.to_path_buf(),
                    name: name.to_os_string(),
                });
            } else {
                errors.push(err(format!(
                    "proc {:?}: watch path {:?} does not exist and neither does its parent directory",
                    spec.name,
                    entry.display()
                )));
            }
        }
        match ignore_set(&spec.ignore) {
            Ok(ignore) => slots.push(SlotWatch {
                proc,
                targets,
                ignore,
            }),
            Err(m) => errors.push(err(format!("proc {:?}: {m}", spec.name))),
        }
    }
    if errors.is_empty() { Ok(slots) } else { Err(errors) }
}

/// The path `watch = "self"` means: the command's first token, which must
/// look like a path. A bare program name is found through `$PATH` and is not
/// what "watch myself" means.
fn self_target(spec: &ProcSpec) -> Result<PathBuf, String> {
    let first = spec.command.split_whitespace().next().unwrap_or("");
    if first.contains('/') {
        Ok(PathBuf::from(first))
    } else {
        Err(format!(
            "proc {:?}: watch = \"self\" needs a path as the command's first token (got {first:?}); \"self\" cannot watch a program found through $PATH",
            spec.name
        ))
    }
}
```

Imports `fs`, `HashSet`, `BTreeMap`, `mpsc`, `Sender`, `Instant`, `Duration`, `Changed`, `Event` are for Tasks 3–4; keep only what compiles without warnings now and add the rest when needed (clippy treats unused imports as warnings).

- [ ] **Step 5: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 118 passed, no warnings (module-level `#![allow(dead_code)] // wired in by a later task` on `watch.rs` is acceptable until Task 5, which must remove it).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/watch.rs src/config.rs src/types.rs src/main.rs
git commit -m "Resolve watch entries and ignore patterns"
```

---

### Task 3: The `Debouncer`

**Files:**
- Modify: `src/watch.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const SHOWN_PATHS: usize = 3;
  pub struct Debouncer { … }
  impl Debouncer {
      pub fn new(quiet: Duration) -> Debouncer;
      pub fn observe(&mut self, proc: ProcId, path: PathBuf, now: Instant);
      pub fn next_deadline(&self) -> Option<Instant>;
      pub fn due(&mut self, now: Instant) -> Vec<Changed>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
    fn t0() -> Instant {
        Instant::now()
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn fires_once_the_slot_has_been_quiet() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        assert_eq!(d.next_deadline(), None);
        d.observe(0, "a.rs".into(), start);
        assert_eq!(d.next_deadline(), Some(start + ms(100)));
        assert!(d.due(start + ms(99)).is_empty());
        assert_eq!(
            d.due(start + ms(100)),
            vec![Changed { proc: 0, paths: vec!["a.rs".into()], more: 0 }]
        );
        assert!(d.due(start + ms(1000)).is_empty(), "fires once");
        assert_eq!(d.next_deadline(), None);
    }

    #[test]
    fn a_burst_extends_the_quiet_period_and_coalesces_paths() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        d.observe(0, "a".into(), start);
        d.observe(0, "b".into(), start + ms(50));
        d.observe(0, "a".into(), start + ms(80)); // duplicate, not counted again
        d.observe(0, "c".into(), start + ms(120));
        d.observe(0, "d".into(), start + ms(130));
        d.observe(0, "e".into(), start + ms(140));
        assert!(d.due(start + ms(200)).is_empty(), "still within 100ms of the last event");
        assert_eq!(
            d.due(start + ms(240)),
            vec![Changed { proc: 0, paths: vec!["a".into(), "b".into(), "c".into()], more: 2 }]
        );
    }

    #[test]
    fn slots_are_independent_and_returned_in_slot_order() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        d.observe(2, "x".into(), start);
        d.observe(1, "y".into(), start + ms(50));
        assert_eq!(d.next_deadline(), Some(start + ms(100)));
        let first = d.due(start + ms(100));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].proc, 2);
        assert_eq!(d.next_deadline(), Some(start + ms(150)));
        d.observe(0, "z".into(), start + ms(50));
        let rest = d.due(start + ms(150));
        assert_eq!(rest.iter().map(|c| c.proc).collect::<Vec<_>>(), vec![0, 1]);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q watch::fires_once`
Expected: compile error — `Debouncer` not found.

- [ ] **Step 3: Implement**

```rust
/// How many changed paths a marker names; the rest are counted.
pub const SHOWN_PATHS: usize = 3;

/// Per-slot pending change.
#[derive(Debug)]
struct Pending {
    paths: Vec<PathBuf>,
    more: usize,
    last: Instant,
}

/// Quiet-period debounce: a slot's restart fires once `quiet` has passed
/// with no further event for it. Driven by an explicit clock so it can be
/// tested without sleeping, like the shutdown machine.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    pending: BTreeMap<ProcId, Pending>,
}

impl Debouncer {
    pub fn new(quiet: Duration) -> Debouncer {
        Debouncer {
            quiet,
            pending: BTreeMap::new(),
        }
    }

    /// Record a change at `path` (project-relative) for `proc` at time `now`.
    pub fn observe(&mut self, proc: ProcId, path: PathBuf, now: Instant) {
        let p = self.pending.entry(proc).or_insert_with(|| Pending {
            paths: Vec::new(),
            more: 0,
            last: now,
        });
        p.last = now;
        if p.paths.contains(&path) {
            return;
        }
        if p.paths.len() < SHOWN_PATHS {
            p.paths.push(path);
        } else {
            p.more += 1;
        }
    }

    /// When the earliest pending slot would fire if nothing else happens.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.last + self.quiet).min()
    }

    /// Every slot whose quiet period has elapsed by `now`, in slot order;
    /// they are removed from the pending set.
    pub fn due(&mut self, now: Instant) -> Vec<Changed> {
        let ready: Vec<ProcId> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.last) >= self.quiet)
            .map(|(&proc, _)| proc)
            .collect();
        ready
            .into_iter()
            .map(|proc| {
                let p = self.pending.remove(&proc).expect("listed as ready");
                Changed {
                    proc,
                    paths: p.paths,
                    more: p.more,
                }
            })
            .collect()
    }
}
```

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 121 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/watch.rs
git commit -m "Add the quiet-period debouncer"
```

---

### Task 4: The watcher thread

**Files:**
- Modify: `src/watch.rs`

**Interfaces:**
- Produces: `pub fn start(slots: Vec<SlotWatch>, project_dir: PathBuf, quiet: Duration, tx: Sender<Event>) -> Result<(), ConfigError>` — registers every watch synchronously (errors are returned), then detaches a thread that emits `Event::Changed`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn slot_for(root: &Path, proc: ProcId, watch: Watch) -> SlotWatch {
        let kf = file_with(root, vec![spec("s", "x", root, watch, &[])]);
        let mut s = resolve_all(&kf).unwrap().remove(0);
        s.proc = proc;
        s
    }

    fn next_changed(rx: &mpsc::Receiver<Event>, within: Duration) -> Option<Changed> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Changed(c)) => return Some(c),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    #[test]
    fn a_write_under_a_watched_directory_yields_one_changed_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(vec![slot_for(&root, 3, paths(&["src"]))], root.clone(), ms(50), tx).unwrap();

        fs::write(root.join("src/nested/a.rs"), "x").unwrap();
        fs::write(root.join("src/nested/a.rs"), "xy").unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("a change");
        assert_eq!(c.proc, 3);
        assert_eq!(c.paths, vec![PathBuf::from("src/nested/a.rs")]);
        assert!(next_changed(&rx, ms(300)).is_none(), "the burst was coalesced");
    }

    #[test]
    fn a_rename_onto_a_watched_file_is_seen_and_unrelated_files_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(vec![slot_for(&root, 0, paths(&["target/debug/app"]))], root.clone(), ms(50), tx).unwrap();

        fs::write(root.join("target/debug/app.d"), "dep info").unwrap();
        assert!(next_changed(&rx, ms(400)).is_none(), "sibling file is not the target");

        fs::write(root.join("target/debug/app.tmp"), "binary").unwrap();
        fs::rename(root.join("target/debug/app.tmp"), root.join("target/debug/app")).unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("the rename");
        assert_eq!(c.paths, vec![PathBuf::from("target/debug/app")]);
    }

    #[test]
    fn ignored_paths_produce_nothing_and_new_directories_are_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(vec![slot_for(&root, 0, paths(&["."]))], root.clone(), ms(50), tx).unwrap();

        fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(root.join("src/main.rs.swp"), "x").unwrap();
        assert!(next_changed(&rx, ms(400)).is_none());

        fs::create_dir(root.join("src/new")).unwrap();
        // Give the registry a moment to add the new directory before writing into it.
        std::thread::sleep(ms(200));
        let _ = next_changed(&rx, ms(200)); // the directory creation itself is a change; drain it
        fs::write(root.join("src/new/b.rs"), "x").unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("write in a new subdirectory");
        assert!(c.paths.contains(&PathBuf::from("src/new/b.rs")), "{c:?}");
    }

    #[test]
    fn start_fails_fast_on_an_unwatchable_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let slot = SlotWatch {
            proc: 0,
            targets: vec![WatchTarget::Dir(root.join("vanished"))],
            ignore: ignore_set(&[]).unwrap(),
        };
        let (tx, _rx) = mpsc::channel();
        let err = start(vec![slot], root, ms(50), tx).unwrap_err();
        assert!(err.to_string().contains("cannot watch"), "{err}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q watch::a_write_under`
Expected: compile error — `start` not found.

- [ ] **Step 3: Implement**

```rust
use notify::event::ModifyKind;
use notify::{ErrorKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Whether a notify event kind means content changed. Access and metadata
/// (chmod, mtime-only) events are noise.
fn is_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)))
}

/// Owns the `notify` watcher and the set of directories registered with it.
/// Descends directory trees itself, non-recursively per directory, so that
/// ignored subtrees (`target`, `node_modules`, `.git`) are never registered.
struct Registry {
    watcher: RecommendedWatcher,
    dirs: HashSet<PathBuf>,
    /// Directory roots with their slot's ignore set, for deciding whether a
    /// newly created directory should be descended into.
    roots: Vec<(PathBuf, GlobSet)>,
}

impl Registry {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error> {
        if self.dirs.contains(dir) {
            return Ok(());
        }
        self.watcher.watch(dir, RecursiveMode::NonRecursive)?;
        self.dirs.insert(dir.to_path_buf());
        Ok(())
    }

    /// Register `start` and every non-ignored directory below it (relative
    /// to `root` for ignore matching). Symlinked directories are not
    /// followed, so a link cycle cannot recurse forever.
    fn watch_tree(&mut self, root: &Path, start: &Path, ignore: &GlobSet) -> Result<(), notify::Error> {
        let mut stack = vec![start.to_path_buf()];
        while let Some(dir) = stack.pop() {
            self.watch_dir(&dir)?;
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if !is_dir {
                    continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if !ignored(ignore, rel) {
                    stack.push(path);
                }
            }
        }
        Ok(())
    }

    /// A directory appeared under one of the roots: start watching it too.
    fn on_new_dir(&mut self, path: &Path) {
        let matching: Vec<(PathBuf, GlobSet)> = self
            .roots
            .iter()
            .filter(|(root, ignore)| {
                path.strip_prefix(root)
                    .is_ok_and(|rel| !ignored(ignore, rel))
            })
            .cloned()
            .collect();
        for (root, ignore) in matching {
            // Best effort: a failure here only means a subtree is unwatched;
            // the next change at a higher level still restarts the slot.
            let _ = self.watch_tree(&root, path, &ignore);
        }
    }
}

/// Register every slot's targets, then run the watcher on a detached thread
/// that emits [`Event::Changed`] on `tx` after debouncing. Registration
/// failures are returned so the caller can refuse to start half-watched;
/// the inotify limit gets a message naming the sysctl to raise.
pub fn start(
    slots: Vec<SlotWatch>,
    project_dir: PathBuf,
    quiet: Duration,
    tx: Sender<Event>,
) -> Result<(), ConfigError> {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let watcher = notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    })
    .map_err(|e| watch_error(&project_dir, &project_dir, &e))?;
    let mut registry = Registry {
        watcher,
        dirs: HashSet::new(),
        roots: Vec::new(),
    };

    for slot in &slots {
        for target in &slot.targets {
            let result = match target {
                WatchTarget::Dir(root) => {
                    registry.roots.push((root.clone(), slot.ignore.clone()));
                    registry.watch_tree(root, root, &slot.ignore)
                }
                WatchTarget::File { dir, .. } => registry.watch_dir(dir),
            };
            if let Err(e) = result {
                let shown = match target {
                    WatchTarget::Dir(p) => p.clone(),
                    WatchTarget::File { dir, .. } => dir.clone(),
                };
                return Err(watch_error(&project_dir, &shown, &e));
            }
        }
    }

    std::thread::spawn(move || {
        let mut debouncer = Debouncer::new(quiet);
        loop {
            let timeout = debouncer
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(1));
            match raw_rx.recv_timeout(timeout) {
                Ok(Ok(ev)) => {
                    if !is_change(&ev.kind) {
                        continue;
                    }
                    let now = Instant::now();
                    for path in &ev.paths {
                        if matches!(ev.kind, EventKind::Create(_)) && path.is_dir() {
                            registry.on_new_dir(path);
                        }
                        let rel = path.strip_prefix(&project_dir).unwrap_or(path).to_path_buf();
                        for slot in &slots {
                            if slot.matches(path) {
                                debouncer.observe(slot.proc, rel.clone(), now);
                            }
                        }
                    }
                }
                // A backend error for one path is not fatal; the next event
                // for that slot still arrives.
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            for changed in debouncer.due(Instant::now()) {
                if tx.send(Event::Changed(changed)).is_err() {
                    return;
                }
            }
        }
    });
    Ok(())
}

/// A registration failure as a config error: names the path and, for the
/// inotify limit, what to do about it.
fn watch_error(project_dir: &Path, path: &Path, e: &notify::Error) -> ConfigError {
    let shown = path.strip_prefix(project_dir).unwrap_or(path).display();
    let message = match e.kind {
        ErrorKind::MaxFilesWatch => format!(
            "cannot watch {shown:?}: inotify watch limit reached; raise fs.inotify.max_user_watches (sysctl) or narrow the watch paths"
        ),
        _ => format!("cannot watch {shown:?}: {e}"),
    };
    ConfigError {
        path: project_dir.join(crate::config::FILE_NAME),
        line: None,
        message,
    }
}
```

`registry` must live as long as the thread: it is moved into the closure (`registry` is used inside the loop, so it is). If the compiler complains that `registry` is unused in the `File`-only case, it is still moved — fine.

Note on the test `ignored_paths_produce_nothing_and_new_directories_are_picked_up`: creating `src/new` is itself a change under `src` (one `Changed` for `src/new`), which the test drains before writing into it. If on your kernel the write into the new directory is coalesced into that first event (both within 50 ms), the assertion on `c.paths` still holds because the test takes the *second* event after a 200 ms sleep — if it flakes, raise the sleep to 400 ms and note it.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 125 passed, no warnings. Run `cargo test -q watch::` three times in a row to check for flakiness; fix timing (not assertions) if any appears.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/watch.rs
git commit -m "Add the watcher thread with an ignore-aware directory registry"
```

---

### Task 5: Wire watching into launch, events, and the status bar

**Files:**
- Modify: `src/main.rs`, `src/ui.rs`, `README.md`

**Interfaces:**
- Produces: `struct Launch { specs, timeout, debounce, project_dir: Option<PathBuf>, watches: Vec<SlotWatch> }`; `fn resolve_launch(cli: &Cli) -> Result<Launch, Vec<String>>` (replaces `resolve_specs`); `fn run(specs, config, tx, rx)`; `UiState::set_watched(&mut self, watched: Vec<bool>)`; `UiState::slot_label(&self, proc) -> Vec<Span<'static>>`.
- Consumes: `watch::{resolve_all, start, SlotWatch}`, `ProcManager::standard_command`, `Trigger::Watch`.

- [ ] **Step 1: Write the failing tests**

`src/ui.rs` tests:

```rust
    #[test]
    fn watched_slots_show_a_dim_w_after_the_name() {
        let mut s = ui(2);
        s.set_watched(vec![true, false]);
        let first = TuiLine::from(s.slot_label(0));
        let second = TuiLine::from(s.slot_label(1));
        assert_eq!(plain(&first), "[1] p0 w ●");
        assert_eq!(plain(&second), "[2] p1 ●");
        let w = first.spans.iter().find(|sp| sp.content == "w").unwrap();
        assert!(w.style.add_modifier.contains(Modifier::DIM));
    }
```

`src/main.rs` tests — replace `resolve_specs` uses with `resolve_launch` (`.specs`, `.timeout`) and add:

```rust
    #[test]
    fn launch_from_a_file_resolves_watches_and_debounce() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let file = dir.path().join(FILE_NAME);
        fs::write(&file, "[settings]\ndebounce_ms = 20\n[[proc]]\nname = \"a\"\ncmd = \"true\"\nwatch = [\"src\"]\n[[proc]]\nname = \"b\"\ncmd = \"true\"\n").unwrap();
        let launch = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap();
        assert_eq!(launch.debounce, Duration::from_millis(20));
        assert_eq!(launch.watches.len(), 1);
        assert_eq!(launch.watches[0].proc, 0);
        assert_eq!(launch.project_dir, Some(dir.path().canonicalize().unwrap()));

        let adhoc = resolve_launch(&cli(&["true"])).unwrap();
        assert!(adhoc.watches.is_empty());
        assert_eq!(adhoc.debounce, Duration::from_millis(100));
    }

    #[test]
    fn watch_resolution_errors_join_the_config_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        fs::write(&file, "[[proc]]\nname = \"all\"\ncmd = \"npm x\"\nwatch = \"self\"\n").unwrap();
        let errs = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("is reserved"), "parse errors come first and stop resolution: {errs:#?}");

        fs::write(&file, "[[proc]]\nname = \"web\"\ncmd = \"npm x\"\nwatch = \"self\"\n").unwrap();
        let errs = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap_err();
        assert!(errs[0].contains("$PATH"), "{errs:#?}");
    }

    #[test]
    fn a_change_restarts_the_slot_unless_a_restart_is_in_flight() {
        let (tx, rx) = mpsc::channel();
        let config = Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        };
        let mut manager = ProcManager::spawn_all(&["sleep 30".to_string()], &config, tx.clone());
        let mut buffers = BufferSet::new(1, &config);
        let mut ui = UiState::new(vec!["sleep".to_string()]);
        let changed = || Event::Changed(Changed { proc: 0, paths: vec!["src/a.rs".into()], more: 0 });

        tx.send(changed()).unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        assert!(manager.is_restarting(0));
        assert_eq!(ui.health(0), Health::Restarting);

        // In flight: a second change is dropped, not queued.
        tx.send(changed()).unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut transitions = Vec::new();
        while transitions.is_empty() && Instant::now() < deadline {
            transitions = manager.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = transitions.pop().expect("one restart");
        assert_eq!(t.trigger, Trigger::Watch { paths: vec!["src/a.rs".into()], more: 0 });
        assert_eq!(t.new.command, "sleep 30");
        assert!(!manager.is_restarting(0), "the dropped change did not queue another");
        manager.shutdown();
    }
```

Import `Changed` in the test module. Change the three existing `drain_events(&rx, …, &manager)` calls to `&mut manager`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q watched_slots && cargo test -q launch_from`
Expected: compile errors — no `set_watched`/`slot_label`; no `resolve_launch`; `drain_events` takes `&ProcManager`.

- [ ] **Step 3: Implement**

`src/ui.rs`:

- `UiState` gains `watched: Vec<bool>` (doc: `/// Which slots restart on file changes; shown as a dim \`w\`.`), initialised to `vec![false; proc_count]` in `new`.
- ```rust
      /// Mark which slots are watched for changes.
      pub fn set_watched(&mut self, watched: Vec<bool>) {
          self.watched = watched;
      }

      /// The status-bar spans for one slot: index, name, optional `w`, health.
      /// Factored out of the bar so it can be tested as plain text.
      pub fn slot_label(&self, p: ProcId) -> Vec<Span<'static>> {
          let mut spans = Vec::new();
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
              spans.push(Span::styled(name.clone(), Style::default().fg(proc_color(p))));
              spans.push(Span::raw(" "));
          }
          if self.watched.get(p).copied().unwrap_or(false) {
              spans.push(Span::styled("w", Style::default().add_modifier(Modifier::DIM)));
              spans.push(Span::raw(" "));
          }
          spans.push(Span::styled(glyph, gstyle));
          spans
      }
  ```
- `render_status_bar`'s per-slot loop body becomes `spans.extend(self.slot_label(p));` (keep the `"  "` separator between slots).

`src/main.rs`:

```rust
/// Everything `main` decides before the terminal is touched.
struct Launch {
    specs: Vec<ProcSpec>,
    timeout: Option<Duration>,
    /// Quiet period for watch-triggered restarts; 100 ms unless the file
    /// says otherwise.
    debounce: Duration,
    /// The Krawattefile's directory; `None` in ad-hoc mode.
    project_dir: Option<PathBuf>,
    watches: Vec<SlotWatch>,
}

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(100);

fn resolve_launch(cli: &Cli) -> Result<Launch, Vec<String>> {
    if !cli.commands.is_empty() {
        let specs = cli.commands.iter().map(|c| ProcSpec::adhoc(c)).collect();
        return Ok(Launch {
            specs,
            timeout: None,
            debounce: DEFAULT_DEBOUNCE,
            project_dir: None,
            watches: Vec::new(),
        });
    }
    let path = /* unchanged discovery code */;
    let as_messages = |errors: Vec<config::ConfigError>| errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    let file = config::load(&path).map_err(as_messages)?;
    let watches = watch::resolve_all(&file).map_err(as_messages)?;
    Ok(Launch {
        watches,
        debounce: file.debounce.unwrap_or(DEFAULT_DEBOUNCE),
        project_dir: Some(file.project_dir),
        specs: file.procs,
        timeout: file.timeout,
    })
}
```

`main`:

```rust
    let launch = match resolve_launch(&cli) { Ok(l) => l, Err(messages) => { …exit(2) } };
    let config = Config { grace_period: grace_period(cli.timeout, launch.timeout), ..Config::default() };
    let (tx, rx) = mpsc::channel::<Event>();
    // Watches are registered before anything is spawned or drawn, so a
    // registration failure is a clean exit 2 and the build slot's first
    // output is already observed.
    if let (Some(project_dir), false) = (&launch.project_dir, launch.watches.is_empty())
        && let Err(e) = watch::start(launch.watches.clone(), project_dir.clone(), launch.debounce, tx.clone())
    {
        eprintln!("krawatte: {e}");
        std::process::exit(2);
    }
    let watched: Vec<bool> = (0..launch.specs.len()).map(|p| launch.watches.iter().any(|w| w.proc == p)).collect();
    match run(&launch.specs, &watched, &config, tx, rx) { … }
```

`run(specs, watched: &[bool], config, tx, rx)`: no longer creates the channel; after `UiState::new(names.clone())` call `ui.set_watched(watched.to_vec())`.

`drain_events` takes `manager: &mut ProcManager` and gains:

```rust
            Event::Changed(changed) => {
                // Mid-restart the new generation has not spawned yet and will
                // read the disk as it is then, so a further change adds nothing.
                if manager.is_restarting(changed.proc) {
                    continue;
                }
                let standard = manager.standard_command(changed.proc).to_string();
                let trigger = Trigger::Watch {
                    paths: changed.paths,
                    more: changed.more,
                };
                if manager.replace(changed.proc, standard, trigger) {
                    ui.set_health(changed.proc, Health::Restarting);
                }
            }
```

Update the doc comment of `drain_events` with one sentence about `Changed`. Remove the `// wired in by a later task` allows in `watch.rs` and on `standard_command`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 129 passed, no warnings.

- [ ] **Step 5: Document**

`README.md` — in the Krawattefile section, replace the last paragraph (the one deferring `watch`/`ignore`) with:

```markdown
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
```

Update the Keys/Behavior text where it says marker blocks record "the command" to also mention the trigger.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/main.rs src/ui.rs README.md
git commit -m "Restart watched slots on file changes"
```

---

### Task 6: End-to-end smoke and the erhebimus switch-over

**Files:**
- Create: `/home/christoph/Projects/erhebimus/Krawattefile` (outside this repo; **do not commit there** — leave it for the owner to review)
- Modify: `/home/christoph/Projects/erhebimus/Makefile` (`dev-backend`, `dev-payment`; same: leave uncommitted)

No code changes in krawatte; this task is verification. Nothing to commit in krawatte unless a bug is found (then: test first, fix, commit, and report it).

- [ ] **Step 1: Two-stage smoke in a temp project**

```bash
cargo build --release -q
d=$(mktemp -d); mkdir -p "$d/src" "$d/bin"
cat > "$d/Krawattefile" <<'EOF'
[settings]
timeout = 1
debounce_ms = 50

[[proc]]
name  = "build"
cmd   = "sleep 0.3; cp src/app.sh bin/app.tmp && chmod +x bin/app.tmp && mv bin/app.tmp bin/app; echo built"
watch = ["src"]

[[proc]]
name  = "server"
cmd   = "bin/app"
watch = "self"
EOF
printf '#!/bin/sh\necho serving v1; sleep 100\n' > "$d/src/app.sh"
```

Drive with a pty helper (Python `pty`): start `krawatte` in `$d`; expect `server` to show `✖ spawn` or `✖ exit 127`-ish at first (no `bin/app` yet), `build` to print `built` and exit 0, then `server` to restart via `watch` with `changed: bin/app` and print `serving v1`. Then `printf '#!/bin/sh\necho serving v2; sleep 100\n' > "$d/src/app.sh"`: `build` restarts (`changed: src/app.sh`), exits 0, `server` restarts and prints `serving v2`. Then write a broken build (`cmd` exits 1 — e.g. edit `src/app.sh` to be empty and have build `test -s src/app.sh &&` guard) and confirm `server` keeps running v2. Status bar shows `build w` and `server w`. `q` exits cleanly. Record the transcript.

- [ ] **Step 2: erhebimus**

Read `/home/christoph/Projects/erhebimus/Makefile` (variables `SERVER_DIR`, `PAYMENT_DIR`, `ERHEBIMUS_ADMIN_SOCKET_PATH`, and the `dev-backend`/`dev-payment` recipes). Write `/home/christoph/Projects/erhebimus/Krawattefile` expressing the two `watchexec` targets as two-stage slots:

```toml
[[proc]]
name  = "build"
cmd   = "cargo build -p erhebimus --bin erhebimus"
watch = ["<SERVER_DIR>/src", "<SERVER_DIR>/migrations"]

[[proc]]
name  = "server"
cmd   = "mkdir -p \"$(dirname \"$ERHEBIMUS_ADMIN_SOCKET_PATH\")\" 2>/dev/null; exec target/debug/erhebimus"
watch = ["target/debug/erhebimus"]

[[proc]]
name  = "build-payment"
cmd   = "cargo build -p erhebimus-payment"
watch = ["<PAYMENT_DIR>/src", "<PAYMENT_DIR>/config.dev.toml"]

[[proc]]
name  = "payment"
cmd   = "target/debug/erhebimus-payment --config <PAYMENT_DIR>/config.dev.toml"
watch = "self"
```

with the placeholders replaced by the Makefile's literal values (note `server` uses the explicit file path rather than `"self"` because its first token is `mkdir`; the `ERHEBIMUS_ADMIN_SOCKET_PATH` default comes from the Makefile's `DEV_ADMIN_SOCKET_PATH` — put its value into `env = { ERHEBIMUS_ADMIN_SOCKET_PATH = "…" }` on the `server` slot if it is a literal in the Makefile; if it is computed, keep the `make` variable in the Makefile and pass it via `env` there is not possible, so document it in the Makefile comment instead). Then change the Makefile:

```make
dev-backend:
	krawatte   # Krawattefile: build + server slots replace watchexec + cargo run
```

Actually keep the recipe list-free: replace the two recipes' bodies with `krawatte -f Krawattefile` and delete the two `watchexec` lines; leave `dev`, `dev-payment-tunnel`, `dev-website` untouched (they still reference the targets). Do **not** commit in erhebimus; report `git -C ~/Projects/erhebimus diff` and the new file's content so the owner can review.

- [ ] **Step 3: Run it once**

From `~/Projects/erhebimus`, run `krawatte` (release binary) via the pty helper for ~20 s or until `build` has exited once, then `q`. It is acceptable for the build to take longer than the window or to fail for environmental reasons (missing toolchain, database); what must hold: the Krawattefile loads without config errors, both `w` markers appear, and `q` shuts down. Report exactly what happened.

---

## Self-review

**Spec coverage.**
- Bare `"self"` keyword vs array paths → already in B; `self` path-like check → Task 2 (`self_requires_a_path_like_first_token`).
- Dir recursive; file via parent dir filtered by name; absent file allowed, absent parent error → Task 2 (`directories_files_and_absent_files_resolve`, `missing_parent…`), Task 4 (rename test).
- Relative to slot cwd → Task 2 (`entries_resolve_against_the_slots_cwd`).
- Change kinds (no access/metadata) → Task 4 `is_change`.
- Ignore defaults + per-slot, component matching, not descending → Task 2 (`ignored`), Task 4 (`Registry::watch_tree`, test with `node_modules`).
- Debounce quiet period, `settings.debounce_ms` default 100 → Tasks 2, 3, 5.
- Rules 1 and 3 (in-flight drop, replace standard) → Task 5 (`a_change_restarts_the_slot_unless_a_restart_is_in_flight`); rule 2 deferred to C per spec.
- Dead slot revived by change → follows from `replace` on a dead slot (spec A), exercised by the smoke (server starts once the binary appears).
- Marker trigger + `changed:` line, ≤3 paths, `(+N more)`, key triggers shown → Task 1 (tests), `SHOWN_PATHS` in Task 3.
- Dim `w` in the status bar → Task 5.
- Two-stage cargo flow end to end → Task 6.
- ENOSPC message, fail fast, never half-watched → Task 4 (`watch_error`, `start_fails_fast…`), Task 5 (start before spawn/draw).
- Deleted root not re-registered; restart storms bounded by in-flight rule → documented, no task (matches spec's out-of-scope).
- README and erhebimus switch-over → Tasks 5, 6.

**Placeholder scan.** Task 6 Step 2 uses `<SERVER_DIR>` placeholders deliberately: the values are in the other repo's Makefile and the step says to substitute them. Nothing else.

**Type consistency.** `Trigger::Watch { paths: Vec<PathBuf>, more: usize }` (Task 1) matches `Changed { proc, paths, more }` (Task 2) and the conversion in Task 5. `SlotWatch { proc, targets, ignore: GlobSet }` consistent in Tasks 2, 4, 5. `start(Vec<SlotWatch>, PathBuf, Duration, Sender<Event>) -> Result<(), ConfigError>` in Tasks 4, 5. `Debouncer::{new, observe, next_deadline, due}` in Tasks 3, 4. `replace(proc, String, Trigger)` / `kill(proc, Trigger)` in Tasks 1, 5. Test counts: 108 → 110 → 118 → 121 → 125 → 129.
