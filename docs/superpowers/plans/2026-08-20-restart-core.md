# Restart Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Red–green TDD is mandatory** (see Global Constraints): test first, watch it fail, then implement. Use superpowers:test-driven-development for every task.

**Goal:** A slot can be torn down and respawned in place (`r`/`k` hotkeys) without blocking the UI, keeping its index, name and buffer, and writing a marker block into the buffer describing the transition.

**Architecture:** `Proc` is split into the slot (standard command, generation counter, policy) and a `Generation` (one spawn: pgid, waiter, status). A restart is an in-flight single-slot `ShutdownMachine` — the existing TERM→grace→KILL machine, unchanged — stepped from the 50 ms main loop via `ProcManager::tick()`, which spawns the next generation once the old group is gone and returns a `Transition` record. Events carry the generation they came from so the main loop can drop stale ones.

**Tech Stack:** Rust 2024, `nix` (killpg/setpgid), `ratatui`/`crossterm`, `jiff`. Tests are plain `#[test]`s; process tests use real `sh` children as the existing `proc.rs` tests do.

**Spec:** `docs/superpowers/specs/2026-08-20-restart-core-design.md`. One naming deviation: the spec calls `tick`'s return type `Respawned`; this plan names it `Transition` (old generation → new generation) so spec C can extend it with a trigger field without the name reading oddly.

## Global Constraints

- **Red–green TDD is mandatory.** For every behavior change: write the test
  first, run it and *observe it fail for the expected reason* (red), write
  the minimal code that makes it pass, run it and observe it pass (green),
  then refactor with the suite green. Never write implementation code before
  the red run has been seen; a test that passes on first run is a test that
  proves nothing — fix it until it fails without the implementation. Each
  task below is laid out in that order; do not reorder steps or batch
  several tasks' implementation ahead of their tests. The only exception is
  Task 2, a pure refactor: it adds no behavior and is covered by keeping the
  existing suite green after every edit.
- Linux/Unix only; process groups and POSIX signals via `nix`.
- `cargo test` and `cargo clippy --all-targets` are clean at the start (69 tests) and must stay clean after every task.
- Shutdown and restart must stay bounded: nothing may ever block on pipe EOF or on a process that survived SIGKILL (see the `waiter` comment in `proc.rs`).
- No crash-restart: a generation that exits on its own stays dead.
- `r`/`k` are silent no-ops in the all-view and while a restart is in flight for that slot.
- The buffer is never cleared by a restart.
- Commit after every task; commit messages in the imperative, as in `git log`.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `src/types.rs` | Adds `Gen`, `StreamTag::Marker`, `Health::Restarting`; `gen` field on every `Event` variant. |
| `src/proc.rs` | `Generation` / `Proc` split; `Restart`, `Transition`, `OldGen`, `NewGen`, `Outcome`; `replace`, `kill`, `tick`, `is_current`, `is_restarting`, `current_command`, `current_gen`, `next_seq`. Shutdown machine untouched. |
| `src/marker.rs` (new) | Pure text formatting of the restart marker block from a `Transition`. |
| `src/buffer.rs` | `StyledLine::marker` constructor. |
| `src/ui.rs` | `r`/`k` key mapping, `Action::Restart`/`Kill`, `↻` glyph, dim rendering of `Marker` lines, `clock()` and `health()` accessors. |
| `src/main.rs` | Wires actions to the manager, filters stale events, applies transitions (marker block + health). |
| `README.md` | Documents the keys and restart behavior. |

---

### Task 1: Generation-tagged events and shared types

**Files:**
- Modify: `src/types.rs`
- Modify: `src/proc.rs` (`spawn_one`, `spawn_reader`, `emit`, `spawn_all_with_shell`, test `line_events_carry_increasing_seq`)
- Modify: `src/main.rs` (`drain_events`)

**Interfaces:**
- Produces: `pub type Gen = u32;` `StreamTag::Marker`, `Health::Restarting`, `Event::{Line, Exited, SpawnFailed}` each with a `gen: Gen` field placed directly after `proc`.

- [ ] **Step 1: Extend the existing seq test to also assert the generation**

In `src/proc.rs`, in `line_events_carry_increasing_seq`, replace the event-collecting loop with:

```rust
        let mut seqs: Vec<Seq> = Vec::new();
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut gens: Vec<Gen> = Vec::new();
        for e in rx.try_iter() {
            if let Event::Line {
                seq, bytes, gen, ..
            } = e
            {
                seqs.push(seq);
                lines.push(bytes);
                gens.push(gen);
            }
        }
        assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        // The initial spawn is generation 0; restarts count up from there.
        assert!(gens.iter().all(|&g| g == 0));
```

- [ ] **Step 2: Run it to verify it fails to compile**

Run: `cargo test -q line_events_carry_increasing_seq`
Expected: compile error — `Gen` not found / no field `gen` on `Event::Line`.

- [ ] **Step 3: Add the types**

In `src/types.rs`, after the `Seq` alias:

```rust
/// Generation counter of a slot: `0` for the initial spawn, incremented every
/// time the slot is respawned. Events carry the generation they came from so
/// the UI can drop output from a generation that has since been replaced.
pub type Gen = u32;
```

Extend `StreamTag`:

```rust
pub enum StreamTag {
    Stdout,
    Stderr,
    /// A line krawatte inserted into a slot's buffer itself (a restart marker),
    /// not process output. Rendered dim, without the stderr marker.
    Marker,
}
```

Extend `Health` (after `Running`):

```rust
    /// The current generation is being torn down ahead of a respawn.
    Restarting,
```

Extend every `Event` variant with `gen: Gen` immediately after `proc`:

```rust
    Line {
        proc: ProcId,
        gen: Gen,
        stream: StreamTag,
        seq: Seq,
        at: SystemTime,
        bytes: Vec<u8>,
    },
    /// A child process exited and was reaped.
    Exited {
        proc: ProcId,
        gen: Gen,
        status: ExitStatus,
    },
    /// A command failed to spawn.
    SpawnFailed {
        proc: ProcId,
        gen: Gen,
        #[allow(dead_code)]
        error: String,
    },
```

- [ ] **Step 4: Thread `gen` through the spawn path**

In `src/proc.rs`, add `Gen` to the `crate::types` import:

```rust
use crate::types::{Config, Event, ExitStatus, Gen, ProcId, Seq, StreamTag};
```

Change `spawn_one`'s signature and body so the generation is captured by the reader and waiter threads:

```rust
fn spawn_one(
    proc: ProcId,
    gen: Gen,
    shell: &str,
    command: &str,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
) -> std::io::Result<SpawnParts> {
```

…and inside it:

```rust
    spawn_reader(proc, gen, StreamTag::Stdout, stdout, seq.clone(), tx.clone());
    spawn_reader(proc, gen, StreamTag::Stderr, stderr, seq.clone(), tx.clone());
```

```rust
        let _ = waiter_tx.send(Event::Exited {
            proc,
            gen,
            status: st,
        });
```

`spawn_reader` and `emit` gain a `gen: Gen` parameter right after `proc`, passed straight through; `emit` sends `Event::Line { proc, gen, stream, seq: s, at, bytes }`.

In `spawn_all_with_shell`, call `spawn_one(proc, 0, shell, command, &seq, &tx)` and send `Event::SpawnFailed { proc, gen: 0, error: err.to_string() }`.

In `src/main.rs` `drain_events`, ignore the new field for now (Task 7 uses it):

```rust
            Event::Line {
                proc,
                gen: _,
                stream,
                seq,
                at,
                bytes,
            } => {
```

```rust
            Event::Exited { proc, status, .. } => {
```

(`SpawnFailed { proc, .. }` already ignores the rest.)

- [ ] **Step 5: Run the whole suite and clippy**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 69 passed; clippy may warn that `StreamTag::Marker` and `Health::Restarting` are never constructed — add `#[allow(dead_code)]` on those two variants for now and remove the attributes in Tasks 5 and 6.

- [ ] **Step 6: Commit**

```bash
git add src/types.rs src/proc.rs src/main.rs
git commit -m "Tag events with the generation they came from"
```

---

### Task 2: Split `Proc` into slot and `Generation` (pure refactor)

**Files:**
- Modify: `src/proc.rs` (`Proc`, `ProcManager`, `spawn_all_with_shell`, `all_dead`, `shutdown`, `was_started`, `Drop`, `spawn_one`, `RealEffects`, test `genuine_spawn_failure_reports_dead_slot`)

**Interfaces:**
- Produces: `struct Generation { gen, command, pid: i32, pgid, started: Instant, dead, status, waiter, finished }`, `struct Proc { standard, short, gen, live: Option<Generation> }`, `ProcManager { procs, grace_period, shell: String, seq: Arc<AtomicU64>, tx: Sender<Event> }`, `fn spawn_one(proc, gen, shell, command, seq, tx) -> io::Result<Generation>`.
- Consumes: Task 1's `Gen` and event fields.

No new tests: this task changes no behavior, so the existing suite is the test. One test touches a renamed field.

- [ ] **Step 1: Replace the `Proc` struct**

In `src/proc.rs`, replace the whole `struct Proc { … }` definition and its doc comment with:

```rust
/// One spawn of a slot's command. A slot holds at most one generation that may
/// still be alive; a restart tears it down and replaces it.
struct Generation {
    /// Which generation of its slot this is; `0` for the initial spawn.
    gen: Gen,
    /// The command this generation runs (as passed to `sh -c`).
    command: String,
    /// Pid of the `sh` that leads this generation's process group.
    pid: i32,
    /// Process-group id used for signalling (equal to `pid`, since the child
    /// leads its own group).
    pgid: Pid,
    /// When the generation was spawned, for the runtime reported on restart.
    started: Instant,
    /// Set to `true` by the waiter thread once the child has been reaped.
    dead: Arc<AtomicBool>,
    /// Final exit status, filled in by the waiter thread. `None` while running.
    status: Arc<Mutex<Option<ExitStatus>>>,
    /// Join handle for the waiter thread. Safe to join once `dead` is set;
    /// `None` once joined.
    ///
    /// The two reader threads deliberately have no handles here: they block in
    /// `read()` until *every* holder of the pipe's write end closes it, and that
    /// can include processes which outlived the child or escaped its process
    /// group entirely (a `setsid` daemon, say). Shutdown must never wait on an
    /// event krawatte cannot force, so readers are detached and left to be
    /// cleaned up by process exit.
    waiter: Option<JoinHandle<()>>,
    /// Set once this generation's process group has been confirmed empty, so
    /// nothing signals the pgid again (by then the kernel may have recycled it
    /// for an unrelated process).
    finished: bool,
}

/// Per-slot state: the configured command and the current generation.
struct Proc {
    /// The slot's configured command (the CLI argument).
    standard: String,
    /// Precomputed short display name for the status bar.
    short: String,
    /// Number of the most recent generation; `0` for the initial spawn.
    gen: Gen,
    /// The most recent generation, or `None` if the slot has never spawned
    /// successfully.
    live: Option<Generation>,
}
```

- [ ] **Step 2: Give the manager what it needs to respawn**

```rust
/// Manages the full set of child processes and the shared event channel.
pub struct ProcManager {
    procs: Vec<Proc>,
    grace_period: Duration,
    /// Shell every command is run through (`sh` outside tests).
    shell: String,
    /// One shared, monotonically increasing sequence counter across every
    /// process and both streams, so the all-view can reconstruct arrival order.
    seq: Arc<AtomicU64>,
    tx: Sender<Event>,
}
```

Replace the body of `spawn_all_with_shell`:

```rust
    fn spawn_all_with_shell(
        commands: &[String],
        config: &Config,
        tx: Sender<Event>,
        shell: &str,
    ) -> ProcManager {
        let mut mgr = ProcManager {
            procs: Vec::with_capacity(commands.len()),
            grace_period: config.grace_period,
            shell: shell.to_string(),
            seq: Arc::new(AtomicU64::new(0)),
            tx,
        };
        for (proc, command) in commands.iter().enumerate() {
            let live = match spawn_one(proc, 0, &mgr.shell, command, &mgr.seq, &mgr.tx) {
                Ok(generation) => Some(generation),
                Err(err) => {
                    // Spawn failure: report it and record a slot with no generation.
                    let _ = mgr.tx.send(Event::SpawnFailed {
                        proc,
                        gen: 0,
                        error: err.to_string(),
                    });
                    None
                }
            };
            mgr.procs.push(Proc {
                standard: command.clone(),
                short: short_name_of(command),
                gen: 0,
                live,
            });
        }
        mgr
    }
```

- [ ] **Step 3: Make `spawn_one` return a `Generation`**

Delete the `SpawnParts` type alias and its doc comment. New signature and tail of `spawn_one`:

```rust
/// Spawn a single child in its own process group, wiring reader threads for
/// stdout/stderr and a waiter thread that reaps and reports the exit.
fn spawn_one(
    proc: ProcId,
    gen: Gen,
    shell: &str,
    command: &str,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
) -> std::io::Result<Generation> {
```

Replace the final `Ok((pgid, dead, status, waiter))` with:

```rust
    Ok(Generation {
        gen,
        command: command.to_string(),
        pid,
        pgid,
        started: Instant::now(),
        dead,
        status,
        waiter: Some(waiter),
        finished: false,
    })
```

- [ ] **Step 4: Update every reader of the old fields**

`all_dead`:

```rust
    pub fn all_dead(&self) -> bool {
        self.procs
            .iter()
            .all(|p| p.live.as_ref().is_none_or(|g| g.dead.load(Ordering::SeqCst)))
    }
```

`shutdown` — the candidate list, the post-machine bookkeeping and the status collection:

```rust
        let live: Vec<ProcId> = self
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.live.is_some())
            .map(|(i, _)| i)
            .collect();
```

```rust
        for g in self.procs.iter_mut().filter_map(|p| p.live.as_mut()) {
            g.finished = g.dead.load(Ordering::SeqCst) && group_gone(g.pgid);
            if g.dead.load(Ordering::SeqCst)
                && let Some(h) = g.waiter.take()
            {
                let _ = h.join();
            }
        }
        self.procs
            .iter()
            .map(|p| p.live.as_ref().and_then(|g| *g.status.lock().unwrap()))
            .collect()
```

`was_started`:

```rust
    pub fn was_started(&self, proc: ProcId) -> bool {
        self.procs[proc].live.is_some()
    }
```

`Drop`:

```rust
    fn drop(&mut self) {
        for g in self.procs.iter().filter_map(|p| p.live.as_ref()) {
            if !g.finished {
                let _ = killpg(g.pgid, Signal::SIGKILL);
            }
        }
        for g in self.procs.iter_mut().filter_map(|p| p.live.as_mut()) {
            if g.dead.load(Ordering::SeqCst)
                && let Some(h) = g.waiter.take()
            {
                let _ = h.join();
            }
        }
    }
```

`RealEffects`:

```rust
impl ShutdownEffects for RealEffects<'_> {
    fn term(&mut self, proc: ProcId) {
        if let Some(g) = &self.mgr.procs[proc].live {
            let _ = killpg(g.pgid, Signal::SIGTERM);
        }
    }

    fn kill(&mut self, proc: ProcId) {
        if let Some(g) = &self.mgr.procs[proc].live {
            let _ = killpg(g.pgid, Signal::SIGKILL);
        }
    }

    /// A slot counts as exited only once its direct child has been reaped *and*
    /// its process group is empty. Testing `dead` alone would declare shutdown
    /// complete while background jobs left in the group were still running.
    fn poll_exited(&mut self) -> Vec<ProcId> {
        self.mgr
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.live
                    .as_ref()
                    .is_none_or(|g| g.dead.load(Ordering::SeqCst) && group_gone(g.pgid))
            })
            .map(|(i, _)| i)
            .collect()
    }
```

(`now` and `sleep` unchanged.)

In the test `genuine_spawn_failure_reports_dead_slot`, replace `assert!(mgr.procs[0].pgid.is_none());` with `assert!(mgr.procs[0].live.is_none());`.

- [ ] **Step 5: Run the suite and clippy**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 69 passed, no warnings. (`Generation::command`, `pid`, `started` and `Proc::standard`, `gen` are read only in Task 3 — if clippy flags them as never read, add `#[allow(dead_code)]` to the struct field and remove it in Task 3.)

- [ ] **Step 6: Commit**

```bash
git add src/proc.rs
git commit -m "Split Proc into slot and Generation"
```

---

### Task 3: The restart primitive: `replace` and `tick`

**Files:**
- Modify: `src/proc.rs` (new types after `Proc`; new methods on `ProcManager`; `shutdown`; tests)

**Interfaces:**
- Produces:
  ```rust
  pub enum Outcome { Exited(ExitStatus), Abandoned }
  pub struct OldGen { pub gen: Gen, pub pid: i32, pub outcome: Outcome, pub ran: Duration }
  pub struct NewGen { pub gen: Gen, pub command: String, pub spawn: Result<i32, String> }
  pub struct Transition { pub proc: ProcId, pub old: Option<OldGen>, pub new: NewGen }
  impl ProcManager {
      pub fn replace(&mut self, proc: ProcId, command: String) -> bool;
      pub fn tick(&mut self) -> Vec<Transition>;
      pub fn current_gen(&self, proc: ProcId) -> Gen;
      pub fn is_current(&self, proc: ProcId, gen: Gen) -> bool;
      pub fn is_restarting(&self, proc: ProcId) -> bool;
      pub fn current_command(&self, proc: ProcId) -> &str;
      pub fn next_seq(&self) -> Seq;
  }
  ```
- Consumes: Task 2's `Generation`/`Proc`, existing `ShutdownMachine`, `RealEffects`, `group_gone`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/proc.rs`, after `read_pid_line`:

```rust
    /// Drive `tick` until some slot reports a transition (bounded).
    fn tick_until_transition(mgr: &mut ProcManager, limit: Duration) -> Transition {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(t) = mgr.tick().pop() {
                return t;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no transition within {limit:?}");
    }

    fn short_grace() -> Config {
        Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        }
    }

    #[test]
    fn restart_of_live_slot_spawns_a_new_pid_in_the_same_slot() {
        let (tx, rx) = mpsc::channel();
        let cmd = "echo $$; sleep 30".to_string();
        let mut mgr = ProcManager::spawn_all(std::slice::from_ref(&cmd), &short_grace(), tx);
        let old_pid = read_pid_line(&rx);

        assert!(mgr.replace(0, cmd.clone()));
        assert!(mgr.is_restarting(0));
        // A second request while one is in flight is ignored.
        assert!(!mgr.replace(0, cmd.clone()));

        let t = tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(t.proc, 0);
        let old = t.old.expect("slot had a live generation");
        assert_eq!(old.gen, 0);
        assert_eq!(old.pid, old_pid.as_raw());
        // `sh` has the default TERM disposition, so the grace period never
        // expires and the machine never escalates to KILL.
        assert_eq!(old.outcome, Outcome::Exited(ExitStatus::Signal(15)));
        assert_eq!(t.new.gen, 1);
        assert_eq!(t.new.command, cmd);
        let new_pid = t.new.spawn.expect("respawn succeeded");
        assert_ne!(new_pid, old_pid.as_raw());

        assert!(group_gone(old_pid));
        assert!(!mgr.is_restarting(0));
        assert_eq!(mgr.current_gen(0), 1);
        assert!(mgr.is_current(0, 1));
        assert!(!mgr.is_current(0, 0));
        assert_eq!(mgr.current_command(0), cmd);
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn restart_of_dead_slot_spawns_without_waiting_out_the_grace() {
        let (tx, _rx) = mpsc::channel();
        let cfg = Config {
            grace_period: Duration::from_secs(30),
            ..Config::default()
        };
        let mut mgr = ProcManager::spawn_all(&["exit 3".to_string()], &cfg, tx);
        wait_until_dead(&mgr);

        let started = Instant::now();
        assert!(mgr.replace(0, "exit 4".to_string()));
        let t = tick_until_transition(&mut mgr, Duration::from_secs(2));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(t.old.unwrap().outcome, Outcome::Exited(ExitStatus::Code(3)));
        assert_eq!(t.new.command, "exit 4");

        wait_until_dead(&mgr);
        assert_eq!(mgr.shutdown(), vec![Some(ExitStatus::Code(4))]);
    }

    #[test]
    fn restart_of_never_started_slot_reports_no_old_generation() {
        let (tx, _rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all_with_shell(
            &["whatever".to_string()],
            &Config::default(),
            tx,
            "/nonexistent/krawatte-no-such-shell",
        );
        assert!(mgr.replace(0, "whatever".to_string()));
        let t = tick_until_transition(&mut mgr, Duration::from_secs(2));
        assert!(t.old.is_none());
        assert_eq!(t.new.gen, 1);
        // The shell is still missing, so the respawn fails too and says why.
        assert!(t.new.spawn.is_err());
        assert_eq!(mgr.current_gen(0), 1);
        assert!(mgr.all_dead());
    }

    #[test]
    fn restart_kills_background_jobs_left_in_the_old_group() {
        let (tx, rx) = mpsc::channel();
        let cmd = "sleep 30 & echo $!".to_string();
        let mut mgr = ProcManager::spawn_all(std::slice::from_ref(&cmd), &short_grace(), tx);
        wait_until_dead(&mgr);
        let bg = read_pid_line(&rx);
        assert!(nix::sys::signal::kill(bg, None).is_ok());

        assert!(mgr.replace(0, "true".to_string()));
        tick_until_transition(&mut mgr, Duration::from_secs(5));

        let deadline = Instant::now() + Duration::from_secs(3);
        while nix::sys::signal::kill(bg, None).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            nix::sys::signal::kill(bg, None).is_err(),
            "background job {bg} survived the restart"
        );
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn shutdown_started_mid_restart_returns_within_the_bound() {
        let (tx, _rx) = mpsc::channel();
        let cmd = "trap '' TERM; sleep 30".to_string();
        let mut mgr = ProcManager::spawn_all(std::slice::from_ref(&cmd), &short_grace(), tx);
        // Give `sh` a moment to install the trap before TERM arrives.
        std::thread::sleep(Duration::from_millis(100));
        assert!(mgr.replace(0, cmd));
        // One step: TERM has been sent, the grace clock is running.
        assert!(mgr.tick().is_empty());
        assert!(mgr.is_restarting(0));
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn next_seq_continues_the_global_sequence() {
        let (tx, rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["printf 'a\\nb\\n'".to_string()], &Config::default(), tx);
        wait_until_dead(&mgr);
        let last = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::Line { seq, .. } => Some(seq),
                _ => None,
            })
            .max()
            .unwrap();
        assert_eq!(mgr.next_seq(), last + 1);
        mgr.shutdown();
    }
```

- [ ] **Step 2: Run them to verify they fail to compile**

Run: `cargo test -q restart_`
Expected: compile errors — `Transition`, `replace`, `tick` not found.

- [ ] **Step 3: Add the types**

In `src/proc.rs`, directly after `struct Proc { … }`:

```rust
/// An in-flight restart: the teardown of the current generation, and what to
/// run once it is gone.
struct Restart {
    /// Single-slot TERM -> grace -> KILL machine; starts `Done` if there was
    /// nothing to tear down.
    machine: ShutdownMachine,
    /// Command to spawn once the old generation is gone.
    next: String,
}

/// How a generation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Reaped with this status.
    Exited(ExitStatus),
    /// Still present after SIGKILL and the reap timeout; given up on so the
    /// restart could finish.
    Abandoned,
}

/// The generation a [`Transition`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldGen {
    pub gen: Gen,
    pub pid: i32,
    pub outcome: Outcome,
    /// How long the generation ran, spawn to teardown.
    pub ran: Duration,
}

/// The generation a [`Transition`] started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGen {
    pub gen: Gen,
    pub command: String,
    /// The new leader's pid, or why it could not be spawned.
    pub spawn: Result<i32, String>,
}

/// A completed slot transition, reported by [`ProcManager::tick`] so the
/// caller can record it in the slot's buffer and update the health display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub proc: ProcId,
    /// `None` when the slot had no generation to end (it never started).
    pub old: Option<OldGen>,
    pub new: NewGen,
}
```

Add one field to `Proc` (after `live`):

```rust
    /// Teardown in progress, if any. A slot has at most one.
    restart: Option<Restart>,
```

and initialise it in `spawn_all_with_shell`: `restart: None,`.

- [ ] **Step 4: Add the manager methods**

In `impl ProcManager`, after `short_name`:

```rust
    /// Tear down the slot's current generation (if any is still around), then
    /// spawn `command` in its place. Non-blocking: the teardown is driven by
    /// [`tick`](Self::tick). Returns `false`, doing nothing, if a restart is
    /// already in flight.
    pub fn replace(&mut self, proc: ProcId, command: String) -> bool {
        let slot = &mut self.procs[proc];
        if slot.restart.is_some() {
            return false;
        }
        // A generation that already exited and whose group is empty needs no
        // teardown. Mark it finished now so nothing signals its pgid -- which
        // the kernel may by now have handed to an unrelated process.
        if let Some(g) = slot.live.as_mut()
            && !g.finished
            && g.dead.load(Ordering::SeqCst)
            && group_gone(g.pgid)
        {
            g.finished = true;
        }
        let to_kill = match &slot.live {
            Some(g) if !g.finished => vec![proc],
            _ => Vec::new(),
        };
        slot.restart = Some(Restart {
            machine: ShutdownMachine::new(to_kill, self.grace_period),
            next: command,
        });
        true
    }

    /// Step every in-flight restart by one poll; spawn the next generation of
    /// each slot whose teardown completed. Returns the completed transitions in
    /// slot order. Call this from the main loop; it never blocks.
    pub fn tick(&mut self) -> Vec<Transition> {
        let mut out = Vec::new();
        for proc in 0..self.procs.len() {
            let Some(mut restart) = self.procs[proc].restart.take() else {
                continue;
            };
            restart.machine.step(&mut RealEffects { mgr: self });
            if restart.machine.phase() != ShutdownPhase::Done {
                self.procs[proc].restart = Some(restart);
                continue;
            }
            out.push(self.complete(proc, restart));
        }
        out
    }

    /// Retire the slot's old generation and spawn the next one.
    fn complete(&mut self, proc: ProcId, restart: Restart) -> Transition {
        let abandoned = !restart.machine.abandoned().is_empty();
        let old = self.procs[proc].live.take().map(|mut g| {
            let ran = g.started.elapsed();
            // Join only a waiter that has already published `dead`; an
            // abandoned generation's waiter is still blocked in `wait()`.
            if g.dead.load(Ordering::SeqCst)
                && let Some(h) = g.waiter.take()
            {
                let _ = h.join();
            }
            let outcome = if abandoned {
                Outcome::Abandoned
            } else {
                let status = *g.status.lock().unwrap();
                Outcome::Exited(status.unwrap_or(ExitStatus::Code(-1)))
            };
            OldGen {
                gen: g.gen,
                pid: g.pid,
                outcome,
                ran,
            }
        });
        let command = restart.next;
        let gen = self.procs[proc].gen + 1;
        self.procs[proc].gen = gen;
        let spawn = match spawn_one(proc, gen, &self.shell, &command, &self.seq, &self.tx) {
            Ok(g) => {
                let pid = g.pid;
                self.procs[proc].live = Some(g);
                Ok(pid)
            }
            Err(e) => {
                let _ = self.tx.send(Event::SpawnFailed {
                    proc,
                    gen,
                    error: e.to_string(),
                });
                Err(e.to_string())
            }
        };
        let new = NewGen {
            gen,
            command,
            spawn,
        };
        Transition { proc, old, new }
    }

    /// Number of the slot's current generation.
    pub fn current_gen(&self, proc: ProcId) -> Gen {
        self.procs[proc].gen
    }

    /// Whether an event stamped `gen` belongs to the slot's current generation.
    /// Anything older is stale output from a replaced generation (or from a
    /// grandchild that escaped its group and still holds the old pipe).
    pub fn is_current(&self, proc: ProcId, gen: Gen) -> bool {
        self.procs.get(proc).is_some_and(|p| p.gen == gen)
    }

    /// Whether a teardown is in flight for this slot.
    pub fn is_restarting(&self, proc: ProcId) -> bool {
        self.procs.get(proc).is_some_and(|p| p.restart.is_some())
    }

    /// The command the slot's current generation runs; the standard command if
    /// the slot has never spawned.
    pub fn current_command(&self, proc: ProcId) -> &str {
        let p = &self.procs[proc];
        p.live.as_ref().map_or(&p.standard, |g| &g.command)
    }

    /// Hand out the next global sequence number, for lines krawatte inserts
    /// into a buffer itself. Shares the counter the reader threads use, so the
    /// line sorts after everything that arrived before it.
    pub fn next_seq(&self) -> Seq {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
```

`ShutdownMachine::phase` and `abandoned` are currently marked `#[allow(dead_code)]`; remove those two attributes now that they have non-test callers.

At the top of `shutdown`, before computing `live`, abandon any in-flight restart — the global machine takes over signalling the same group:

```rust
        for p in &mut self.procs {
            p.restart = None;
        }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 75 passed, no warnings.

If `restart_of_live_slot_spawns_a_new_pid_in_the_same_slot` reports `Outcome::Exited(Code(143))` instead of `Signal(15)`, the system `sh` is one that reports a terminated foreground child as exit 128+n instead of dying itself; accept either with `assert!(matches!(old.outcome, Outcome::Exited(ExitStatus::Signal(15) | ExitStatus::Code(143))))`.

- [ ] **Step 6: Commit**

```bash
git add src/proc.rs
git commit -m "Add non-blocking per-slot restart primitive"
```

---

### Task 4: `kill` returns a slot to its standard command

**Files:**
- Modify: `src/proc.rs` (one method, one test)

**Interfaces:**
- Produces: `pub fn kill(&mut self, proc: ProcId) -> bool`.
- Consumes: Task 3's `replace`, `tick_until_transition`, `short_grace`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn kill_respawns_the_standard_command() {
        let (tx, _rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["sleep 30".to_string()], &short_grace(), tx);
        // Run something else in the slot, as a future override would.
        assert!(mgr.replace(0, "sleep 31".to_string()));
        tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(mgr.current_command(0), "sleep 31");

        assert!(mgr.kill(0));
        assert!(!mgr.kill(0), "kill while in flight is ignored");
        let t = tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(t.old.unwrap().gen, 1);
        assert_eq!(t.new.gen, 2);
        assert_eq!(t.new.command, "sleep 30");
        assert_eq!(mgr.current_command(0), "sleep 30");
        shutdown_within(mgr, Duration::from_secs(5));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -q kill_respawns`
Expected: compile error — no method `kill`.

- [ ] **Step 3: Implement `kill`**

After `replace` in `impl ProcManager`:

```rust
    /// Tear down the slot's current generation and spawn the slot's standard
    /// command in its place. Today every generation *is* the standard command,
    /// so this equals [`replace`](Self::replace) with the current command; it
    /// diverges once an override can run in a slot. Returns `false`, doing
    /// nothing, if a restart is already in flight.
    pub fn kill(&mut self, proc: ProcId) -> bool {
        let standard = self.procs[proc].standard.clone();
        self.replace(proc, standard)
    }
```

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 76 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/proc.rs
git commit -m "Add kill to ProcManager"
```

---

### Task 5: Marker block text and rendering

**Files:**
- Create: `src/marker.rs`
- Modify: `src/main.rs` (add `mod marker;`)
- Modify: `src/buffer.rs` (`StyledLine::marker`)
- Modify: `src/ui.rs` (`tagged_line`, `UiState::clock`, tests)

**Interfaces:**
- Produces: `marker::restart_block(t: &Transition, clock: &str) -> Vec<String>`, `StyledLine::marker(proc, seq, at, text: String) -> StyledLine`, `UiState::clock(&self, at: SystemTime) -> String`.
- Consumes: Task 3's `Transition`, `OldGen`, `NewGen`, `Outcome`; Task 1's `StreamTag::Marker`.

- [ ] **Step 1: Write the failing marker tests**

Create `src/marker.rs`:

```rust
//! Text of the marker block a slot transition writes into its buffer.
//!
//! Pure formatting: one topic per line so no single line grows long. The only
//! unbounded field, the command, gets a line of its own and is clipped or
//! wrapped by the UI like any other line.

use std::time::Duration;

use crate::proc::{Outcome, Transition};
use crate::types::ExitStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{NewGen, OldGen};

    fn old(gen: u32, outcome: Outcome, ran_secs: u64) -> OldGen {
        OldGen {
            gen,
            pid: 47105,
            outcome,
            ran: Duration::from_secs(ran_secs),
        }
    }

    fn new(gen: u32, spawn: Result<i32, String>) -> NewGen {
        NewGen {
            gen,
            command: "target/debug/erhebimus".to_string(),
            spawn,
        }
    }

    #[test]
    fn restart_block_lists_header_old_new_and_command() {
        let t = Transition {
            proc: 0,
            old: Some(old(2, Outcome::Exited(ExitStatus::Signal(15)), 252)),
            new: new(3, Ok(48213)),
        };
        assert_eq!(
            restart_block(&t, "14:02:11"),
            vec![
                "── restart · gen 2 → 3 · 14:02:11 ──",
                "── gen 2: pid 47105 · killed by signal 15 · ran 4m12s ──",
                "── gen 3: pid 48213 ──",
                "── cmd: target/debug/erhebimus ──",
            ]
        );
    }

    #[test]
    fn restart_block_covers_every_old_outcome() {
        let exit = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Exited(ExitStatus::Code(101)), 3)),
            new: new(1, Ok(1)),
        };
        assert_eq!(
            restart_block(&exit, "x")[1],
            "── gen 0: pid 47105 · exit 101 · ran 3s ──"
        );
        let abandoned = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Abandoned, 7322)),
            new: new(1, Ok(1)),
        };
        assert_eq!(
            restart_block(&abandoned, "x")[1],
            "── gen 0: pid 47105 · abandoned · ran 2h02m ──"
        );
        let never = Transition {
            proc: 0,
            old: None,
            new: new(1, Ok(1)),
        };
        let lines = restart_block(&never, "x");
        assert_eq!(lines[0], "── start · gen 1 · x ──");
        assert_eq!(lines[1], "── gen 0: never started ──");
    }

    #[test]
    fn restart_block_reports_spawn_failure() {
        let failed = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Exited(ExitStatus::Code(0)), 1)),
            new: new(1, Err("No such file or directory".to_string())),
        };
        assert_eq!(
            restart_block(&failed, "x")[2],
            "── gen 1: spawn failed: No such file or directory ──"
        );
    }

    #[test]
    fn duration_uses_the_two_largest_units() {
        assert_eq!(fmt_duration(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_duration(Duration::from_secs(59)), "59s");
        assert_eq!(fmt_duration(Duration::from_secs(60)), "1m00s");
        assert_eq!(fmt_duration(Duration::from_secs(252)), "4m12s");
        assert_eq!(fmt_duration(Duration::from_secs(3600)), "1h00m");
        assert_eq!(fmt_duration(Duration::from_secs(7322)), "2h02m");
    }
}
```

Add `mod marker;` to `src/main.rs` after `mod buffer;`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q marker::`
Expected: compile error — `restart_block`, `fmt_duration` not found.

- [ ] **Step 3: Implement the formatting**

Insert between the `use` lines and the `tests` module in `src/marker.rs`:

```rust
/// The lines describing a completed transition, in buffer order. `clock` is
/// the already-formatted local time of the transition (`HH:MM:SS`);
/// formatting time is the UI's job, since only it knows the timezone.
pub fn restart_block(t: &Transition, clock: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(4);

    let header = match &t.old {
        Some(o) => format!("restart · gen {} → {}", o.gen, t.new.gen),
        None => format!("start · gen {}", t.new.gen),
    };
    lines.push(rule(&format!("{header} · {clock}")));

    match &t.old {
        Some(o) => {
            let outcome = match o.outcome {
                Outcome::Exited(ExitStatus::Code(c)) => format!("exit {c}"),
                Outcome::Exited(ExitStatus::Signal(s)) => format!("killed by signal {s}"),
                Outcome::Abandoned => "abandoned".to_string(),
            };
            lines.push(rule(&format!(
                "gen {}: pid {} · {} · ran {}",
                o.gen,
                o.pid,
                outcome,
                fmt_duration(o.ran)
            )));
        }
        None => {
            let gen = t.new.gen.saturating_sub(1);
            lines.push(rule(&format!("gen {gen}: never started")));
        }
    }

    let n = &t.new;
    match &n.spawn {
        Ok(pid) => lines.push(rule(&format!("gen {}: pid {}", n.gen, pid))),
        Err(e) => lines.push(rule(&format!("gen {}: spawn failed: {}", n.gen, e))),
    }
    lines.push(rule(&format!("cmd: {}", n.command)));
    lines
}

fn rule(text: &str) -> String {
    format!("── {text} ──")
}

/// Whole-second runtime in its two largest units: `12s`, `4m12s`, `2h02m`.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}
```

- [ ] **Step 4: Run the marker tests**

Run: `cargo test -q marker::`
Expected: 4 passed.

- [ ] **Step 5: Write the failing buffer and UI tests**

In `src/buffer.rs` tests module (find `mod tests` at the bottom and add):

```rust
    #[test]
    fn marker_line_is_plain_text_tagged_as_marker() {
        let sl = StyledLine::marker(2, 7, SystemTime::UNIX_EPOCH, "── restart ──".to_string());
        assert_eq!(sl.proc, 2);
        assert_eq!(sl.seq, 7);
        assert_eq!(sl.stream, StreamTag::Marker);
        let text: String = sl.content.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "── restart ──");
    }
```

In `src/ui.rs` tests module, next to `tagged_line_prepends_the_stamp_before_the_tag`:

```rust
    #[test]
    fn marker_lines_render_dim_without_the_stderr_marker() {
        let now = at(0);
        let sl = StyledLine::marker(0, 0, now, "── restart ──".to_string());
        let (line, prefix) = tagged_line(&sl, true, TimeDisplay::Off, now, &fixed_tz());
        // All-view keeps the process tag so the reader knows which slot it was.
        assert_eq!(plain(&line), "1│ ── restart ──");
        assert_eq!(prefix, 3);
        assert!(line.spans.iter().all(|s| s.content != "!"));
        let text_span = line.spans.last().unwrap();
        assert!(text_span.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn clock_formats_local_time_of_day() {
        let mut s = ui(1);
        s.tz = fixed_tz();
        assert_eq!(s.clock(at(3600 * 12 + 3 * 60 + 7)), "14:03:07");
    }
```

(`plain` and `at` and `fixed_tz` already exist in that module.)

- [ ] **Step 6: Run to verify failure**

Run: `cargo test -q marker_line`
Expected: compile error — no function `StyledLine::marker`, no method `clock`.

- [ ] **Step 7: Implement the constructor and rendering**

In `src/buffer.rs`, inside `impl StyledLine`, after `parse`:

```rust
    /// A line krawatte inserts itself (a restart marker): plain text, no ANSI
    /// parsing, tagged [`StreamTag::Marker`] so the UI renders it as a note
    /// rather than as process output.
    pub fn marker(proc: ProcId, seq: Seq, at: SystemTime, text: String) -> StyledLine {
        StyledLine {
            proc,
            stream: StreamTag::Marker,
            seq,
            at,
            content: TuiLine::from(text),
        }
    }
```

In `src/ui.rs` `tagged_line`, replace the final two statements (`spans.extend(...)` and the return) with:

```rust
    // Clone the parsed content spans into the new owned line. Marker lines are
    // krawatte's own notes, dimmed so they read as annotations between output.
    match sl.stream {
        StreamTag::Marker => spans.extend(
            sl.content
                .spans
                .iter()
                .cloned()
                .map(|s| s.patch_style(Style::default().add_modifier(Modifier::DIM))),
        ),
        StreamTag::Stdout | StreamTag::Stderr => spans.extend(sl.content.spans.iter().cloned()),
    }
    (TuiLine::from(spans), prefix_width)
```

In `impl UiState`, after `wrap()`:

```rust
    /// Local time of day of `at` as `HH:MM:SS`, in the UI's timezone. Used for
    /// the restart marker header, which is text stored in the buffer and so is
    /// formatted once, at insertion, unlike the live timestamp prefix.
    pub fn clock(&self, at: SystemTime) -> String {
        format_time_only(at, &self.tz)
    }
```

Remove the `#[allow(dead_code)]` you put on `StreamTag::Marker` in Task 1.

- [ ] **Step 8: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 83 passed, no warnings. (`restart_block` and `StyledLine::marker` have no non-test caller until Task 7; if clippy flags them, add `#[allow(dead_code)]` and remove it in Task 7.)

- [ ] **Step 9: Commit**

```bash
git add src/marker.rs src/main.rs src/buffer.rs src/ui.rs
git commit -m "Add restart marker block formatting and rendering"
```

---

### Task 6: `r` / `k` keys and the `↻` health glyph

**Files:**
- Modify: `src/ui.rs` (`Action`, `KeyCommand`, `map_key`, `handle_key`, `health_glyph`, `UiState::health`, tests)

**Interfaces:**
- Produces: `Action::Restart(ProcId)`, `Action::Kill(ProcId)`, `KeyCommand::Restart`, `KeyCommand::Kill`, `UiState::health(&self, proc) -> Health`.

- [ ] **Step 1: Write the failing tests**

In the `src/ui.rs` tests module, next to `handle_key_quit`:

```rust
    #[test]
    fn map_key_restart_and_kill() {
        assert_eq!(map_key(key(KeyCode::Char('r'))), KeyCommand::Restart);
        assert_eq!(map_key(key(KeyCode::Char('k'))), KeyCommand::Kill);
    }

    #[test]
    fn restart_and_kill_act_only_in_a_single_pane() {
        let mut s = ui(3);
        // All-view: silent no-op, view unchanged.
        assert_eq!(s.handle_key(key(KeyCode::Char('r'))), Action::Continue);
        assert_eq!(s.handle_key(key(KeyCode::Char('k'))), Action::Continue);
        assert_eq!(s.view(), View::All);

        s.handle_key(key(KeyCode::Char('2')));
        assert_eq!(s.handle_key(key(KeyCode::Char('r'))), Action::Restart(1));
        assert_eq!(s.handle_key(key(KeyCode::Char('k'))), Action::Kill(1));
        // The key neither changes the view nor touches the scroll position.
        assert_eq!(s.view(), View::Single(1));
        assert!(s.following());
    }

    #[test]
    fn health_accessor_reflects_set_health() {
        let mut s = ui(2);
        assert_eq!(s.health(1), Health::Running);
        s.set_health(1, Health::Restarting);
        assert_eq!(s.health(1), Health::Restarting);
    }
```

Extend `health_glyph_variants`:

```rust
        assert_eq!(health_glyph(Health::Restarting).0, "↻");
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q restart_and_kill`
Expected: compile error — no variant `KeyCommand::Restart`, `Action::Restart`.

- [ ] **Step 3: Implement**

`Action`:

```rust
pub enum Action {
    /// Continue running.
    Continue,
    /// User requested quit (`q` or Ctrl-C); begin shutdown.
    Quit,
    /// `r` in a single pane: restart that slot's current generation.
    Restart(ProcId),
    /// `k` in a single pane: kill that slot's current generation and apply its
    /// on-exit policy.
    Kill(ProcId),
}
```

`KeyCommand` (before `None`):

```rust
    /// `r`: restart the viewed slot.
    Restart,
    /// `k`: kill the viewed slot's current generation.
    Kill,
```

`map_key` (next to `'w'`):

```rust
        KeyCode::Char('r') => KeyCommand::Restart,
        KeyCode::Char('k') => KeyCommand::Kill,
```

`handle_key` (before `KeyCommand::None => {}`). In the all-view there is no slot to act on, so the keys do nothing at all — no view change, no message:

```rust
            KeyCommand::Restart => {
                if let View::Single(p) = self.view {
                    return Action::Restart(p);
                }
            }
            KeyCommand::Kill => {
                if let View::Single(p) = self.view {
                    return Action::Kill(p);
                }
            }
```

`health_glyph` (after `Running`):

```rust
        Health::Restarting => ("↻".to_string(), Style::default().fg(Color::Yellow)),
```

`impl UiState`, after `set_health`:

```rust
    /// Current health of a slot, as shown in the status bar.
    pub fn health(&self, proc: ProcId) -> Health {
        self.health.get(proc).copied().unwrap_or(Health::Running)
    }
```

Remove the `#[allow(dead_code)]` from `Health::Restarting` (Task 1).

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 86 passed. If clippy flags `UiState::health` as unused outside tests, leave it — Task 7's main test uses it; add a temporary `#[allow(dead_code)]` only if the warning blocks you and remove it in Task 7.

- [ ] **Step 5: Commit**

```bash
git add src/ui.rs
git commit -m "Add r and k hotkeys and the restarting health glyph"
```

---

### Task 7: Wire it together in `main.rs`, document, smoke-test

**Files:**
- Modify: `src/main.rs` (`event_loop`, `drain_events`, new `apply_transition`, tests)
- Modify: `README.md`

**Interfaces:**
- Consumes: `ProcManager::{replace, kill, tick, is_current, is_restarting, current_command, next_seq}`, `Transition`, `marker::restart_block`, `StyledLine::marker`, `UiState::{clock, health}`, `Action::{Restart, Kill}`.

- [ ] **Step 1: Write the failing test**

In `src/main.rs` `mod tests`:

```rust
    use crate::types::{Gen, StreamTag};
    use std::sync::mpsc;
    use std::time::{Instant, SystemTime};

    fn line(proc: usize, gen: Gen, text: &str) -> Event {
        Event::Line {
            proc,
            gen,
            stream: StreamTag::Stdout,
            seq: 0,
            at: SystemTime::UNIX_EPOCH,
            bytes: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn stale_generation_events_are_dropped_and_transitions_write_markers() {
        let (tx, rx) = mpsc::channel();
        let config = Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        };
        let mut manager = ProcManager::spawn_all(&["sleep 30".to_string()], &config, tx.clone());
        let mut buffers = BufferSet::new(1, &config);
        let mut ui = UiState::new(vec!["sleep".to_string()]);

        assert!(manager.replace(0, "sleep 30".to_string()));
        ui.set_health(0, Health::Restarting);

        // Mid-teardown: output from the dying generation is still shown, but
        // its exit must not flip the health away from Restarting.
        tx.send(line(0, 0, "shutting down")).unwrap();
        tx.send(Event::Exited {
            proc: 0,
            gen: 0,
            status: ExitStatus::Signal(15),
        })
        .unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &manager);
        assert_eq!(buffers.buffer(0).len(), 1);
        assert_eq!(ui.health(0), Health::Restarting);

        // Drive the restart to completion.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut transitions = Vec::new();
        while transitions.is_empty() && Instant::now() < deadline {
            drain_events(&rx, &mut buffers, &mut ui, &manager);
            transitions = manager.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = transitions.pop().expect("restart completed");
        apply_transition(&t, &manager, &mut buffers, &mut ui);
        assert_eq!(ui.health(0), Health::Running);
        // The four-line marker block followed the one real line.
        assert_eq!(buffers.buffer(0).len(), 5);
        assert!(
            buffers
                .buffer(0)
                .iter()
                .skip(1)
                .all(|l| l.stream == StreamTag::Marker)
        );

        // After the swap, generation 0 is stale and generation 1 is live.
        tx.send(line(0, 0, "late")).unwrap();
        tx.send(line(0, 1, "fresh")).unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &manager);
        assert_eq!(buffers.buffer(0).len(), 6);
        assert_eq!(ui.health(0), Health::Running);

        manager.shutdown();
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q stale_generation`
Expected: compile error — `apply_transition` not found.

- [ ] **Step 3: Implement the wiring**

Imports at the top of `src/main.rs`:

```rust
use crate::buffer::{BufferSet, StyledLine};
use crate::proc::{ProcManager, Transition};
use crate::types::{Config, Event, ExitStatus, Health};
use crate::ui::{Action, UiState};
```

Replace the key-handling `if` in `event_loop` and add the tick:

```rust
        // Poll for a key event with a short timeout so we stay responsive to
        // process output even when the user is idle.
        if event::poll(Duration::from_millis(50))?
            && let CtEvent::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match ui.handle_key(key) {
                Action::Quit => return Ok(()),
                Action::Restart(p) => {
                    let command = manager.current_command(p).to_string();
                    if manager.replace(p, command) {
                        ui.set_health(p, Health::Restarting);
                    }
                }
                Action::Kill(p) => {
                    if manager.kill(p) {
                        ui.set_health(p, Health::Restarting);
                    }
                }
                Action::Continue => {}
            }
        }

        // Drain all currently-available process events into the buffers / UI,
        // then advance any in-flight restarts.
        drain_events(rx, buffers, ui, manager);
        for t in manager.tick() {
            apply_transition(&t, manager, buffers, ui);
        }
```

Replace `drain_events` and add `apply_transition`:

```rust
/// Apply every currently-pending process event to the buffers and UI health.
///
/// Events from a generation other than the slot's current one are stale --
/// late output from a replaced process, or from a grandchild that escaped its
/// group and still holds the old pipe -- and are dropped. While a teardown is
/// in flight the dying generation's lines are still shown (its shutdown output
/// is real), but its exit is not: the slot is `Restarting`, not `✖ sig 15`.
fn drain_events(
    rx: &mpsc::Receiver<Event>,
    buffers: &mut BufferSet,
    ui: &mut UiState,
    manager: &ProcManager,
) {
    for ev in rx.try_iter() {
        match ev {
            Event::Line {
                proc,
                gen,
                stream,
                seq,
                at,
                bytes,
            } => {
                if manager.is_current(proc, gen) {
                    buffers.push(StyledLine::parse(proc, stream, seq, at, &bytes));
                }
            }
            Event::Exited { proc, gen, status } => {
                if manager.is_current(proc, gen) && !manager.is_restarting(proc) {
                    ui.set_health(proc, health_from_exit(status));
                }
            }
            Event::SpawnFailed { proc, gen, .. } => {
                if manager.is_current(proc, gen) {
                    ui.set_health(proc, Health::SpawnFailed);
                }
            }
        }
    }
}

/// Record a completed restart in the slot's buffer and set its health.
fn apply_transition(
    t: &Transition,
    manager: &ProcManager,
    buffers: &mut BufferSet,
    ui: &mut UiState,
) {
    let at = SystemTime::now();
    for text in marker::restart_block(t, &ui.clock(at)) {
        buffers.push(StyledLine::marker(t.proc, manager.next_seq(), at, text));
    }
    let health = match t.new.spawn {
        Ok(_) => Health::Running,
        Err(_) => Health::SpawnFailed,
    };
    ui.set_health(t.proc, health);
}
```

Add `use std::time::SystemTime;` to the top-level imports (`use std::time::{Duration, SystemTime};`).

Remove any temporary `#[allow(dead_code)]` added in Tasks 5 and 6.

- [ ] **Step 4: Run the suite and clippy**

Run: `cargo test -q && cargo clippy --all-targets -q`
Expected: 87 passed, no warnings.

- [ ] **Step 5: Manual smoke test**

Run: `cargo run -q -- "while true; do date; sleep 1; done" "sleep 100"` then:

1. Press `1`, then `r`. The status bar shows `↻` briefly, then `●`; the pane shows the four marker lines dimmed, and the date loop resumes from a new pid.
2. Press `2`, then `k`. `sleep 100` is killed (`ran Ns`, `killed by signal 15`) and respawned.
3. Press `0`, then `r` and `k`: nothing happens.
4. Press `d` once: marker lines carry timestamps like every other line. Press `w`: a long `cmd:` line wraps.
5. Press `r` in pane 1 and immediately `q`: krawatte exits within the grace period and prints final statuses.

Expected: all five as described, no panic, terminal restored.

- [ ] **Step 6: Document**

In `README.md`, add to the Keys table after the `w` row:

```markdown
| `r` | restart the viewed pane's process (no-op in the all-view) |
| `k` | kill the viewed pane's process; it is restarted like `r` (no-op in the all-view) |
```

Add to the Behavior list after the **A child exiting** bullet:

```markdown
- **Restart** (`r`/`k` in a single pane) sends SIGTERM to that child's process
  group, waits out the grace period, SIGKILLs stragglers, then runs the same
  command again in the same slot. The UI stays live throughout; the slot shows
  `↻` while the old process is being torn down. The buffer is kept and a dim
  marker block records the transition — generation numbers, pids, how the old
  process ended and how long it ran, and the command. Output that arrives late
  from the old process is discarded. A child that exits on its own is *not*
  restarted.
```

Extend the **Status bar** bullet's glyph list: `` (`●` running, `↻` restarting, `✔ exit 0`, `✖ exit N`) ``.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs README.md
git commit -m "Add per-slot restart with r and k hotkeys"
```

---

## Self-review

**Spec coverage.**
- `r`/`k` semantics, all-view no-op, in-flight no-op → Tasks 6 (keys) and 3/4 (`replace`/`kill` return `false`).
- Dead / never-started slot restarts immediately → Task 3 (`begin` marks finished, machine starts `Done`; tests for both).
- No crash-restart → a self-exit only reaches `drain_events`, which sets health; nothing respawns.
- `↻` health, old exit never shows as `✖` → Task 6 glyph; Task 7 `is_restarting` filter, tested.
- Marker block with every outcome, one topic per line → Task 5, tested for exit/signal/abandoned/never started/spawn failed.
- Generations on events, stale drop → Tasks 1, 3, 7 (tested in `proc.rs` via `is_current` and in `main.rs` end to end).
- Invariant "one live generation per slot", `q` mid-restart bounded → Task 3 (`complete` takes `live` before spawning; `shutdown` clears `restart`; test `shutdown_started_mid_restart_returns_within_the_bound`).
- Abandoned groups don't hang a restart → inherited from `ShutdownMachine`'s `KILL_REAP_TIMEOUT`; `complete` handles `abandoned()`.
- Background job in old group killed by restart → Task 3 test.
- `Respawned` → named `Transition`; noted in the header. `k` = kill + spawn standard (Task 4), no policy enum.

**Placeholder scan.** None.

**Type consistency.** `Transition { proc, old: Option<OldGen>, new: NewGen }`, `NewGen.spawn: Result<i32, String>`, `OldGen.ran: Duration`, `Outcome::{Exited(ExitStatus), Abandoned}` used identically in Tasks 3, 5, 7. `replace(proc, String) -> bool`, `kill(proc) -> bool`, `tick() -> Vec<Transition>` consistent across Tasks 3, 4, 7. `UiState::clock(SystemTime) -> String` and `health(ProcId) -> Health` defined in Tasks 5/6, used in Task 7.
