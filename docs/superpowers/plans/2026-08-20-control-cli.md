# Control Socket and CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Red–green TDD is mandatory** (see Global Constraints): test first, watch it fail, then implement. Use superpowers:test-driven-development for every task.

**Goal:** A running krawatte listens on a unix socket; `krawatte status|restart|kill|stop|start|run|quit|logs` from anywhere in the project talks to it, human-readable by default and JSON on request. `run` adds the one-shot override that resumes the standard command when it exits.

**Architecture:** `protocol.rs` holds the serde request/response types (pure). `control.rs` owns the socket path derivation, the listener with stale/live detection, the per-connection threads that forward `Event::Control { request, reply }` onto the existing channel, and `handle()`, the pure-given-the-manager request handler. `ProcManager` learns generation kinds (`Standard`/`Override`), `stop` (teardown with nothing to spawn), and a `snapshot` for status. `main` routes control events, parks `--wait` replies until the matching transitions complete, and implements the two remaining override rules (self-exit resumes; watch events ignore overrides). `cli.rs` is the synchronous client behind clap subcommands.

**Tech Stack:** Rust 2024, `serde_json` (new); std `UnixListener`/`UnixStream`; `nix::unistd::getuid`; existing `clap`, `serde`.

**Spec:** `docs/superpowers/specs/2026-08-20-control-cli-design.md`. Roadmap: `docs/superpowers/specs/2026-08-20-roadmap.md`.

## Global Constraints

- **Red–green TDD is mandatory.** Test first; run it and *observe the expected failure*; minimal implementation; green; refactor under green. Never write implementation before the red run. Do not reorder or batch steps across tasks.
- `gen` is a reserved keyword in edition 2024; spell it `r#gen`.
- Baseline: `cargo test -q` → 129 passed, clippy silent, `cargo fmt --check` clean. Keep all three clean after every task; `cargo fmt` before committing.
- Nothing in the TUI thread may block on a client: connection threads do the blocking; the main loop only `try_iter`s the channel.
- The socket lives under `$XDG_RUNTIME_DIR/krawatte/` (fallback `/tmp/krawatte-<uid>/`), dir `0700`, socket `0600`, named by the FNV-1a-64 hex of the canonical project dir. Never inside the project.
- One request per connection, one JSON object per line each way, `"v": 1` on requests.
- `all` is the reserved word for every slot; verbs on `all` skip in-flight slots and list them; exit 0 unless every slot was skipped.
- Override rules: `r` restarts the override itself; `k`/`kill` return to standard; self-exit of an override resumes standard (`Trigger::Resume`); watch events never touch an override; a new `run` replaces a running override (subject to the in-flight rule).
- `stop` leaves the slot dead with its exit shown; `start` revives a dead slot, reports `already running` otherwise; `quit` is the `q` path.
- Exit codes: 0 ok; 1 instance refused; 2 usage/config; 3 no instance running.
- Commit after every task; imperative messages; do not push.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `src/buffer.rs` | `StyledLine` keeps `raw: Vec<u8>` and `r#gen`; `plain()`. |
| `src/types.rs` | `Trigger::{Cli(String), Resume}`; `Event::Control { request, reply }`. |
| `src/proc.rs` | `GenKind`, `Proc.kind`, `replace_with`, `stop`, `is_override`, `is_dead`, `snapshot`/`SlotInfo`; `Restart.next: Option<String>`, `Transition.new: Option<NewGen>`. |
| `src/marker.rs` | `stop` header + `slot stopped` line; `cli <verb>` / `resume` labels. |
| `src/protocol.rs` (new) | `Request`, `Response`, `ProcStatus`, `LogLine`, `Started`, `Skipped`, `Envelope`; `PROTOCOL_VERSION`. |
| `src/control.rs` (new) | `socket_path`, `fnv1a64`, `Listener::{bind, serve}`, `BindError`, `handle`, `Handled`, `resolve_slot`, `parse_duration`. |
| `src/cli.rs` (new) | clap `Sub` enum, `run_client`, human rendering. |
| `src/ui.rs` | `set_control`, `set_override`; `*` after override names; `CTRL`/`NO CTRL` marker. |
| `src/main.rs` | subcommand dispatch; listener lifecycle; `Event::Control` routing; waiters; quit flag; resume-on-exit; watch rule 2. |
| `README.md` | CLI section. |

---

### Task 1: Stored lines keep their raw bytes and generation

**Files:**
- Modify: `src/buffer.rs`, `src/main.rs`, `src/ui.rs` (test call sites)

**Interfaces:**
- Produces: `StyledLine { …, pub r#gen: Gen, pub raw: Vec<u8> }`; `StyledLine::parse(proc, r#gen, stream, seq, at, bytes)`; `StyledLine::marker(proc, r#gen, seq, at, text)`; `pub fn plain(&self) -> String`.

- [ ] **Step 1: Write the failing tests**

In `src/buffer.rs` tests:

```rust
    #[test]
    fn parse_keeps_raw_bytes_and_generation_and_yields_plain_text() {
        let raw = b"\x1b[31mred\x1b[0m text";
        let sl = StyledLine::parse(1, 4, StreamTag::Stdout, 9, SystemTime::UNIX_EPOCH, raw);
        assert_eq!(sl.r#gen, 4);
        assert_eq!(sl.raw, raw.to_vec());
        assert_eq!(sl.plain(), "red text");
        let m = StyledLine::marker(1, 4, 10, SystemTime::UNIX_EPOCH, "── x ──".to_string());
        assert_eq!(m.raw, "── x ──".as_bytes());
        assert_eq!(m.plain(), "── x ──");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q parse_keeps_raw`
Expected: compile error — wrong number of arguments / no field `raw`.

- [ ] **Step 3: Implement**

```rust
pub struct StyledLine {
    pub proc: ProcId,
    /// Generation of the slot that produced the line; markers carry the
    /// generation current when they were inserted.
    pub r#gen: Gen,
    pub stream: StreamTag,
    pub seq: Seq,
    pub at: SystemTime,
    /// The bytes as written (ANSI escapes intact), so a client asking for
    /// color gets exactly what the process emitted.
    pub raw: Vec<u8>,
    /// ANSI-parsed styled content, owned (`'static`).
    pub content: TuiLine<'static>,
}
```

`parse` takes `r#gen: Gen` after `proc`, stores `raw: bytes.to_vec()`; `marker(proc, r#gen, seq, at, text)` stores `raw: text.clone().into_bytes()` then `content: TuiLine::from(text)`. Add:

```rust
    /// The text without styling: the concatenated span contents.
    pub fn plain(&self) -> String {
        self.content.spans.iter().map(|s| s.content.as_ref()).collect()
    }
```

Update every caller: `main.rs` `drain_events` (`StyledLine::parse(proc, r#gen, stream, seq, at, &bytes)`), `apply_transition` (`StyledLine::marker(t.proc, manager.current_gen(t.proc), …)`), and the test call sites in `buffer.rs`, `ui.rs`, `main.rs` (pass `0` for the generation). The `ui.rs` test helper `plain(&TuiLine)` can stay.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 130 passed, no warnings (`raw`/`plain` read only by tests until Task 5: `#[allow(dead_code)] // read by the control handler, a later task` if clippy insists; remove in Task 5).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/buffer.rs src/main.rs src/ui.rs
git commit -m "Keep raw bytes and generation on stored lines"
```

---

### Task 2: Generation kinds, `stop`, `Resume`, and the stop marker

**Files:**
- Modify: `src/types.rs`, `src/proc.rs`, `src/marker.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  ```rust
  // types.rs
  pub enum Trigger { Key(char), Watch { .. }, Cli(String), Resume }
  // proc.rs
  pub enum GenKind { Standard, Override }
  pub struct SlotInfo { pub index: usize, pub name: String, pub r#gen: Gen, pub pid: Option<i32>, pub alive: bool, pub command: String, pub standard: String, pub kind: GenKind, pub since: Option<Duration> }
  impl ProcManager {
      pub fn replace_with(&mut self, proc, command: String, kind: GenKind, trigger: Trigger) -> bool;  // replace() keeps the current kind; kill() sets Standard
      pub fn stop(&mut self, proc, trigger: Trigger) -> bool;
      pub fn kind(&self, proc) -> GenKind;
      pub fn is_override(&self, proc) -> bool;
      pub fn is_dead(&self, proc) -> bool;
      pub fn snapshot(&self, proc) -> SlotInfo;
  }
  pub struct Transition { pub proc, pub old: Option<OldGen>, pub new: Option<NewGen>, pub trigger }
  ```

- [ ] **Step 1: Write the failing tests**

`src/proc.rs` tests:

```rust
    #[test]
    fn override_kind_survives_r_and_is_dropped_by_kill() {
        let (tx, _rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["sleep 30".to_string()], &short_grace(), tx);
        assert_eq!(mgr.kind(0), GenKind::Standard);
        assert!(mgr.replace_with(0, "sleep 31".to_string(), GenKind::Override, Trigger::Cli("run".into())));
        tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert!(mgr.is_override(0));
        assert_eq!(mgr.current_command(0), "sleep 31");

        // `r`: same command, still an override.
        let cmd = mgr.current_command(0).to_string();
        assert!(mgr.replace(0, cmd, Trigger::Key('r')));
        tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert!(mgr.is_override(0));
        assert_eq!(mgr.current_command(0), "sleep 31");

        // `k`: back to the standard command, standard kind.
        assert!(mgr.kill(0, Trigger::Key('k')));
        let t = tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(t.new.as_ref().unwrap().command, "sleep 30");
        assert!(!mgr.is_override(0));
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn stop_leaves_the_slot_dead_with_its_status_and_start_revives_it() {
        let (tx, _rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["sleep 30".to_string()], &short_grace(), tx);
        assert!(!mgr.is_dead(0));
        assert!(mgr.stop(0, Trigger::Cli("stop".into())));
        assert!(!mgr.stop(0, Trigger::Cli("stop".into())), "in flight");
        let t = tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert!(t.new.is_none());
        let old = t.old.unwrap();
        assert_eq!(old.r#gen, 0);
        assert_eq!(old.outcome, Outcome::Exited(ExitStatus::Signal(15)));
        assert_eq!(t.trigger, Trigger::Cli("stop".into()));
        assert!(mgr.is_dead(0));
        assert_eq!(mgr.current_gen(0), 0, "no new generation");
        // The dead generation stays on the slot so the final printout has its status.
        assert!(mgr.was_started(0));
        let info = mgr.snapshot(0);
        assert_eq!(info.pid, None);
        assert!(!info.alive);

        assert!(mgr.replace_with(0, mgr.standard_command(0).to_string(), GenKind::Standard, Trigger::Cli("start".into())));
        let t = tick_until_transition(&mut mgr, Duration::from_secs(5));
        assert_eq!(t.new.unwrap().r#gen, 1);
        assert!(!mgr.is_dead(0));
        assert_eq!(mgr.shutdown().len(), 1);
    }

    #[test]
    fn snapshot_describes_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["echo $$; sleep 30".to_string()], &short_grace(), tx);
        let pid = read_pid_line(&rx);
        let info = mgr.snapshot(0);
        assert_eq!(info.index, 1);
        assert_eq!(info.name, "echo");
        assert_eq!(info.r#gen, 0);
        assert_eq!(info.pid, Some(pid.as_raw()));
        assert!(info.alive);
        assert_eq!(info.command, "echo $$; sleep 30");
        assert_eq!(info.standard, "echo $$; sleep 30");
        assert_eq!(info.kind, GenKind::Standard);
        assert!(info.since.is_some());
        shutdown_within(mgr, Duration::from_secs(5));
    }
```

Existing proc tests that read `t.new.gen`/`t.new.command`/`t.new.spawn` now go through `Option`: use `t.new.as_ref().unwrap()` (or `.unwrap()` when `t` is not reused). `restart_of_never_started_slot_reports_no_old_generation` and the others: adjust mechanically.

`src/marker.rs` tests: existing literals use `new: Some(new(...))`; add

```rust
    #[test]
    fn stop_and_cli_and_resume_triggers_render() {
        let stopped = Transition {
            proc: 0,
            old: Some(old(1, Outcome::Exited(ExitStatus::Signal(15)), 1)),
            new: None,
            trigger: Trigger::Cli("stop".to_string()),
        };
        assert_eq!(
            restart_block(&stopped, "x"),
            vec![
                "── stop · gen 1 · x · cli stop ──",
                "── gen 1: pid 47105 · killed by signal 15 · ran 1s ──",
                "── slot stopped ──",
            ]
        );
        let resumed = Transition {
            proc: 0,
            old: Some(old(3, Outcome::Exited(ExitStatus::Code(0)), 30)),
            new: Some(new(4, Ok(7))),
            trigger: Trigger::Resume,
        };
        assert_eq!(restart_block(&resumed, "x")[0], "── restart · gen 3 → 4 · x · resume ──");
        let ran = Transition { trigger: Trigger::Cli("run".to_string()), ..resumed };
        assert_eq!(restart_block(&ran, "x")[0], "── restart · gen 3 → 4 · x · cli run ──");
    }
```

`src/main.rs` test `stale_generation_events_are_dropped_and_transitions_write_markers`: nothing changes semantically; it compiles once `apply_transition` handles `Option`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q override_kind`
Expected: compile errors — `GenKind`, `replace_with`, `stop`, `snapshot` not found; `Trigger::Cli` not found.

- [ ] **Step 3: Implement**

`src/types.rs` — extend `Trigger`:

```rust
    /// A CLI verb over the control socket: `restart`, `kill`, `stop`,
    /// `start`, `run`.
    Cli(String),
    /// An override exited on its own; the standard command resumes.
    Resume,
```

`src/proc.rs`:

```rust
/// What kind of command a slot's current generation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenKind {
    /// The configured command.
    Standard,
    /// A one-shot command put there by `krawatte run`; when it exits on its
    /// own the standard command resumes.
    Override,
}

/// A point-in-time description of a slot, for `krawatte status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotInfo {
    /// 1-based, as shown in the status bar.
    pub index: usize,
    pub name: String,
    pub r#gen: Gen,
    /// Pid of the current generation's leader while it is alive.
    pub pid: Option<i32>,
    pub alive: bool,
    /// What the current generation runs (or ran).
    pub command: String,
    pub standard: String,
    pub kind: GenKind,
    /// Time since the current generation was spawned, if it ever was.
    pub since: Option<Duration>,
}
```

`Proc` gains `kind: GenKind` (init `GenKind::Standard`). `Restart` gains `kind: GenKind` and `next` becomes `Option<String>`. `Transition.new` becomes `Option<NewGen>`.

```rust
    /// Tear down the slot's current generation (if any), then spawn `command`
    /// as a generation of the given kind. Returns `false`, doing nothing, if a
    /// restart is already in flight.
    pub fn replace_with(&mut self, proc: ProcId, command: String, kind: GenKind, trigger: Trigger) -> bool {
        self.begin(proc, Some(command), kind, trigger)
    }

    /// Replace keeping the current generation's kind: `r` on an override
    /// restarts the override.
    pub fn replace(&mut self, proc: ProcId, command: String, trigger: Trigger) -> bool {
        let kind = self.procs[proc].kind;
        self.replace_with(proc, command, kind, trigger)
    }

    /// Tear down the current generation and spawn the slot's standard
    /// command. Ends an override early.
    pub fn kill(&mut self, proc: ProcId, trigger: Trigger) -> bool {
        let standard = self.procs[proc].spec.command.clone();
        self.replace_with(proc, standard, GenKind::Standard, trigger)
    }

    /// Tear down the current generation and leave the slot dead. The retired
    /// generation stays on the slot so its exit status is still reported.
    pub fn stop(&mut self, proc: ProcId, trigger: Trigger) -> bool {
        self.begin(proc, None, GenKind::Standard, trigger)
    }

    fn begin(&mut self, proc: ProcId, next: Option<String>, kind: GenKind, trigger: Trigger) -> bool {
        // body of today's `replace` up to building `Restart { machine, next, kind, trigger }`
    }
```

`complete` becomes:

```rust
    fn complete(&mut self, proc: ProcId, restart: Restart) -> Transition {
        let Restart { machine, next, kind, trigger } = restart;
        let abandoned = !machine.abandoned().is_empty();
        let old = self.procs[proc].live.as_mut().map(|g| retire(g, abandoned));
        let new = next.map(|command| {
            // A new generation replaces the retired one entirely.
            self.procs[proc].live = None;
            let r#gen = self.procs[proc].r#gen + 1;
            self.procs[proc].r#gen = r#gen;
            let spec = &self.procs[proc].spec;
            let spawn = match spawn_one(proc, r#gen, &self.shell, &command, spec, &self.seq, &self.tx) {
                Ok(g) => { let pid = g.pid; self.procs[proc].live = Some(g); Ok(pid) }
                Err(e) => { let _ = self.tx.send(Event::SpawnFailed { proc, r#gen, error: e.to_string() }); Err(e.to_string()) }
            };
            self.procs[proc].kind = kind;
            NewGen { r#gen, command, spawn }
        });
        Transition { proc, old, new, trigger }
    }
```

```rust
/// Close out a generation whose group is gone (or abandoned): join its
/// waiter if it has finished, mark it so nothing signals the pgid again, and
/// describe how it ended.
fn retire(g: &mut Generation, abandoned: bool) -> OldGen {
    let ran = g.started.elapsed();
    if g.dead.load(Ordering::SeqCst)
        && let Some(h) = g.waiter.take()
    {
        let _ = h.join();
    }
    g.finished = true;
    let outcome = if abandoned {
        Outcome::Abandoned
    } else {
        let status = *g.status.lock().unwrap();
        Outcome::Exited(status.unwrap_or(ExitStatus::Code(-1)))
    };
    OldGen { r#gen: g.r#gen, pid: g.pid, outcome, ran }
}
```

(`finished = true` on a retired-but-kept generation means global shutdown and `Drop` skip it, which is right: its group is gone or was given up on.)

Accessors:

```rust
    pub fn kind(&self, proc: ProcId) -> GenKind { self.procs[proc].kind }
    pub fn is_override(&self, proc: ProcId) -> bool { self.procs.get(proc).is_some_and(|p| p.kind == GenKind::Override) }
    /// No live generation, or one that has exited.
    pub fn is_dead(&self, proc: ProcId) -> bool {
        self.procs[proc].live.as_ref().is_none_or(|g| g.dead.load(Ordering::SeqCst))
    }
    pub fn snapshot(&self, proc: ProcId) -> SlotInfo {
        let p = &self.procs[proc];
        let alive = !self.is_dead(proc);
        SlotInfo {
            index: proc + 1,
            name: p.spec.name.clone(),
            r#gen: p.r#gen,
            pid: p.live.as_ref().filter(|_| alive).map(|g| g.pid),
            alive,
            command: self.current_command(proc).to_string(),
            standard: p.spec.command.clone(),
            kind: p.kind,
            since: p.live.as_ref().map(|g| g.started.elapsed()),
        }
    }
```

`src/marker.rs`:

```rust
    let header = match (&t.old, &t.new) {
        (Some(o), Some(n)) => format!("restart · gen {} → {}", o.r#gen, n.r#gen),
        (None, Some(n)) => format!("start · gen {}", n.r#gen),
        (Some(o), None) => format!("stop · gen {}", o.r#gen),
        (None, None) => "stop".to_string(),
    };
```

old-line: when `old` is `None`, the "never started" line uses `t.new.as_ref().map_or(0, |n| n.r#gen.saturating_sub(1))`. New-line block: `match &t.new { Some(n) => { pid/spawn-failed line; cmd line } None => lines.push(rule("slot stopped")) }`. `trigger_label`: `Trigger::Cli(v) => format!("cli {v}")`, `Trigger::Resume => "resume".to_string()`.

`src/main.rs` `apply_transition`:

```rust
    let health = match (&t.new, &t.old) {
        (Some(n), _) => match n.spawn { Ok(_) => Health::Running, Err(_) => Health::SpawnFailed },
        // Stopped: show how the retired generation ended. An abandoned one
        // was sent SIGKILL, the closest thing the bar can say.
        (None, Some(o)) => match o.outcome {
            Outcome::Exited(status) => health_from_exit(status),
            Outcome::Abandoned => Health::ExitedErr(ExitStatus::Signal(9)),
        },
        (None, None) => Health::SpawnFailed,
    };
```

(import `Outcome`). `StyledLine::marker(t.proc, manager.current_gen(t.proc), …)` stays.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 134 passed, no warnings (temporary allows on `snapshot`, `SlotInfo`, `is_override`, `kind` permitted with the usual comment; removed in Tasks 5–6).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/types.rs src/proc.rs src/marker.rs src/main.rs
git commit -m "Add generation kinds, stop, and resume/cli triggers"
```

---

### Task 3: Override rules in the TUI: resume on self-exit, watch ignores overrides, `*` in the bar

**Files:**
- Modify: `src/main.rs`, `src/ui.rs`

**Interfaces:**
- Produces: `UiState::set_override(&mut self, proc, bool)`; `UiState::set_control(&mut self, Option<bool>)` (`None` = no socket attempted, `Some(true)` = `CTRL`, `Some(false)` = `NO CTRL`); `UiState::status_markers(&self) -> Vec<Span<'static>>` (the FOLLOW/SCROLL/WRAP/CTRL tail, factored for testing).

- [ ] **Step 1: Write the failing tests**

`src/ui.rs`:

```rust
    #[test]
    fn override_slots_show_a_star_and_control_state_is_marked() {
        let mut s = ui(2);
        s.set_override(1, true);
        assert_eq!(plain(&TuiLine::from(s.slot_label(1))), "[2] p1* ●");
        assert_eq!(plain(&TuiLine::from(s.slot_label(0))), "[1] p0 ●");

        assert_eq!(plain(&TuiLine::from(s.status_markers())), " FOLLOW");
        s.set_control(Some(true));
        assert_eq!(plain(&TuiLine::from(s.status_markers())), " FOLLOW CTRL");
        s.set_control(Some(false));
        let markers = TuiLine::from(s.status_markers());
        assert_eq!(plain(&markers), " FOLLOW NO CTRL");
        assert_eq!(markers.spans.last().unwrap().style.fg, Some(Color::Red));
    }
```

`src/main.rs`:

```rust
    #[test]
    fn an_override_that_exits_resumes_the_standard_command() {
        let (tx, rx) = mpsc::channel();
        let config = Config { grace_period: Duration::from_millis(200), ..Config::default() };
        let mut manager = ProcManager::spawn_all(&["sleep 30".to_string()], &config, tx.clone());
        let mut buffers = BufferSet::new(1, &config);
        let mut ui = UiState::new(vec!["sleep".to_string()]);

        assert!(manager.replace_with(0, "true".to_string(), GenKind::Override, Trigger::Cli("run".into())));
        let t = tick_until(&mut manager, Duration::from_secs(5));
        apply_transition(&t, &manager, &mut buffers, &mut ui);
        assert!(ui.override_marked(0));

        // Wait for `true` to exit, then let the main loop see it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !manager.is_dead(0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        assert!(manager.is_restarting(0), "self-exit of an override starts the resume");
        let t = tick_until(&mut manager, Duration::from_secs(5));
        assert_eq!(t.trigger, Trigger::Resume);
        assert_eq!(t.new.as_ref().unwrap().command, "sleep 30");
        apply_transition(&t, &manager, &mut buffers, &mut ui);
        assert!(!manager.is_override(0));
        assert!(!ui.override_marked(0));
        assert_eq!(ui.health(0), Health::Running);
        manager.shutdown();
    }

    #[test]
    fn a_change_does_not_touch_a_running_override() {
        let (tx, rx) = mpsc::channel();
        let config = Config { grace_period: Duration::from_millis(200), ..Config::default() };
        let mut manager = ProcManager::spawn_all(&["sleep 30".to_string()], &config, tx.clone());
        let mut buffers = BufferSet::new(1, &config);
        let mut ui = UiState::new(vec!["sleep".to_string()]);
        assert!(manager.replace_with(0, "sleep 31".to_string(), GenKind::Override, Trigger::Cli("run".into())));
        tick_until(&mut manager, Duration::from_secs(5));

        tx.send(Event::Changed(Changed { proc: 0, paths: vec!["a".into()], more: 0 })).unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        assert!(!manager.is_restarting(0));
        assert_eq!(manager.current_command(0), "sleep 31");
        manager.shutdown();
    }
```

Add a test helper in `main.rs` tests (the existing `stale_generation…` test has the loop inline; factor it):

```rust
    fn tick_until(manager: &mut ProcManager, limit: Duration) -> Transition {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(t) = manager.tick().pop() {
                return t;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no transition within {limit:?}");
    }
```

and `UiState::override_marked(&self, proc) -> bool` (test-visible accessor, `#[allow(dead_code)] // test-only accessor` like `health`). Import `GenKind`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q override_slots && cargo test -q an_override_that_exits`
Expected: compile errors — `set_override`, `set_control`, `status_markers`, `override_marked` not found.

- [ ] **Step 3: Implement**

`src/ui.rs`: fields `overrides: Vec<bool>` (init false) and `control: Option<bool>` (init `None`); setters; `override_marked`; in `slot_label` append `*` to the name span's text when `self.overrides[p]` (i.e. `format!("{name}*")`, same style); factor the tail of `render_status_bar` into

```rust
    /// The right-hand markers of the status bar: FOLLOW/SCROLL, WRAP, and the
    /// control-socket state.
    pub fn status_markers(&self) -> Vec<Span<'static>> {
        let mut spans = vec![if self.following() {
            Span::styled(" FOLLOW", Style::default().fg(Color::Green))
        } else {
            Span::styled(" SCROLL", Style::default().fg(Color::Yellow))
        }];
        if self.wrap {
            spans.push(Span::styled(" WRAP", Style::default().add_modifier(Modifier::DIM)));
        }
        match self.control {
            Some(true) => spans.push(Span::styled(" CTRL", Style::default().add_modifier(Modifier::DIM))),
            Some(false) => spans.push(Span::styled(" NO CTRL", Style::default().fg(Color::Red))),
            None => {}
        }
        spans
    }
```

and `render_status_bar` does `spans.extend(self.status_markers());`.

`src/main.rs`:

- `drain_events`, `Event::Exited` arm:
  ```rust
                if manager.is_current(proc, r#gen) && !manager.is_restarting(proc) {
                    if manager.is_override(proc) {
                        // The one-shot command is done; the standard one resumes.
                        let standard = manager.standard_command(proc).to_string();
                        if manager.replace_with(proc, standard, GenKind::Standard, Trigger::Resume) {
                            ui.set_health(proc, Health::Restarting);
                        }
                    } else {
                        ui.set_health(proc, health_from_exit(status));
                    }
                }
  ```
- `Event::Changed` arm: after the in-flight check add `if manager.is_override(changed.proc) { continue; } // overrides are pinned`.
- `apply_transition` ends with `ui.set_override(t.proc, manager.is_override(t.proc));`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 137 passed, no warnings (`set_control` gets its caller in Task 6).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/main.rs src/ui.rs
git commit -m "Resume overrides on exit, pin them against watches, mark them in the bar"
```

---

### Task 4: The wire protocol

**Files:**
- Create: `src/protocol.rs`
- Modify: `Cargo.toml` (`serde_json = "1"`), `src/main.rs` (`mod protocol;`)

**Interfaces:**
- Produces (all `Serialize + Deserialize + Debug + Clone + PartialEq`):
  ```rust
  pub const PROTOCOL_VERSION: u32 = 1;
  pub struct Envelope { pub v: u32, #[serde(flatten)] pub request: Request }
  #[serde(tag = "cmd", rename_all = "lowercase")]
  pub enum Request {
      Status,
      Restart { slot: String, #[serde(default)] wait: bool },
      Kill    { slot: String, #[serde(default)] wait: bool },
      Stop    { slot: String, #[serde(default)] wait: bool },
      Start   { slot: String, #[serde(default)] wait: bool },
      Run     { slot: String, #[serde(default)] cmd: Vec<String>, #[serde(default)] wrap: Option<String>, #[serde(default)] wait: bool },
      Quit,
      Logs    { #[serde(default)] slot: Option<String>, #[serde(default = "default_tail")] tail: usize, #[serde(default)] since_ms: Option<u64>, #[serde(default)] color: bool },
  }
  #[serde(untagged)]
  pub enum Response {
      Error  { ok: bool, error: String },
      Status { ok: bool, pid: u32, dir: String, procs: Vec<ProcStatus> },
      Logs   { ok: bool, lines: Vec<LogLine> },
      Acted  { ok: bool, started: Vec<Started>, skipped: Vec<Skipped>, #[serde(default, skip_serializing_if = "Option::is_none")] markers: Option<Vec<String>> },
      Done   { ok: bool },
  }
  pub struct ProcStatus { pub index: usize, pub name: String, pub health: String, pub r#gen: u32, pub pid: Option<i32>, pub command: String, pub standard: String, pub r#override: bool, pub since_ms: Option<u64> }
  pub struct Started { pub proc: usize, pub name: String, pub from_gen: Option<u32> }
  pub struct Skipped { pub proc: usize, pub name: String, pub reason: String }
  pub struct LogLine { pub seq: u64, pub at_ms: u64, pub r#gen: u32, pub proc: usize, pub name: String, pub stream: String, pub text: String }
  impl Response { pub fn error(msg: impl Into<String>) -> Response; pub fn done() -> Response; }
  ```
  (`r#override` and `r#gen` serialize as `override`/`gen` — serde strips the `r#`.)

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_with_defaults() {
        let e: Envelope = serde_json::from_str(r#"{"v":1,"cmd":"restart","slot":"server"}"#).unwrap();
        assert_eq!(e.v, 1);
        assert_eq!(e.request, Request::Restart { slot: "server".into(), wait: false });

        let e: Envelope = serde_json::from_str(r#"{"v":1,"cmd":"logs"}"#).unwrap();
        assert_eq!(e.request, Request::Logs { slot: None, tail: 100, since_ms: None, color: false });

        let e: Envelope = serde_json::from_str(r#"{"v":1,"cmd":"run","slot":"server","wrap":"perf record -g","wait":true}"#).unwrap();
        assert_eq!(e.request, Request::Run { slot: "server".into(), cmd: vec![], wrap: Some("perf record -g".into()), wait: true });

        let text = serde_json::to_string(&Envelope { v: 1, request: Request::Status }).unwrap();
        assert_eq!(text, r#"{"v":1,"cmd":"status"}"#);

        assert!(serde_json::from_str::<Envelope>(r#"{"v":1,"cmd":"dance"}"#).is_err());
    }

    #[test]
    fn responses_round_trip_and_untagged_order_is_unambiguous() {
        let cases = vec![
            Response::error("nope"),
            Response::Status { ok: true, pid: 7, dir: "/p".into(), procs: vec![ProcStatus { index: 1, name: "a".into(), health: "running".into(), r#gen: 2, pid: Some(3), command: "x".into(), standard: "x".into(), r#override: false, since_ms: Some(10) }] },
            Response::Logs { ok: true, lines: vec![LogLine { seq: 1, at_ms: 2, r#gen: 0, proc: 0, name: "a".into(), stream: "stdout".into(), text: "hi".into() }] },
            Response::Acted { ok: true, started: vec![Started { proc: 0, name: "a".into(), from_gen: Some(1) }], skipped: vec![], markers: None },
            Response::Acted { ok: true, started: vec![], skipped: vec![Skipped { proc: 1, name: "b".into(), reason: "restart in flight".into() }], markers: Some(vec!["── x ──".into()]) },
            Response::done(),
        ];
        for r in cases {
            let text = serde_json::to_string(&r).unwrap();
            let back: Response = serde_json::from_str(&text).unwrap();
            assert_eq!(back, r, "{text}");
        }
        let text = serde_json::to_string(&Response::error("nope")).unwrap();
        assert_eq!(text, r#"{"ok":false,"error":"nope"}"#);
        let text = serde_json::to_string(&Response::Status { ok: true, pid: 1, dir: "d".into(), procs: vec![] }).unwrap();
        assert!(text.contains(r#""override""#) || !text.contains("r#"), "{text}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q protocol::`
Expected: compile error — module/types missing.

- [ ] **Step 3: Implement**

`Cargo.toml`: `serde_json = "1"`. `src/main.rs`: `mod protocol;`. `src/protocol.rs`: module doc (`//! The line-JSON protocol spoken over the control socket. Pure data.`), the types above with doc comments, `fn default_tail() -> usize { 100 }`, and:

```rust
impl Response {
    pub fn error(message: impl Into<String>) -> Response {
        Response::Error { ok: false, error: message.into() }
    }
    pub fn done() -> Response {
        Response::Done { ok: true }
    }
}
```

Untagged order in the enum must be exactly `Error, Status, Logs, Acted, Done` so that deserialization picks by distinguishing fields (`error`, `procs`, `lines`, `started`).

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 139 passed, no warnings (module-level `#![allow(dead_code)] // wired in by a later task` permitted until Task 5).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/protocol.rs src/main.rs
git commit -m "Add the control protocol types"
```

---

### Task 5: `control.rs` — socket path, listener, request handler

**Files:**
- Create: `src/control.rs`
- Modify: `src/types.rs` (`Event::Control`), `src/main.rs` (`mod control;`, `Event::Control(..) => {}` placeholder arm until Task 6)

**Interfaces:**
- Produces:
  ```rust
  pub fn fnv1a64(bytes: &[u8]) -> u64;
  pub fn runtime_dir() -> PathBuf;                 // $XDG_RUNTIME_DIR/krawatte or /tmp/krawatte-<uid>
  pub fn socket_path(project_dir: &Path) -> PathBuf;   // runtime_dir()/<hex>.sock
  pub enum BindError { AnotherInstance(PathBuf), Io(PathBuf, io::Error) }
  pub struct Listener { … }    // Drop unlinks
  impl Listener { pub fn bind(path: &Path) -> Result<Listener, BindError>; pub fn path(&self) -> &Path; pub fn serve(&mut self, tx: Sender<Event>); }
  pub struct Ctx<'a> { pub manager: &'a mut ProcManager, pub buffers: &'a BufferSet, pub ui: &'a mut UiState, pub project_dir: &'a Path }
  pub enum Handled { Now(Response), AfterTransitions { procs: HashSet<ProcId>, partial: Response }, Quit(Response) }
  pub fn handle(request: &Request, ctx: Ctx<'_>) -> Handled;
  pub fn resolve_slot(manager: &ProcManager, slot: &str) -> Result<Vec<ProcId>, String>;
  pub fn parse_duration(s: &str) -> Result<Duration, String>;   // "30s" "5m" "1h30m"
  pub fn health_text(h: Health) -> String;
  ```
- `types.rs`: `Event::Control { request: Request, reply: Sender<Response> }` (doc: `/// A request from the control socket; answer on \`reply\`.`).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::GenKind;
    use crate::types::Trigger;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn fnv_and_socket_path_are_stable_and_short() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        let a = socket_path(Path::new("/home/c/Projects/erhebimus"));
        let b = socket_path(Path::new("/home/c/Projects/erhebimus"));
        let c = socket_path(Path::new("/home/c/Projects/krawatte"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.to_string_lossy().ends_with(".sock"));
        assert!(a.as_os_str().len() < 100, "{a:?}");
        assert!(a.starts_with(runtime_dir()));
    }

    #[test]
    fn bind_replaces_a_stale_socket_but_yields_to_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.sock");
        let first = Listener::bind(&path).unwrap();
        assert!(path.exists());
        match Listener::bind(&path) {
            Err(BindError::AnotherInstance(p)) => assert_eq!(p, path),
            other => panic!("expected AnotherInstance, got {other:?}"),
        }
        drop(first);
        assert!(!path.exists(), "dropping the listener unlinks the socket");

        // Stale: a socket file nobody listens on.
        std::os::unix::net::UnixListener::bind(&path).map(drop).unwrap();
        assert!(path.exists());
        let again = Listener::bind(&path).expect("stale socket is replaced");
        assert_eq!(again.path(), path);
    }

    #[test]
    fn serve_forwards_requests_and_writes_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.sock");
        let mut listener = Listener::bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Event>();
        listener.serve(tx);

        // Answer one request from a fake main loop.
        let answerer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let Event::Control { request, reply } = ev else { panic!("not a control event") };
            assert_eq!(request, Request::Status);
            reply.send(Response::done()).unwrap();
        });
        let mut s = UnixStream::connect(&path).unwrap();
        s.write_all(b"{\"v\":1,\"cmd\":\"status\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), r#"{"ok":true}"#);
        answerer.join().unwrap();

        // Malformed and wrong-version requests are refused without reaching main.
        for bad in ["not json\n", "{\"v\":2,\"cmd\":\"status\"}\n"] {
            let mut s = UnixStream::connect(&path).unwrap();
            s.write_all(bad.as_bytes()).unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            let r: Response = serde_json::from_str(&line).unwrap();
            assert!(matches!(r, Response::Error { .. }), "{line}");
        }
    }

    #[test]
    fn parse_duration_accepts_units_and_rejects_bare_numbers() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
    }

    // --- handle() against a real manager ----------------------------------

    struct World {
        manager: ProcManager,
        buffers: BufferSet,
        ui: UiState,
        rx: mpsc::Receiver<Event>,
        dir: PathBuf,
    }

    fn world(cmds: &[&str]) -> World {
        let (tx, rx) = mpsc::channel();
        let config = crate::types::Config { grace_period: Duration::from_millis(200), ..Default::default() };
        let cmds: Vec<String> = cmds.iter().map(|s| s.to_string()).collect();
        let manager = ProcManager::spawn_all(&cmds, &config, tx);
        let names = (0..manager.len()).map(|p| manager.short_name(p).to_string()).collect();
        World { manager, buffers: BufferSet::new(cmds.len(), &config), ui: UiState::new(names), rx, dir: PathBuf::from("/p") }
    }

    impl World {
        fn handle(&mut self, r: Request) -> Handled {
            handle(&r, Ctx { manager: &mut self.manager, buffers: &self.buffers, ui: &mut self.ui, project_dir: &self.dir })
        }
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if !self.manager.tick().is_empty() { return; }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("no transition");
        }
        fn drain(&mut self) {
            for ev in self.rx.try_iter() {
                if let Event::Line { proc, r#gen, stream, seq, at, bytes } = ev
                    && self.manager.is_current(proc, r#gen)
                {
                    self.buffers.push(StyledLine::parse(proc, r#gen, stream, seq, at, &bytes));
                }
            }
        }
    }

    fn now(h: Handled) -> Response {
        match h { Handled::Now(r) => r, other => panic!("expected Now, got {other:?}") }
    }

    #[test]
    fn resolve_slot_by_name_index_and_all() {
        let w = world(&["sleep 30", "sleep 31"]);
        assert_eq!(resolve_slot(&w.manager, "sleep").unwrap(), vec![0], "first name match");
        assert_eq!(resolve_slot(&w.manager, "2").unwrap(), vec![1]);
        assert_eq!(resolve_slot(&w.manager, "all").unwrap(), vec![0, 1]);
        let err = resolve_slot(&w.manager, "web").unwrap_err();
        assert!(err.contains("unknown slot \"web\""), "{err}");
        assert!(err.contains("sleep"), "lists the slots: {err}");
        assert!(resolve_slot(&w.manager, "3").is_err());
        assert!(resolve_slot(&w.manager, "0").is_err());
        w.manager.shutdown_owned();
    }

    #[test]
    fn status_reports_every_slot() {
        let mut w = world(&["sleep 30", "exit 3"]);
        std::thread::sleep(Duration::from_millis(100));
        // Mirror what the main loop would do for the exited slot.
        w.ui.set_health(1, Health::ExitedErr(crate::types::ExitStatus::Code(3)));
        let Response::Status { ok, pid, dir, procs } = now(w.handle(Request::Status)) else { panic!() };
        assert!(ok);
        assert_eq!(pid, std::process::id());
        assert_eq!(dir, "/p");
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0].index, 1);
        assert_eq!(procs[0].health, "running");
        assert!(procs[0].pid.is_some());
        assert_eq!(procs[1].health, "exit 3");
        assert_eq!(procs[1].pid, None);
        assert!(!procs[0].r#override);
        w.manager.shutdown();
    }

    #[test]
    fn restart_all_skips_in_flight_slots_and_wait_collects_transitions() {
        let mut w = world(&["sleep 30", "sleep 31"]);
        assert!(w.manager.replace(1, "sleep 31".into(), Trigger::Key('r')));
        let h = w.handle(Request::Restart { slot: "all".into(), wait: true });
        let Handled::AfterTransitions { procs, partial } = h else { panic!("{h:?}") };
        assert_eq!(procs, HashSet::from([0]));
        let Response::Acted { started, skipped, .. } = partial else { panic!() };
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].proc, 0);
        assert_eq!(started[0].from_gen, Some(0));
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].proc, 1);
        assert_eq!(skipped[0].reason, "restart in flight");
        assert_eq!(w.ui.health(0), Health::Restarting);
        w.settle();
        w.manager.shutdown();
    }

    #[test]
    fn restart_all_with_everything_in_flight_is_an_error() {
        let mut w = world(&["sleep 30"]);
        assert!(w.manager.replace(0, "sleep 30".into(), Trigger::Key('r')));
        let r = now(w.handle(Request::Restart { slot: "all".into(), wait: false }));
        assert!(matches!(r, Response::Error { .. }), "{r:?}");
        w.settle();
        w.manager.shutdown();
    }

    #[test]
    fn stop_start_kill_and_run_apply_the_right_primitives() {
        let mut w = world(&["sleep 30"]);
        let r = now(w.handle(Request::Stop { slot: "1".into(), wait: false }));
        assert!(matches!(r, Response::Acted { .. }));
        w.settle();
        assert!(w.manager.is_dead(0));

        let r = now(w.handle(Request::Stop { slot: "1".into(), wait: false }));
        let Response::Acted { skipped, .. } = r else { panic!() };
        assert_eq!(skipped[0].reason, "already stopped");

        let r = now(w.handle(Request::Start { slot: "sleep".into(), wait: false }));
        assert!(matches!(r, Response::Acted { .. }));
        w.settle();
        assert!(!w.manager.is_dead(0));
        let Response::Acted { skipped, .. } = now(w.handle(Request::Start { slot: "1".into(), wait: false })) else { panic!() };
        assert_eq!(skipped[0].reason, "already running");

        let r = now(w.handle(Request::Run { slot: "1".into(), cmd: vec![], wrap: Some("env FOO=1".into()), wait: false }));
        assert!(matches!(r, Response::Acted { .. }), "{r:?}");
        w.settle();
        assert!(w.manager.is_override(0));
        assert_eq!(w.manager.current_command(0), "env FOO=1 sleep 30");
        assert!(w.ui.override_marked(0) || true, "the bar is updated by apply_transition in main, not here");

        let r = now(w.handle(Request::Run { slot: "all".into(), cmd: vec!["x".into()], wrap: None, wait: false }));
        assert!(matches!(r, Response::Error { .. }), "run needs one slot");
        let r = now(w.handle(Request::Run { slot: "1".into(), cmd: vec![], wrap: None, wait: false }));
        assert!(matches!(r, Response::Error { .. }), "run needs cmd or wrap");

        let r = now(w.handle(Request::Kill { slot: "1".into(), wait: false }));
        assert!(matches!(r, Response::Acted { .. }));
        w.settle();
        assert!(!w.manager.is_override(0));
        assert_eq!(w.manager.current_command(0), "sleep 30");
        w.manager.shutdown();
    }

    #[test]
    fn logs_tail_since_color_and_all() {
        let mut w = world(&["printf 'a\\n\\033[31mb\\033[0m\\n'", "echo c"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while (w.buffers.buffer(0).len() < 2 || w.buffers.buffer(1).len() < 1) && Instant::now() < deadline {
            w.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        let Response::Logs { lines, .. } = now(w.handle(Request::Logs { slot: Some("1".into()), tail: 100, since_ms: None, color: false })) else { panic!() };
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(lines[0].name, "printf");
        assert_eq!(lines[0].stream, "stdout");

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs { slot: Some("1".into()), tail: 1, since_ms: None, color: true })) else { panic!() };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "\x1b[31mb\x1b[0m");

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs { slot: None, tail: 100, since_ms: None, color: false })) else { panic!() };
        assert_eq!(lines.len(), 3);
        assert!(lines.windows(2).all(|p| p[0].seq < p[1].seq), "all = arrival order");

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs { slot: None, tail: 100, since_ms: Some(0), color: false })) else { panic!() };
        assert!(lines.is_empty(), "since 0 ms ago excludes everything");

        assert!(matches!(now(w.handle(Request::Logs { slot: Some("nope".into()), tail: 1, since_ms: None, color: false })), Response::Error { .. }));
        w.manager.shutdown();
    }

    #[test]
    fn quit_is_reported_as_such() {
        let mut w = world(&["sleep 30"]);
        assert!(matches!(w.handle(Request::Quit), Handled::Quit(Response::Done { ok: true })));
        w.manager.shutdown();
    }
}
```

Note: `resolve_slot_by_name_index_and_all` ends with `w.manager.shutdown_owned()` — replace that with `let mut w = world(..); …; w.manager.shutdown();` (make `w` mutable). The `assert!(... || true …)` line in `stop_start_kill_and_run…` documents where the bar is updated; delete it rather than keep a tautology.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q control::`
Expected: compile error — module missing.

- [ ] **Step 3: Implement**

`src/types.rs`: `Event::Control { request: crate::protocol::Request, reply: std::sync::mpsc::Sender<crate::protocol::Response> }`. (`Event` derives `Debug`; `Sender` is `Debug`, `Request` derives it.) `main.rs` `drain_events`: `Event::Control { .. } => {} // routed in a later task`.

`src/control.rs`:

```rust
//! The control socket: where it lives, how it is bound and served, and how
//! a request is answered. `handle` is pure given the manager and is the
//! unit-test surface; the socket code is a thin threaded shell around it.

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::buffer::{BufferSet, StyledLine};
use crate::config::FILE_NAME;
use crate::proc::{GenKind, ProcManager};
use crate::protocol::{Envelope, LogLine, ProcStatus, Request, Response, Skipped, Started, PROTOCOL_VERSION};
use crate::types::{Event, ExitStatus, Health, ProcId, StreamTag, Trigger};
use crate::ui::UiState;

/// 64-bit FNV-1a. Stable across builds and platforms (unlike `DefaultHasher`),
/// which is what a filename derived from a path needs.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `$XDG_RUNTIME_DIR/krawatte`, or `/tmp/krawatte-<uid>` when the runtime
/// dir is unset or missing. Neither is inside any project tree.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_dir() => dir.join("krawatte"),
        _ => PathBuf::from(format!("/tmp/krawatte-{}", nix::unistd::getuid().as_raw())),
    }
}

/// The socket for a project: hashed so the path stays well under the
/// 108-byte `sockaddr_un` limit however deep the project lives.
pub fn socket_path(project_dir: &Path) -> PathBuf {
    let key = project_dir.canonicalize().unwrap_or_else(|_| project_dir.to_path_buf());
    runtime_dir().join(format!("{:016x}.sock", fnv1a64(key.as_os_str().as_encoded_bytes())))
}

#[derive(Debug)]
pub enum BindError {
    /// Something is listening there already: another krawatte for this project.
    AnotherInstance(PathBuf),
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AnotherInstance(p) => write!(f, "another krawatte is already listening on {}", p.display()),
            BindError::Io(p, e) => write!(f, "cannot bind control socket {}: {e}", p.display()),
        }
    }
}

/// A bound control socket. Unlinked on drop, including the panic path.
#[derive(Debug)]
pub struct Listener {
    path: PathBuf,
    listener: Option<UnixListener>,
}

impl Listener {
    /// Bind `path`, creating its directory `0700` and the socket `0600`. A
    /// path that is already taken is probed: a live listener means another
    /// instance (yield to it); a dead one is a leftover from a crash and is
    /// replaced.
    pub fn bind(path: &Path) -> Result<Listener, BindError> {
        let io = |e: io::Error| BindError::Io(path.to_path_buf(), e);
        if let Some(dir) = path.parent() {
            let mut b = fs::DirBuilder::new();
            b.recursive(true).mode(0o700);
            b.create(dir).map_err(io)?;
        }
        let listener = match UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                if UnixStream::connect(path).is_ok() {
                    return Err(BindError::AnotherInstance(path.to_path_buf()));
                }
                fs::remove_file(path).map_err(io)?;
                UnixListener::bind(path).map_err(io)?
            }
            Err(e) => return Err(io(e)),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io)?;
        Ok(Listener { path: path.to_path_buf(), listener: Some(listener) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept connections on a detached thread. Each connection gets its own
    /// thread that reads one request, forwards it as [`Event::Control`], waits
    /// for the reply and writes it. Nothing here touches manager state.
    pub fn serve(&mut self, tx: Sender<Event>) {
        let Some(listener) = self.listener.take() else { return };
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { break };
                let tx = tx.clone();
                std::thread::spawn(move || serve_one(stream, &tx));
            }
        });
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// How long a connection thread waits for the main loop to answer. Generous:
/// a `--wait` on a slot with a long grace period is legitimate.
const REPLY_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest request line accepted.
const MAX_REQUEST: u64 = 64 * 1024;

fn serve_one(mut stream: UnixStream, tx: &Sender<Event>) {
    let mut line = String::new();
    let response = match BufReader::new(&stream).take(MAX_REQUEST).read_line(&mut line) {
        Err(e) => Response::error(format!("read: {e}")),
        Ok(_) => match serde_json::from_str::<Envelope>(&line) {
            Err(e) => Response::error(format!("bad request: {e}")),
            Ok(env) if env.v != PROTOCOL_VERSION => Response::error(format!("unsupported protocol version {} (this krawatte speaks {PROTOCOL_VERSION})", env.v)),
            Ok(env) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(Event::Control { request: env.request, reply: reply_tx }).is_err() {
                    Response::error("krawatte is shutting down")
                } else {
                    match reply_rx.recv_timeout(REPLY_TIMEOUT) {
                        Ok(r) => r,
                        Err(_) => Response::error("no reply from krawatte (it may have exited)"),
                    }
                }
            }
        },
    };
    if let Ok(mut text) = serde_json::to_string(&response) {
        text.push('\n');
        let _ = stream.write_all(text.as_bytes());
        let _ = stream.flush();
    }
}

// --- request handling --------------------------------------------------------

/// What `handle` needs from the main loop.
pub struct Ctx<'a> {
    pub manager: &'a mut ProcManager,
    pub buffers: &'a BufferSet,
    pub ui: &'a mut UiState,
    pub project_dir: &'a Path,
}

/// The outcome of a request: answer now, answer once these slots have
/// transitioned (the main loop appends their marker blocks), or quit.
#[derive(Debug)]
pub enum Handled {
    Now(Response),
    AfterTransitions { procs: HashSet<ProcId>, partial: Response },
    Quit(Response),
}

/// Slots named by `slot`: `all`, a 1-based index, or a name.
pub fn resolve_slot(manager: &ProcManager, slot: &str) -> Result<Vec<ProcId>, String> {
    let n = manager.len();
    if slot == "all" {
        return Ok((0..n).collect());
    }
    if let Ok(i) = slot.parse::<usize>() {
        return if (1..=n).contains(&i) { Ok(vec![i - 1]) } else { Err(format!("slot index {i} out of range (1-{n})")) };
    }
    if let Some(p) = (0..n).find(|&p| manager.short_name(p) == slot) {
        return Ok(vec![p]);
    }
    let names: Vec<&str> = (0..n).map(|p| manager.short_name(p)).collect();
    Err(format!("unknown slot {slot:?}; slots are: {} (1-{n})", names.join(", ")))
}

/// `30s`, `5m`, `1h30m`, `250ms`. A bare number is rejected rather than
/// guessed at.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration {s:?}: use units like 30s, 5m, 1h30m");
    if s.is_empty() { return Err(bad()); }
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() { num.push(c); continue; }
        let value: u64 = num.parse().map_err(|_| bad())?;
        num.clear();
        let unit = if c == 'm' && chars.peek() == Some(&'s') { chars.next(); "ms" } else { match c { 's' => "s", 'm' => "m", 'h' => "h", _ => return Err(bad()) } };
        total += match unit { "ms" => Duration::from_millis(value), "s" => Duration::from_secs(value), "m" => Duration::from_secs(value * 60), _ => Duration::from_secs(value * 3600) };
    }
    if !num.is_empty() { return Err(bad()); }
    Ok(total)
}

/// Health as the CLI prints it.
pub fn health_text(h: Health) -> String {
    match h {
        Health::Running => "running".into(),
        Health::Restarting => "restarting".into(),
        Health::ExitedOk => "exit 0".into(),
        Health::ExitedErr(ExitStatus::Code(c)) => format!("exit {c}"),
        Health::ExitedErr(ExitStatus::Signal(s)) => format!("signal {s}"),
        Health::SpawnFailed => "spawn failed".into(),
    }
}

pub fn handle(request: &Request, ctx: Ctx<'_>) -> Handled {
    match request {
        Request::Status => Handled::Now(status(&ctx)),
        Request::Quit => Handled::Quit(Response::done()),
        Request::Logs { slot, tail, since_ms, color } => Handled::Now(logs(&ctx, slot.as_deref(), *tail, *since_ms, *color)),
        Request::Restart { slot, wait } => act(ctx, slot, *wait, "restart", |m, p| {
            let cmd = m.current_command(p).to_string();
            m.replace(p, cmd, Trigger::Cli("restart".into())).then_some(()).ok_or("restart in flight")
        }),
        Request::Kill { slot, wait } => act(ctx, slot, *wait, "kill", |m, p| {
            m.kill(p, Trigger::Cli("kill".into())).then_some(()).ok_or("restart in flight")
        }),
        Request::Stop { slot, wait } => act(ctx, slot, *wait, "stop", |m, p| {
            if m.is_restarting(p) { return Err("restart in flight"); }
            if m.is_dead(p) { return Err("already stopped"); }
            m.stop(p, Trigger::Cli("stop".into())).then_some(()).ok_or("restart in flight")
        }),
        Request::Start { slot, wait } => act(ctx, slot, *wait, "start", |m, p| {
            if m.is_restarting(p) { return Err("restart in flight"); }
            if !m.is_dead(p) { return Err("already running"); }
            let std_cmd = m.standard_command(p).to_string();
            m.replace_with(p, std_cmd, GenKind::Standard, Trigger::Cli("start".into())).then_some(()).ok_or("restart in flight")
        }),
        Request::Run { slot, cmd, wrap, wait } => {
            if slot == "all" { return Handled::Now(Response::error("run takes a single slot, not all")); }
            let command = match (cmd.is_empty(), wrap) {
                (false, None) => cmd.join(" "),
                (true, Some(prefix)) => {
                    let Ok(procs) = resolve_slot(ctx.manager, slot) else { return Handled::Now(Response::error(resolve_slot(ctx.manager, slot).unwrap_err())) };
                    format!("{prefix} {}", ctx.manager.standard_command(procs[0]))
                }
                _ => return Handled::Now(Response::error("run needs exactly one of a command (after --) or --wrap")),
            };
            act(ctx, slot, *wait, "run", move |m, p| {
                m.replace_with(p, command.clone(), GenKind::Override, Trigger::Cli("run".into())).then_some(()).ok_or("restart in flight")
            })
        }
    }
}

fn status(ctx: &Ctx<'_>) -> Response {
    let procs = (0..ctx.manager.len()).map(|p| {
        let info = ctx.manager.snapshot(p);
        ProcStatus {
            index: info.index,
            name: info.name,
            health: health_text(ctx.ui.health(p)),
            r#gen: info.r#gen,
            pid: info.pid,
            command: info.command,
            standard: info.standard,
            r#override: info.kind == GenKind::Override,
            since_ms: info.since.map(|d| d.as_millis() as u64),
        }
    }).collect();
    Response::Status { ok: true, pid: std::process::id(), dir: ctx.project_dir.display().to_string(), procs }
}

/// Apply `op` to every slot `slot` names. Slots the op refuses are listed
/// as skipped; the reply is an error only if nothing was started.
fn act(ctx: Ctx<'_>, slot: &str, wait: bool, verb: &str, mut op: impl FnMut(&mut ProcManager, ProcId) -> Result<(), &'static str>) -> Handled {
    let targets = match resolve_slot(ctx.manager, slot) {
        Ok(t) => t,
        Err(e) => return Handled::Now(Response::error(e)),
    };
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    for p in targets {
        let name = ctx.manager.short_name(p).to_string();
        let from_gen = ctx.manager.was_started(p).then(|| ctx.manager.current_gen(p));
        match op(ctx.manager, p) {
            Ok(()) => {
                ctx.ui.set_health(p, Health::Restarting);
                started.push(Started { proc: p, name, from_gen });
            }
            Err(reason) => skipped.push(Skipped { proc: p, name, reason: reason.to_string() }),
        }
    }
    if started.is_empty() {
        let reasons: Vec<String> = skipped.iter().map(|s| format!("{} ({})", s.name, s.reason)).collect();
        return Handled::Now(Response::error(format!("{verb}: nothing to do: {}", reasons.join(", "))));
    }
    let partial = Response::Acted { ok: true, started: started.clone(), skipped, markers: None };
    if wait {
        Handled::AfterTransitions { procs: started.iter().map(|s| s.proc).collect(), partial }
    } else {
        Handled::Now(partial)
    }
}

fn logs(ctx: &Ctx<'_>, slot: Option<&str>, tail: usize, since_ms: Option<u64>, color: bool) -> Response {
    let lines: Vec<&StyledLine> = match slot {
        None | Some("all") => ctx.buffers.interleaved(),
        Some(s) => match resolve_slot(ctx.manager, s) {
            Ok(procs) => ctx.buffers.buffer(procs[0]).iter().collect(),
            Err(e) => return Response::error(e),
        },
    };
    let cutoff = since_ms.map(|ms| SystemTime::now() - Duration::from_millis(ms));
    let recent: Vec<&StyledLine> = lines.into_iter().filter(|l| cutoff.is_none_or(|c| l.at >= c)).collect();
    let start = recent.len().saturating_sub(tail);
    let out = recent[start..].iter().map(|l| LogLine {
        seq: l.seq,
        at_ms: l.at.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        r#gen: l.r#gen,
        proc: l.proc,
        name: ctx.manager.short_name(l.proc).to_string(),
        stream: match l.stream { StreamTag::Stdout => "stdout", StreamTag::Stderr => "stderr", StreamTag::Marker => "marker" }.to_string(),
        text: if color { String::from_utf8_lossy(&l.raw).into_owned() } else { l.plain() },
    }).collect();
    Response::Logs { ok: true, lines: out }
}
```

`FILE_NAME` import is unused here — drop it. If `as_encoded_bytes` is unavailable on your toolchain, use `key.to_string_lossy().as_bytes()`. In `logs`, `since_ms: Some(0)` yields `cutoff = now`, and lines stamped before it are excluded, as the test expects.

Remove Task 1/2/4's temporary allows on `raw`, `plain`, `snapshot`, `SlotInfo`, `is_override`, `kind`, `protocol` now that `control.rs` uses them.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 149 passed, no warnings (module-level allow on `control.rs` permitted until Task 6).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/control.rs src/types.rs src/main.rs
git commit -m "Add the control socket listener and request handler"
```

---

### Task 6: Route control events through the main loop

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- `drain_events(...) -> bool` (true = quit requested); `struct Waiter { outstanding: HashSet<ProcId>, reply: Sender<Response>, partial: Response, markers: Vec<String> }`; `run(specs, watched, config, tx, rx, control: Option<Listener>, project_dir: PathBuf)`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn control_requests_are_answered_and_wait_replies_carry_markers() {
        let (tx, rx) = mpsc::channel();
        let config = Config { grace_period: Duration::from_millis(200), ..Config::default() };
        let mut manager = ProcManager::spawn_all(&["sleep 30".to_string()], &config, tx.clone());
        let mut buffers = BufferSet::new(1, &config);
        let mut ui = UiState::new(vec!["sleep".to_string()]);
        let mut waiters = Vec::new();
        let dir = PathBuf::from("/p");

        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Event::Control { request: Request::Status, reply: reply_tx }).unwrap();
        assert!(!drain_events(&rx, &mut buffers, &mut ui, &mut manager, &mut waiters, &dir));
        assert!(matches!(reply_rx.try_recv().unwrap(), Response::Status { .. }));

        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Event::Control { request: Request::Restart { slot: "1".into(), wait: true }, reply: reply_tx }).unwrap();
        assert!(!drain_events(&rx, &mut buffers, &mut ui, &mut manager, &mut waiters, &dir));
        assert_eq!(waiters.len(), 1);
        assert!(reply_rx.try_recv().is_err(), "not answered before the transition");
        let t = tick_until(&mut manager, Duration::from_secs(5));
        apply_transition(&t, &manager, &mut buffers, &mut ui, &mut waiters);
        assert!(waiters.is_empty());
        let Response::Acted { markers: Some(markers), .. } = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap() else { panic!() };
        assert!(markers[0].contains("cli restart"), "{markers:?}");

        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Event::Control { request: Request::Quit, reply: reply_tx }).unwrap();
        assert!(drain_events(&rx, &mut buffers, &mut ui, &mut manager, &mut waiters, &dir), "quit requested");
        assert!(matches!(reply_rx.try_recv().unwrap(), Response::Done { ok: true }));
        manager.shutdown();
    }
```

Update the other `drain_events`/`apply_transition` calls in tests to the new signatures (`&mut Vec::new()` and `&PathBuf::from("/p")` are fine). Import `Request`, `Response`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q control_requests_are`
Expected: compile error — argument count mismatch.

- [ ] **Step 3: Implement**

```rust
/// A `--wait` reply parked until every slot it started has transitioned.
struct Waiter {
    outstanding: HashSet<ProcId>,
    reply: mpsc::Sender<Response>,
    partial: Response,
    markers: Vec<String>,
}
```

`drain_events(rx, buffers, ui, manager, waiters: &mut Vec<Waiter>, project_dir: &Path) -> bool`:

```rust
            Event::Control { request, reply } => {
                let ctx = control::Ctx { manager, buffers, ui, project_dir };
                match control::handle(&request, ctx) {
                    control::Handled::Now(r) => { let _ = reply.send(r); }
                    control::Handled::AfterTransitions { procs, partial } => waiters.push(Waiter { outstanding: procs, reply, partial, markers: Vec::new() }),
                    control::Handled::Quit(r) => { let _ = reply.send(r); quit = true; }
                }
            }
```

(`let mut quit = false;` at the top; `return quit` at the end; all other arms unchanged.) `apply_transition(t, manager, buffers, ui, waiters)`: after pushing the marker block (keep the `Vec<String>` from `restart_block` in a local first), run

```rust
    waiters.retain_mut(|w| {
        if !w.outstanding.remove(&t.proc) { return true; }
        w.markers.extend(block.iter().cloned());
        if !w.outstanding.is_empty() { return true; }
        if let Response::Acted { markers, .. } = &mut w.partial { *markers = Some(std::mem::take(&mut w.markers)); }
        let _ = w.reply.send(w.partial.clone());
        false
    });
```

`event_loop`: `if drain_events(...) { return Ok(()); }`; owns `let mut waiters: Vec<Waiter> = Vec::new();` and passes `project_dir`.

`run(specs, watched, config, tx, rx, control: Option<control::Listener>, project_dir: PathBuf)`: `ui.set_control(control.as_ref().map(|_| true))` — and `Some(false)` when `main` tried and failed (see below): make the parameter `control: ControlState` where `enum ControlState { Off, On(Listener), Failed(String) }`; `Off` → `set_control(None)`, `On` → `Some(true)`, `Failed` → `Some(false)`. The listener lives in `run`'s scope until after `manager.shutdown()` so the socket is unlinked after the children are gone (a client's `quit --wait` sees the connection close then). After the terminal is restored, if `Failed(msg)`, `eprintln!("krawatte: control socket unavailable: {msg}")`.

`main`: before the terminal,

```rust
    let project_dir = launch.project_dir.clone().unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let control = match control::Listener::bind(&control::socket_path(&project_dir)) {
        Ok(mut l) => { l.serve(tx.clone()); ControlState::On(l) }
        Err(e) => ControlState::Failed(e.to_string()),
    };
```

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 150 passed, no warnings. Remove any remaining temporary allows.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/main.rs
git commit -m "Serve control requests from the main loop"
```

---

### Task 7: The CLI client, README, smoke

**Files:**
- Create: `src/cli.rs`
- Modify: `src/main.rs` (`Cli` subcommand, dispatch), `README.md`

**Interfaces:**
- Produces: `pub enum Sub { Status {..}, Restart {..}, Kill {..}, Stop {..}, Start {..}, Run {..}, Quit {..}, Logs {..} }` (clap `Subcommand`); `pub fn run_client(sub: &Sub, file: Option<&Path>) -> i32`; `pub fn project_dir_for(file: Option<&Path>, cwd: &Path) -> Result<PathBuf, String>`; `pub fn request_for(sub: &Sub) -> Result<Request, String>`; `pub fn render(sub: &Sub, resp: &Response) -> String`; `pub fn exit_code(resp: &Response) -> i32`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ProcStatus, Skipped, Started};
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        sub: Sub,
    }
    fn sub(args: &[&str]) -> Sub {
        T::try_parse_from(std::iter::once("krawatte").chain(args.iter().copied())).unwrap().sub
    }

    #[test]
    fn subcommands_map_to_requests() {
        assert_eq!(request_for(&sub(&["status"])).unwrap(), Request::Status);
        assert_eq!(request_for(&sub(&["restart", "server", "--wait"])).unwrap(), Request::Restart { slot: "server".into(), wait: true });
        assert_eq!(request_for(&sub(&["stop", "all"])).unwrap(), Request::Stop { slot: "all".into(), wait: false });
        assert_eq!(
            request_for(&sub(&["run", "server", "--", "perf", "record", "-g", "target/debug/app"])).unwrap(),
            Request::Run { slot: "server".into(), cmd: vec!["perf".into(), "record".into(), "-g".into(), "target/debug/app".into()], wrap: None, wait: false }
        );
        assert_eq!(
            request_for(&sub(&["run", "server", "--wrap", "perf record -g"])).unwrap(),
            Request::Run { slot: "server".into(), cmd: vec![], wrap: Some("perf record -g".into()), wait: false }
        );
        assert!(request_for(&sub(&["run", "server"])).is_err(), "needs -- or --wrap");
        assert_eq!(
            request_for(&sub(&["logs", "server", "--tail", "5", "--since", "2m", "--color"])).unwrap(),
            Request::Logs { slot: Some("server".into()), tail: 5, since_ms: Some(120_000), color: true }
        );
        assert_eq!(request_for(&sub(&["logs"])).unwrap(), Request::Logs { slot: None, tail: 100, since_ms: None, color: false });
        assert!(request_for(&sub(&["logs", "--since", "5"])).is_err());
    }

    #[test]
    fn project_dir_prefers_file_then_discovery_then_cwd() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("p");
        let deep = project.join("a");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_dir_for(None, &deep).unwrap(), deep.canonicalize().unwrap(), "no file: cwd (ad-hoc instance)");
        std::fs::write(project.join(crate::config::FILE_NAME), "").unwrap();
        assert_eq!(project_dir_for(None, &deep).unwrap(), project.canonicalize().unwrap());
        let other = root.path().join("q");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("Krawattefile"), "").unwrap();
        assert_eq!(project_dir_for(Some(&other.join("Krawattefile")), &deep).unwrap(), other.canonicalize().unwrap());
        assert!(project_dir_for(Some(Path::new("/nonexistent/Krawattefile")), &deep).is_err());
    }

    #[test]
    fn human_rendering_and_exit_codes() {
        let status = Response::Status { ok: true, pid: 48001, dir: "/home/c/e".into(), procs: vec![
            ProcStatus { index: 1, name: "build".into(), health: "exit 0".into(), r#gen: 4, pid: None, command: "cargo build".into(), standard: "cargo build".into(), r#override: false, since_ms: Some(12_000) },
            ProcStatus { index: 2, name: "server".into(), health: "running".into(), r#gen: 3, pid: Some(48213), command: "perf record -g app".into(), standard: "app".into(), r#override: true, since_ms: Some(252_000) },
        ]};
        let text = render(&sub(&["status"]), &status);
        assert!(text.starts_with("krawatte 48001 · /home/c/e\n"), "{text}");
        assert!(text.contains("[1] build"), "{text}");
        assert!(text.contains("exit 0"), "{text}");
        assert!(text.contains("[2] server*"), "{text}");
        assert!(text.contains("pid 48213"), "{text}");
        assert!(text.contains("4m12s"), "{text}");
        assert!(text.contains("perf record -g app"), "{text}");
        assert_eq!(exit_code(&status), 0);

        let acted = Response::Acted { ok: true, started: vec![Started { proc: 0, name: "build".into(), from_gen: Some(4) }], skipped: vec![Skipped { proc: 1, name: "server".into(), reason: "restart in flight".into() }], markers: Some(vec!["── restart · gen 4 → 5 · x · cli restart ──".into()]) };
        let text = render(&sub(&["restart", "all", "--wait"]), &acted);
        assert!(text.contains("build: restarting (gen 4)"), "{text}");
        assert!(text.contains("skipped: server (restart in flight)"), "{text}");
        assert!(text.contains("── restart · gen 4 → 5"), "{text}");
        assert_eq!(exit_code(&acted), 0);

        let err = Response::error("unknown slot \"web\"");
        assert_eq!(render(&sub(&["restart", "web"]), &err), "krawatte: unknown slot \"web\"\n");
        assert_eq!(exit_code(&err), 1);

        let json = render(&sub(&["status", "--json"]), &status);
        assert!(json.starts_with('{') && json.ends_with('\n'), "{json}");
        serde_json::from_str::<Response>(json.trim()).unwrap();
    }

    #[test]
    fn log_lines_render_with_clock_and_name_for_all() {
        let logs = Response::Logs { ok: true, lines: vec![
            LogLine { seq: 1, at_ms: 0, r#gen: 0, proc: 0, name: "build".into(), stream: "stdout".into(), text: "hi".into() },
        ]};
        let single = render(&sub(&["logs", "build"]), &logs);
        assert!(single.ends_with(" hi\n"), "{single}");
        assert!(!single.contains("build│"), "{single}");
        let all = render(&sub(&["logs"]), &logs);
        assert!(all.contains("build│ hi"), "{all}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q cli::`
Expected: compile error — module missing.

- [ ] **Step 3: Implement**

`src/cli.rs`:

```rust
//! The command-line client: clap subcommands, the one-shot socket round
//! trip, and human-readable rendering of replies.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::config;
use crate::control::{parse_duration, socket_path};
use crate::protocol::{Envelope, LogLine, Request, Response, PROTOCOL_VERSION};

/// Control a running krawatte for this project.
#[derive(Debug, Clone, Subcommand, PartialEq)]
pub enum Sub {
    /// Show every slot: health, generation, pid, command.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Tear a slot down and run its current command again (SLOT or `all`).
    Restart {
        slot: String,
        /// Return once the new generation has spawned; print the marker block.
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Tear a slot down and run its standard command (ends an override).
    Kill { slot: String, #[arg(long)] wait: bool, #[arg(long)] json: bool },
    /// Tear a slot down and leave it stopped.
    Stop { slot: String, #[arg(long)] wait: bool, #[arg(long)] json: bool },
    /// Start a stopped slot's standard command.
    Start { slot: String, #[arg(long)] wait: bool, #[arg(long)] json: bool },
    /// Run a one-shot command in a slot; the standard command resumes when it exits.
    Run {
        slot: String,
        /// Prefix the standard command, e.g. --wrap "perf record -g".
        #[arg(long, conflicts_with = "cmd")]
        wrap: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
        /// The full command to run, after `--`.
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Shut the whole instance down, like pressing q.
    Quit { #[arg(long)] wait: bool, #[arg(long)] json: bool },
    /// Print recent output of a slot, or of all slots interleaved.
    Logs {
        slot: Option<String>,
        /// Newest N lines.
        #[arg(long, default_value_t = 100)]
        tail: usize,
        /// Only lines newer than this (30s, 5m, 1h30m).
        #[arg(long)]
        since: Option<String>,
        /// Keep ANSI colors instead of stripping them.
        #[arg(long)]
        color: bool,
        #[arg(long)]
        json: bool,
    },
}

impl Sub {
    fn json(&self) -> bool {
        match self {
            Sub::Status { json } | Sub::Quit { json, .. } | Sub::Logs { json, .. } => *json,
            Sub::Restart { json, .. } | Sub::Kill { json, .. } | Sub::Stop { json, .. } | Sub::Start { json, .. } | Sub::Run { json, .. } => *json,
        }
    }
    fn waits(&self) -> bool {
        matches!(self, Sub::Restart { wait: true, .. } | Sub::Kill { wait: true, .. } | Sub::Stop { wait: true, .. } | Sub::Start { wait: true, .. } | Sub::Run { wait: true, .. } | Sub::Quit { wait: true, .. })
    }
}

/// The protocol request for a subcommand.
pub fn request_for(sub: &Sub) -> Result<Request, String> {
    Ok(match sub {
        Sub::Status { .. } => Request::Status,
        Sub::Restart { slot, wait, .. } => Request::Restart { slot: slot.clone(), wait: *wait },
        Sub::Kill { slot, wait, .. } => Request::Kill { slot: slot.clone(), wait: *wait },
        Sub::Stop { slot, wait, .. } => Request::Stop { slot: slot.clone(), wait: *wait },
        Sub::Start { slot, wait, .. } => Request::Start { slot: slot.clone(), wait: *wait },
        Sub::Run { slot, wrap, wait, cmd, .. } => {
            if cmd.is_empty() && wrap.is_none() {
                return Err("run needs a command after `--` or --wrap PREFIX".into());
            }
            Request::Run { slot: slot.clone(), cmd: cmd.clone(), wrap: wrap.clone(), wait: *wait }
        }
        Sub::Quit { .. } => Request::Quit,
        Sub::Logs { slot, tail, since, color, .. } => Request::Logs {
            slot: slot.clone(),
            tail: *tail,
            since_ms: match since { Some(s) => Some(parse_duration(s)?.as_millis() as u64), None => None },
            color: *color,
        },
    })
}

/// The project an instance is keyed by: the given file's directory, else
/// the nearest Krawattefile's, else the cwd itself (an ad-hoc instance).
pub fn project_dir_for(file: Option<&Path>, cwd: &Path) -> Result<PathBuf, String> {
    let dir = match file {
        Some(f) => f.canonicalize().map_err(|e| format!("{}: {e}", f.display()))?.parent().map(Path::to_path_buf).ok_or_else(|| "file has no parent".to_string())?,
        None => match config::discover(cwd) {
            Some(f) => f.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.to_path_buf()),
            None => cwd.to_path_buf(),
        },
    };
    dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))
}

/// Exit status for a reply: 1 if the instance refused, else 0.
pub fn exit_code(resp: &Response) -> i32 {
    if matches!(resp, Response::Error { .. }) { 1 } else { 0 }
}

/// Connect, send, receive, print. Returns the process exit code.
pub fn run_client(sub: &Sub, file: Option<&Path>) -> i32 {
    let request = match request_for(sub) {
        Ok(r) => r,
        Err(e) => { eprintln!("krawatte: {e}"); return 2; }
    };
    let cwd = match std::env::current_dir() { Ok(d) => d, Err(e) => { eprintln!("krawatte: {e}"); return 2; } };
    let dir = match project_dir_for(file, &cwd) { Ok(d) => d, Err(e) => { eprintln!("krawatte: {e}"); return 2; } };
    let path = socket_path(&dir);
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => { eprintln!("krawatte: no krawatte running for {}", dir.display()); return 3; }
    };
    let timeout = if sub.waits() { Duration::from_secs(600) } else { Duration::from_secs(10) };
    let _ = stream.set_read_timeout(Some(timeout));
    let mut text = serde_json::to_string(&Envelope { v: PROTOCOL_VERSION, request }).expect("serializable");
    text.push('\n');
    if let Err(e) = stream.write_all(text.as_bytes()) { eprintln!("krawatte: send: {e}"); return 1; }
    let mut line = String::new();
    match BufReader::new(&stream).read_line(&mut line) {
        Ok(0) if matches!(sub, Sub::Quit { wait: true, .. }) => { println!("krawatte: instance exited"); return 0; }
        Ok(_) => {}
        Err(e) => { eprintln!("krawatte: no reply: {e}"); return 1; }
    }
    let resp: Response = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => { eprintln!("krawatte: bad reply: {e}: {line}"); return 1; }
    };
    let out = render(sub, &resp);
    if exit_code(&resp) == 0 { print!("{out}"); } else { eprint!("{out}"); }
    if let Sub::Quit { wait: true, .. } = sub {
        // Wait for the instance to go: the socket closes when it exits.
        let mut rest = String::new();
        let _ = BufReader::new(&stream).read_line(&mut rest);
    }
    exit_code(&resp)
}

/// Human (or JSON) rendering of a reply, newline-terminated.
pub fn render(sub: &Sub, resp: &Response) -> String {
    if sub.json() {
        let mut s = serde_json::to_string(resp).expect("serializable");
        s.push('\n');
        return s;
    }
    match resp {
        Response::Error { error, .. } => format!("krawatte: {error}\n"),
        Response::Done { .. } => "ok\n".to_string(),
        Response::Status { pid, dir, procs, .. } => {
            let mut out = format!("krawatte {pid} · {dir}\n");
            let name_w = procs.iter().map(|p| p.name.len() + usize::from(p.r#override)).max().unwrap_or(0);
            for p in procs {
                let name = if p.r#override { format!("{}*", p.name) } else { p.name.clone() };
                let state = match p.pid { Some(pid) => format!("{} pid {pid}", p.health), None => p.health.clone() };
                let since = p.since_ms.map(|ms| fmt_secs(ms / 1000)).unwrap_or_default();
                out.push_str(&format!("[{}] {:<name_w$}  {:<18} gen {:<3} {:<7} {}\n", p.index, name, state, p.r#gen, since, p.command));
            }
            out
        }
        Response::Acted { started, skipped, markers, .. } => {
            let verb = match sub { Sub::Kill { .. } => "killing", Sub::Stop { .. } => "stopping", Sub::Start { .. } => "starting", Sub::Run { .. } => "running override in", _ => "restarting" };
            let mut out = String::new();
            for s in started {
                match s.from_gen { Some(g) => out.push_str(&format!("{}: {verb} (gen {g})\n", s.name)), None => out.push_str(&format!("{}: {verb}\n", s.name)) }
            }
            for s in skipped { out.push_str(&format!("skipped: {} ({})\n", s.name, s.reason)); }
            if let Some(m) = markers { for line in m { out.push_str(line); out.push('\n'); } }
            out
        }
        Response::Logs { lines, .. } => {
            let all = matches!(sub, Sub::Logs { slot: None, .. } | Sub::Logs { slot: Some(ref s), .. } if s == "all");
            let tz = TimeZone::system();
            let mut out = String::new();
            for l in lines { out.push_str(&format_log_line(l, all, &tz)); out.push('\n'); }
            out
        }
    }
}

fn format_log_line(l: &LogLine, with_name: bool, tz: &TimeZone) -> String {
    let clock = Timestamp::from_millisecond(l.at_ms as i64)
        .map(|ts| ts.to_zoned(tz.clone()).strftime("%H:%M:%S").to_string())
        .unwrap_or_else(|_| "??:??:??".to_string());
    if with_name { format!("{clock} {}│ {}", l.name, l.text) } else { format!("{clock} {}", l.text) }
}

/// `12s`, `4m12s`, `2h02m` — same shape as the marker block's runtime.
fn fmt_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 { format!("{h}h{m:02}m") } else if m > 0 { format!("{m}m{s:02}s") } else { format!("{s}s") }
}
```

(`fmt_secs` duplicates `marker::fmt_duration`'s logic on purpose: the marker one takes a `Duration` and is private; making it `pub(crate) fn fmt_duration(Duration)` in `marker.rs` and calling it here is the better move — do that and delete `fmt_secs`.)

`src/main.rs`: `mod cli;` and on `Cli`:

```rust
    #[command(subcommand)]
    command: Option<cli::Sub>,
```

with `file` made `#[arg(short, long, value_name = "PATH", global = true, conflicts_with = "commands")]` so `krawatte -f PATH status` works. At the top of `main`, right after `Cli::parse()`:

```rust
    if let Some(sub) = &cli.command {
        std::process::exit(cli::run_client(sub, cli.file.as_deref()));
    }
```

If clap rejects the combination of a trailing-var-arg positional with subcommands, add `#[command(args_conflicts_with_subcommands = true)]` on `Cli` and keep `-f` global; verify with the existing `cli_accepts_no_arguments_and_rejects_file_with_commands` test plus a new one: `Cli::try_parse_from(["krawatte", "status"]).unwrap().command.is_some()` and `Cli::try_parse_from(["krawatte", "--", "status"]).unwrap().commands == ["status"]`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 155 passed, no warnings; no temporary allows left anywhere (`grep -rn "wired in by a later task" src` is empty).

- [ ] **Step 5: Manual smoke**

```bash
cargo build --release -q
d=$(mktemp -d); cat > "$d/Krawattefile" <<'EOF'
[settings]
timeout = 1
[[proc]]
name = "ticker"
cmd  = "while true; do echo tick; sleep 1; done"
[[proc]]
name = "napper"
cmd  = "sleep 100"
EOF
```

Start `krawatte` in `$d` via a pty helper (Python `pty`), then from a second shell in `$d/` (create a subdir and `cd` into it to prove discovery):

1. `krawatte status` — header line with pid and `$d`; two rows; `CTRL` visible in the TUI bar.
2. `krawatte restart ticker --wait` — prints `ticker: restarting (gen 0)` then the marker block with `cli restart`; the TUI shows the same block.
3. `krawatte logs ticker --tail 3` — three `HH:MM:SS tick`/marker lines; `--json` is one JSON object; `--color` identical here (no ANSI).
4. `krawatte run napper --wrap "env FOO=1" --wait` then `krawatte status` — `napper*`, command `env FOO=1 sleep 100`; `krawatte kill napper --wait` — `*` gone.
5. `krawatte run napper -- sh -c 'echo once; sleep 0.5'` — after ~1 s `krawatte logs napper` shows `once` and a `resume` marker; status shows `sleep 100` again, no `*`.
6. `krawatte stop all --wait`; status shows both dead (`signal 15`); `krawatte start all`; both running; `krawatte restart all` twice quickly — second reports both skipped and exits 1.
7. `krawatte restart nope` → exit 1 with the slot list; `cd /tmp && krawatte status` → exit 3 "no krawatte running for /tmp".
8. Start a second `krawatte` in `$d` (pty): its bar shows `NO CTRL`; quit it; the first still answers `status`.
9. `krawatte quit --wait` — returns after the TUI has exited; socket file gone from `$(echo ${XDG_RUNTIME_DIR:-/tmp/krawatte-$(id -u)})/krawatte/`.

- [ ] **Step 6: Document**

`README.md`: add after the Krawattefile section:

```markdown
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
command prints the raw reply, for scripts and agents.

`run` puts a one-shot **override** in a slot — `--wrap "perf record -g"`
prefixes the standard command, `-- cmd args` replaces it — in the slot's
working directory and environment. When it exits, the standard command
resumes; `k`/`kill` end it early; `r`/`restart` restart the override itself;
file watches leave it alone. The status bar marks an override slot with `*`.

The bar shows `CTRL` while the socket is up. If another krawatte already
serves this project, the new one runs with `NO CTRL`.

Exit codes: `0` ok, `1` refused (unknown slot, restart in flight), `2` usage,
`3` no instance running for this project.
```

Add `krawatte <subcommand>` to the Usage table and `*`/`CTRL` to the status-bar bullet.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/cli.rs src/main.rs src/marker.rs README.md
git commit -m "Add the krawatte control CLI"
```

---

## Self-review

**Spec coverage.**
- CLI grammar incl. `all`, `stop`/`start`/`quit`, `run -- | --wrap`, `logs` flags → Task 7 (`request_for` tests), Task 5 (`act`, `logs`).
- `all` semantics (skip in-flight, list, exit 0 unless all skipped, `--wait` for all) → Task 5 (`restart_all_*` tests), Task 6 (waiters with sets), Task 7 (`exit_code`).
- `--wait` returns marker block; CLI gives up after grace+10 s → Task 6 (waiters), Task 7 (600 s cap on the client — the spec says grace+10 s; the client does not know the grace, so a generous fixed ceiling is used; server side `REPLY_TIMEOUT` 600 s). *Deviation noted.*
- Override: kind on generation, `r` keeps, `k`/self-exit drop, new `run` replaces, watch pinned, `*` in bar, `resume` marker → Tasks 2, 3.
- `stop` leaves dead with status shown; `start`; `quit` = `q` path with socket closing on exit → Tasks 2, 5, 6, 7.
- `logs` tail/since/color/all/json shapes; stripped by default; markers included → Tasks 1, 5, 7.
- `status` human form with `*` and current command → Task 7; JSON shape → Task 4.
- Exit codes 0/1/2/3 → Task 7.
- Discovery mirrors launch (file → discover → cwd) → Task 7 (`project_dir_for`).
- Socket path (runtime dir, hash, perms), stale vs live, unlink on drop incl. panic, `NO CTRL` → Tasks 5, 6.
- Protocol: one request per connection, line JSON, `v`, error on malformed/version → Tasks 4, 5 (`serve_forwards_requests…`).
- `StyledLine.raw` for `--color` → Task 1.
- Response shape: single-slot verbs return the same `Acted` shape as `all` (one `started` entry) instead of the spec's flat `{proc,name,from_gen,to_gen,marker}` — one shape for both is simpler for clients. *Deviation noted.*
- Out of scope unchanged (`--follow`, headless).

**Placeholder scan.** None.

**Type consistency.** `Request`/`Response` variants and field names identical across Tasks 4, 5, 6, 7. `Handled::{Now, AfterTransitions{procs, partial}, Quit}` in Tasks 5, 6. `Ctx { manager, buffers, ui, project_dir }` in Tasks 5, 6. `replace_with(proc, String, GenKind, Trigger)`, `stop(proc, Trigger)`, `snapshot(proc) -> SlotInfo` in Tasks 2, 3, 5. `StyledLine::parse(proc, gen, stream, seq, at, bytes)` in Tasks 1, 5. `drain_events(rx, buffers, ui, manager, waiters, project_dir) -> bool` and `apply_transition(t, manager, buffers, ui, waiters)` in Task 6 and its test updates. Test counts: 129 → 130 → 134 → 137 → 139 → 149 → 150 → 155.
