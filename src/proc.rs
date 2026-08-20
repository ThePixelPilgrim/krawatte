//! Child process lifecycle: spawning, reader threads, and orderly shutdown.
//!
//! Each command is spawned via `sh -c` in its own process group (`setpgid`) so
//! signals reach the whole child tree. stdout and stderr are piped separately;
//! one reader thread per stream emits [`Event::Line`] messages (with a shared
//! global sequence counter) over the `mpsc` channel, and a per-child waiter
//! thread reaps the process and reports [`Event::Exited`]. Shutdown runs the
//! TERM -> grace -> KILL state machine.
//!
//! The signalling/sequencing logic ([`ShutdownMachine`]) is factored behind the
//! [`ShutdownEffects`] trait so it can be unit-tested against a deterministic
//! stub, while the actual `nix`/`std::process` calls stay thin.

use std::collections::HashSet;
use std::io::{BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, setpgid};

use crate::config::short_name_of;
use crate::types::{Config, Event, ExitStatus, Gen, ProcId, Seq, StreamTag};

/// One spawn of a slot's command. A slot holds at most one generation that may
/// still be alive; a restart tears it down and replaces it.
struct Generation {
    /// Which generation of its slot this is; `0` for the initial spawn.
    r#gen: Gen,
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
    r#gen: Gen,
    /// The most recent generation, or `None` if the slot has never spawned
    /// successfully.
    live: Option<Generation>,
    /// Teardown in progress, if any. A slot has at most one.
    restart: Option<Restart>,
}

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
    pub r#gen: Gen,
    pub pid: i32,
    pub outcome: Outcome,
    /// How long the generation ran, spawn to teardown.
    pub ran: Duration,
}

/// The generation a [`Transition`] started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGen {
    pub r#gen: Gen,
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

impl ProcManager {
    /// Spawn every command (each a string run via `sh -c`), wiring reader and
    /// waiter threads that emit [`Event`]s on `tx`. Spawn failures are reported
    /// as [`Event::SpawnFailed`] rather than aborting the whole set.
    pub fn spawn_all(commands: &[String], config: &Config, tx: Sender<Event>) -> ProcManager {
        Self::spawn_all_with_shell(commands, config, tx, "sh")
    }

    /// Like [`spawn_all`](Self::spawn_all) but with an explicit shell program.
    /// Exists so tests can point at a non-existent program and exercise the
    /// genuine spawn-failure (`Event::SpawnFailed` / dead slot) code path.
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
                        r#gen: 0,
                        error: err.to_string(),
                    });
                    None
                }
            };
            mgr.procs.push(Proc {
                standard: command.clone(),
                short: short_name_of(command),
                r#gen: 0,
                live,
                restart: None,
            });
        }
        mgr
    }

    /// Number of processes managed.
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// True if no live children remain.
    #[allow(dead_code)]
    pub fn all_dead(&self) -> bool {
        self.procs.iter().all(|p| {
            p.live
                .as_ref()
                .is_none_or(|g| g.dead.load(Ordering::SeqCst))
        })
    }

    /// Run the orderly shutdown sequence: SIGTERM every live process group,
    /// poll for exits up to `config.grace_period`, SIGKILL survivors, then reap
    /// all. Returns each process's final status indexed by [`ProcId`].
    pub fn shutdown(&mut self) -> Vec<Option<ExitStatus>> {
        // Every slot that ever had a process group is a shutdown candidate --
        // including one whose direct child has already been reaped. Reaping the
        // group leader does not dissolve its group: background jobs started with
        // `&` live on in it, still holding the child's pipes. Selecting on
        // `!dead` here used to skip those groups entirely, leaving them running.
        for p in &mut self.procs {
            p.restart = None;
        }
        let live: Vec<ProcId> = self
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.live.is_some())
            .map(|(i, _)| i)
            .collect();

        let grace = self.grace_period;
        let mut effects = RealEffects { mgr: self };
        let mut machine = ShutdownMachine::new(live, grace);
        machine.run(&mut effects, Duration::from_millis(20));

        // Join the waiter threads -- bounded, since a waiter is all but finished
        // once it has published `dead` -- and collect the recorded statuses. The
        // reader threads are never joined; see the `waiter` field on `Generation`.
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
    }

    /// Whether this slot's command actually spawned (and so has a process
    /// group), as opposed to having failed at `spawn` time.
    pub fn was_started(&self, proc: ProcId) -> bool {
        self.procs[proc].live.is_some()
    }

    /// Short display name derived from a process's command line (for the status
    /// bar).
    pub fn short_name(&self, proc: ProcId) -> &str {
        &self.procs[proc].short
    }

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

    /// Tear down the slot's current generation and spawn the slot's standard
    /// command in its place. Today every generation *is* the standard command,
    /// so this equals [`replace`](Self::replace) with the current command; it
    /// diverges once an override can run in a slot. Returns `false`, doing
    /// nothing, if a restart is already in flight.
    pub fn kill(&mut self, proc: ProcId) -> bool {
        let standard = self.procs[proc].standard.clone();
        self.replace(proc, standard)
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
                r#gen: g.r#gen,
                pid: g.pid,
                outcome,
                ran,
            }
        });
        let command = restart.next;
        let r#gen = self.procs[proc].r#gen + 1;
        self.procs[proc].r#gen = r#gen;
        let spawn = match spawn_one(proc, r#gen, &self.shell, &command, &self.seq, &self.tx) {
            Ok(g) => {
                let pid = g.pid;
                self.procs[proc].live = Some(g);
                Ok(pid)
            }
            Err(e) => {
                let _ = self.tx.send(Event::SpawnFailed {
                    proc,
                    r#gen,
                    error: e.to_string(),
                });
                Err(e.to_string())
            }
        };
        let new = NewGen {
            r#gen,
            command,
            spawn,
        };
        Transition { proc, old, new }
    }

    /// Number of the slot's current generation.
    #[allow(dead_code)] // test-only accessor
    pub fn current_gen(&self, proc: ProcId) -> Gen {
        self.procs[proc].r#gen
    }

    /// Whether an event stamped `gen` belongs to the slot's current generation.
    /// Anything older is stale output from a replaced generation (or from a
    /// grandchild that escaped its group and still holds the old pipe).
    pub fn is_current(&self, proc: ProcId, r#gen: Gen) -> bool {
        self.procs.get(proc).is_some_and(|p| p.r#gen == r#gen)
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
}

impl Drop for ProcManager {
    /// Panic-safety drop guard: on the normal path `shutdown()` has already
    /// emptied every process group and joined the waiters, so this finds nothing
    /// to do. If the manager is instead dropped while unwinding from a panic in
    /// the UI, this ensures no child is left orphaned: SIGKILL every process
    /// group not yet confirmed empty, then join whichever waiters have already
    /// published `dead`. Nothing here waits on an unbounded event.
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
}

/// Spawn a single child in its own process group, wiring reader threads for
/// stdout/stderr and a waiter thread that reaps and reports the exit.
fn spawn_one(
    proc: ProcId,
    r#gen: Gen,
    shell: &str,
    command: &str,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
) -> std::io::Result<Generation> {
    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the child in its own process group so a later killpg reaches the
    // whole subtree, not just the immediate `sh`.
    unsafe {
        cmd.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
            Ok(())
        });
    }

    let mut child: Child = cmd.spawn()?;
    let pid = child.id() as i32;
    let pgid = Pid::from_raw(pid);

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Detached on purpose -- see the `waiter` field on `Generation`.
    spawn_reader(
        proc,
        r#gen,
        StreamTag::Stdout,
        stdout,
        seq.clone(),
        tx.clone(),
    );
    spawn_reader(
        proc,
        r#gen,
        StreamTag::Stderr,
        stderr,
        seq.clone(),
        tx.clone(),
    );

    let dead = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(None));

    let waiter_dead = dead.clone();
    let waiter_status = status.clone();
    let waiter_tx = tx.clone();
    let waiter = std::thread::spawn(move || {
        let st = match child.wait() {
            Ok(es) => exit_status_from(&es),
            // If wait fails, synthesize a plausible terminal status.
            Err(_) => ExitStatus::Code(-1),
        };
        *waiter_status.lock().unwrap() = Some(st);
        waiter_dead.store(true, Ordering::SeqCst);
        let _ = waiter_tx.send(Event::Exited {
            proc,
            r#gen,
            status: st,
        });
    });

    Ok(Generation {
        r#gen,
        command: command.to_string(),
        pid,
        pgid,
        started: Instant::now(),
        dead,
        status,
        waiter: Some(waiter),
        finished: false,
    })
}

/// Spawn a detached line-reader thread for one stream, emitting [`Event::Line`]
/// per line. The handle is dropped: nothing may ever block on this thread, since
/// it ends only at pipe EOF, which krawatte cannot force.
fn spawn_reader(
    proc: ProcId,
    r#gen: Gen,
    stream: StreamTag,
    src: impl Read + Send + 'static,
    seq: Arc<AtomicU64>,
    tx: Sender<Event>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(src);
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => {
                    // EOF: flush any trailing partial line.
                    if !buf.is_empty() {
                        emit(proc, r#gen, stream, &seq, &tx, &mut buf);
                    }
                    break;
                }
                Ok(_) => {
                    if byte[0] == b'\n' {
                        emit(proc, r#gen, stream, &seq, &tx, &mut buf);
                    } else {
                        buf.push(byte[0]);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

/// Emit one line event, stripping a trailing `\r`, and reset the buffer.
///
/// The arrival timestamp is taken here, next to the sequence number, so it
/// records when the line actually reached krawatte -- unaffected by the UI
/// thread's 50 ms batching. It is display-only; ordering stays governed by
/// [`Seq`].
fn emit(
    proc: ProcId,
    r#gen: Gen,
    stream: StreamTag,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
    buf: &mut Vec<u8>,
) {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    let s: Seq = seq.fetch_add(1, Ordering::SeqCst);
    let at = SystemTime::now();
    let bytes = std::mem::take(buf);
    let _ = tx.send(Event::Line {
        proc,
        r#gen,
        stream,
        seq: s,
        at,
        bytes,
    });
}

/// Convert a std exit status into our terminal [`ExitStatus`].
fn exit_status_from(es: &std::process::ExitStatus) -> ExitStatus {
    if let Some(code) = es.code() {
        ExitStatus::Code(code)
    } else if let Some(sig) = es.signal() {
        ExitStatus::Signal(sig)
    } else {
        ExitStatus::Code(-1)
    }
}

// ---------------------------------------------------------------------------
// Shutdown state machine (pure sequencing, testable against a stub)
// ---------------------------------------------------------------------------

/// The TERM -> grace -> KILL shutdown state machine, factored out of the OS
/// calls so it can be driven and tested deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPhase {
    /// SIGTERM sent; waiting within the grace period for children to exit.
    Terminating,
    /// Grace expired; SIGKILL has been sent to survivors.
    Killing,
    /// All children reaped.
    Done,
}

/// Side effects the shutdown machine performs, abstracted for testing. The real
/// implementation signals process groups and polls waiter threads; the test
/// implementation records calls and simulates exits.
pub trait ShutdownEffects {
    /// Send SIGTERM to the given process's group.
    fn term(&mut self, proc: ProcId);
    /// Send SIGKILL to the given process's group.
    fn kill(&mut self, proc: ProcId);
    /// Return the set of processes that have exited since the last poll.
    fn poll_exited(&mut self) -> Vec<ProcId>;
    /// Monotonic clock reading, used to measure the grace period.
    fn now(&mut self) -> Instant;
    /// Sleep between polls (a no-op in tests).
    fn sleep(&mut self, dur: Duration);
}

/// Deterministic driver for the TERM -> grace -> KILL sequence.
pub struct ShutdownMachine {
    phase: ShutdownPhase,
    live: HashSet<ProcId>,
    grace: Duration,
    started: Option<Instant>,
    /// Deadline for survivors to disappear after SIGKILL; set on entering
    /// [`ShutdownPhase::Killing`].
    kill_deadline: Option<Instant>,
    /// Processes given up on: still present after SIGKILL and the reap timeout.
    abandoned: Vec<ProcId>,
}

/// How long to wait for process groups to vanish after SIGKILL. SIGKILL cannot
/// be caught or blocked, so anything still present after this is wedged in
/// uninterruptible I/O and waiting longer will not help. Shutdown must stay
/// bounded no matter what the children do.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

impl ShutdownMachine {
    /// Create a machine for the given initially-live processes and grace period.
    pub fn new(live: impl IntoIterator<Item = ProcId>, grace: Duration) -> ShutdownMachine {
        let live: HashSet<ProcId> = live.into_iter().collect();
        ShutdownMachine {
            phase: if live.is_empty() {
                ShutdownPhase::Done
            } else {
                ShutdownPhase::Terminating
            },
            live,
            grace,
            started: None,
            kill_deadline: None,
            abandoned: Vec::new(),
        }
    }

    /// Processes shutdown gave up on: still alive after SIGKILL and the reap
    /// timeout. Empty on every normal shutdown.
    pub fn abandoned(&self) -> Vec<ProcId> {
        let mut a = self.abandoned.clone();
        a.sort();
        a
    }

    /// Current phase.
    pub fn phase(&self) -> ShutdownPhase {
        self.phase
    }

    /// Processes still believed alive.
    #[allow(dead_code)]
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Run to completion, polling on `poll_interval`.
    pub fn run(&mut self, effects: &mut impl ShutdownEffects, poll_interval: Duration) {
        while self.phase != ShutdownPhase::Done {
            self.step(effects);
            if self.phase != ShutdownPhase::Done {
                effects.sleep(poll_interval);
            }
        }
    }

    /// Advance the state machine by one poll. Idempotent transitions:
    ///  - On the first step, send SIGTERM to every live group and start the clock.
    ///  - Each step, harvest exits; drop them from the live set.
    ///  - If the grace period elapses while still `Terminating`, SIGKILL the
    ///    survivors and move to `Killing`.
    ///  - When the live set empties, move to `Done`.
    pub fn step(&mut self, effects: &mut impl ShutdownEffects) {
        if self.phase == ShutdownPhase::Done {
            return;
        }

        // First entry: fire SIGTERM at everyone and start the grace clock.
        if self.started.is_none() {
            for &p in &self.live {
                effects.term(p);
            }
            self.started = Some(effects.now());
        }

        // Harvest any exits reported by waiter threads.
        for p in effects.poll_exited() {
            self.live.remove(&p);
        }
        if self.live.is_empty() {
            self.phase = ShutdownPhase::Done;
            return;
        }

        // Grace expiry: escalate to SIGKILL exactly once.
        if self.phase == ShutdownPhase::Terminating {
            let elapsed = effects
                .now()
                .saturating_duration_since(self.started.unwrap());
            if elapsed >= self.grace {
                let survivors: Vec<ProcId> = self.live.iter().copied().collect();
                for p in survivors {
                    effects.kill(p);
                }
                self.phase = ShutdownPhase::Killing;
                self.kill_deadline = Some(effects.now() + KILL_REAP_TIMEOUT);
                return;
            }
        }

        // Post-SIGKILL survivors: give up rather than spin forever. Nothing the
        // user can press would break out of that loop, so a bounded surrender is
        // the only way `q` can be guaranteed to return.
        if self.phase == ShutdownPhase::Killing
            && let Some(deadline) = self.kill_deadline
            && effects.now() >= deadline
        {
            self.abandoned = self.live.iter().copied().collect();
            self.live.clear();
            self.phase = ShutdownPhase::Done;
        }
    }
}

/// True when a process group has no members left at all: `killpg` with the null
/// signal reports ESRCH. A zombie still counts as a member, so this only turns
/// true once the leader has been reaped *and* every background job started in
/// its group is gone. Any other error (notably EPERM) is treated as "still
/// there", which errs toward signalling again rather than abandoning early.
fn group_gone(pgid: Pid) -> bool {
    matches!(killpg(pgid, None), Err(nix::errno::Errno::ESRCH))
}

/// Real effects: signal process groups via `killpg` and observe waiter threads
/// through each child's `dead` flag.
struct RealEffects<'a> {
    mgr: &'a mut ProcManager,
}

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

    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, dur: Duration) {
        std::thread::sleep(dur);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Stub effects: a virtual clock and scripted exits, recording every signal.
    struct StubEffects {
        clock: Instant,
        term_calls: Vec<ProcId>,
        kill_calls: Vec<ProcId>,
        /// Exits to reveal keyed by the poll number at which they surface.
        exits_at_poll: Vec<(u32, ProcId)>,
        poll_count: u32,
        /// How much virtual time each `sleep` advances the clock.
        step_advance: Duration,
    }

    impl StubEffects {
        fn new(step_advance: Duration) -> Self {
            StubEffects {
                clock: Instant::now(),
                term_calls: Vec::new(),
                kill_calls: Vec::new(),
                exits_at_poll: Vec::new(),
                poll_count: 0,
                step_advance,
            }
        }
    }

    impl ShutdownEffects for StubEffects {
        fn term(&mut self, proc: ProcId) {
            self.term_calls.push(proc);
        }
        fn kill(&mut self, proc: ProcId) {
            self.kill_calls.push(proc);
        }
        fn poll_exited(&mut self) -> Vec<ProcId> {
            let now = self.poll_count;
            self.poll_count += 1;
            assert!(
                self.poll_count < 10_000,
                "shutdown machine never reached Done -- it is looping forever"
            );
            self.exits_at_poll
                .iter()
                .filter(|(p, _)| *p == now)
                .map(|(_, id)| *id)
                .collect()
        }
        fn now(&mut self) -> Instant {
            self.clock
        }
        fn sleep(&mut self, _dur: Duration) {
            self.clock += self.step_advance;
        }
    }

    /// Spin until every managed child has exited on its own (bounded).
    fn wait_until_dead(mgr: &ProcManager) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !mgr.all_dead() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn empty_set_starts_done() {
        let m = ShutdownMachine::new(Vec::<ProcId>::new(), Duration::from_secs(5));
        assert_eq!(m.phase(), ShutdownPhase::Done);
    }

    #[test]
    fn term_sent_to_all_on_first_step() {
        let mut fx = StubEffects::new(Duration::from_millis(0));
        let mut m = ShutdownMachine::new([0, 1, 2], Duration::from_secs(5));
        m.step(&mut fx);
        let mut sent = fx.term_calls.clone();
        sent.sort();
        assert_eq!(sent, vec![0, 1, 2]);
        assert_eq!(m.phase(), ShutdownPhase::Terminating);
    }

    #[test]
    fn graceful_exit_within_grace_never_kills() {
        // Children exit while still within grace: SIGKILL must never fire.
        let mut fx = StubEffects::new(Duration::from_millis(100));
        fx.exits_at_poll = vec![(0, 0), (0, 1)];
        let mut m = ShutdownMachine::new([0, 1], Duration::from_secs(5));
        m.run(&mut fx, Duration::from_millis(100));
        assert_eq!(m.phase(), ShutdownPhase::Done);
        assert!(fx.kill_calls.is_empty());
        assert_eq!(fx.term_calls.len(), 2);
    }

    #[test]
    fn straggler_gets_killed_after_grace() {
        // proc 0 exits immediately; proc 1 never does -> must be SIGKILLed
        // after grace, then simulated dead so the machine finishes.
        let mut fx = StubEffects::new(Duration::from_millis(1000));
        // proc 0 exits at first poll; proc 1 "dies" only after the kill (poll 6).
        fx.exits_at_poll = vec![(0, 0), (6, 1)];
        let mut m = ShutdownMachine::new([0, 1], Duration::from_secs(5));
        m.run(&mut fx, Duration::from_millis(1000));
        assert_eq!(m.phase(), ShutdownPhase::Done);
        assert_eq!(fx.kill_calls, vec![1]);
    }

    #[test]
    fn kill_sent_exactly_once() {
        // A survivor that stays alive across many polls after grace must be
        // SIGKILLed only once.
        let mut fx = StubEffects::new(Duration::from_millis(2000));
        fx.exits_at_poll = vec![(50, 0)]; // exits far in the future
        let mut m = ShutdownMachine::new([0], Duration::from_secs(5));
        // Drive several steps manually past grace.
        for _ in 0..10 {
            m.step(&mut fx);
            fx.clock += Duration::from_millis(2000);
        }
        assert_eq!(fx.kill_calls, vec![0]);
        // Past the post-SIGKILL reap timeout the survivor is abandoned, so the
        // machine finishes rather than polling it forever.
        assert_eq!(m.phase(), ShutdownPhase::Done);
        assert_eq!(m.abandoned(), vec![0]);
    }

    #[test]
    fn run_gives_up_when_a_process_survives_sigkill() {
        // A process wedged in uninterruptible I/O never gets reaped, not even
        // after SIGKILL. Shutdown must abandon it rather than spin forever --
        // otherwise `q` never returns and the user cannot escape the TUI.
        let mut fx = StubEffects::new(Duration::from_millis(500));
        fx.exits_at_poll = Vec::new(); // nobody ever exits
        let mut m = ShutdownMachine::new([0], Duration::from_secs(5));
        m.run(&mut fx, Duration::from_millis(500));
        assert_eq!(m.phase(), ShutdownPhase::Done);
        assert_eq!(
            fx.kill_calls,
            vec![0],
            "SIGKILL should still have been tried"
        );
        assert_eq!(m.abandoned(), vec![0]);
    }

    #[test]
    fn spawn_failure_reports_dead_slot() {
        // A command that cannot possibly run should still produce a slot; the
        // executed `sh -c` exits non-zero rather than failing to spawn, but the
        // slot must end up dead and status recorded.
        let (tx, rx) = mpsc::channel();
        let cfg = Config::default();
        let mut mgr = ProcManager::spawn_all(&["exit 7".to_string()], &cfg, tx);
        assert_eq!(mgr.len(), 1);
        wait_until_dead(&mgr);
        let statuses = mgr.shutdown();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0], Some(ExitStatus::Code(7)));
        assert!(mgr.all_dead());
        // At least one Exited event should have been delivered.
        let saw_exit = rx.try_iter().any(|e| matches!(e, Event::Exited { .. }));
        assert!(saw_exit);
    }

    #[test]
    fn genuine_spawn_failure_reports_dead_slot() {
        // Point the manager at a shell program that does not exist, so
        // `Command::spawn` itself fails: this exercises the real `Err` branch of
        // `spawn_one` -> `Event::SpawnFailed` -> dead slot with no pgid.
        let (tx, rx) = mpsc::channel();
        let cfg = Config::default();
        let mut mgr = ProcManager::spawn_all_with_shell(
            &["whatever".to_string()],
            &cfg,
            tx,
            "/nonexistent/krawatte-no-such-shell",
        );
        assert_eq!(mgr.len(), 1);
        // The slot has no process group and is immediately dead.
        assert!(mgr.procs[0].live.is_none());
        assert!(mgr.all_dead());
        // A SpawnFailed event was delivered for this slot.
        let saw_spawn_failed = rx
            .try_iter()
            .any(|e| matches!(e, Event::SpawnFailed { proc: 0, .. }));
        assert!(saw_spawn_failed);
        // Shutdown yields a `None` status (never started) for the slot.
        let statuses = mgr.shutdown();
        assert_eq!(statuses, vec![None]);
    }

    /// Run `shutdown()` on another thread and fail if it has not returned within
    /// `limit`. Keeps a hang from wedging the whole test binary.
    fn shutdown_within(mut mgr: ProcManager, limit: Duration) -> Vec<Option<ExitStatus>> {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let statuses = mgr.shutdown();
            let _ = done_tx.send(statuses);
        });
        match done_rx.recv_timeout(limit) {
            Ok(statuses) => statuses,
            Err(_) => panic!("shutdown() did not return within {limit:?}"),
        }
    }

    #[test]
    fn shutdown_returns_when_a_background_job_still_holds_the_pipe() {
        // The direct child exits immediately but leaves a background job in its
        // process group that inherited the stdout pipe. Shutdown must still
        // finish: it may not wait on a pipe EOF it does not control.
        let (tx, _rx) = mpsc::channel();
        let cfg = Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        };
        let mgr = ProcManager::spawn_all(&["sleep 30 & echo started".to_string()], &cfg, tx);
        // Wait for the direct child to be reaped, as it would be long before the
        // user presses `q`. Its group -- and the pipe -- outlive it.
        wait_until_dead(&mgr);
        assert!(mgr.all_dead());
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn shutdown_kills_background_jobs_left_behind_in_a_dead_child_group() {
        // The direct child exits but leaves a background job in its process
        // group. Reaping the group leader does not dissolve the group, so
        // shutdown must still signal it -- otherwise `q` leaves orphans behind.
        let (tx, rx) = mpsc::channel();
        let cfg = Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        };
        let mgr = ProcManager::spawn_all(&["sleep 30 & echo $!".to_string()], &cfg, tx);
        wait_until_dead(&mgr);

        // The child echoed the background job's pid on stdout.
        let bg = read_pid_line(&rx);
        assert!(
            nix::sys::signal::kill(bg, None).is_ok(),
            "background job {bg} should still be alive before shutdown"
        );

        shutdown_within(mgr, Duration::from_secs(5));

        // Poll briefly: once killed it is reparented to init and reaped there.
        let deadline = Instant::now() + Duration::from_secs(3);
        while nix::sys::signal::kill(bg, None).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            nix::sys::signal::kill(bg, None).is_err(),
            "background job {bg} survived shutdown"
        );
    }

    /// Read the first `Event::Line` off the channel and parse it as a pid.
    fn read_pid_line(rx: &mpsc::Receiver<Event>) -> Pid {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Event::Line { bytes, .. }) = rx.recv_timeout(Duration::from_millis(100)) {
                let text = String::from_utf8_lossy(&bytes);
                if let Ok(n) = text.trim().parse::<i32>() {
                    return Pid::from_raw(n);
                }
            }
        }
        panic!("child never reported a background pid");
    }

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
        assert_eq!(old.r#gen, 0);
        assert_eq!(old.pid, old_pid.as_raw());
        // `sh` has the default TERM disposition, so the grace period never
        // expires and the machine never escalates to KILL.
        assert_eq!(old.outcome, Outcome::Exited(ExitStatus::Signal(15)));
        assert_eq!(t.new.r#gen, 1);
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
        assert_eq!(t.new.r#gen, 1);
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
        let mut mgr =
            ProcManager::spawn_all(&["printf 'a\\nb\\n'".to_string()], &Config::default(), tx);
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

    #[test]
    fn shutdown_returns_when_a_grandchild_escapes_the_process_group() {
        // A `setsid` grandchild leaves the process group entirely, so killpg
        // cannot reach it, yet it holds the stdout pipe open. Shutdown must not
        // block on that either.
        let (tx, _rx) = mpsc::channel();
        let cfg = Config {
            grace_period: Duration::from_millis(200),
            ..Config::default()
        };
        let mgr = ProcManager::spawn_all(&["setsid sleep 30 & echo started".to_string()], &cfg, tx);
        wait_until_dead(&mgr);
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn line_events_carry_increasing_seq() {
        let (tx, rx) = mpsc::channel();
        let cfg = Config::default();
        let mut mgr = ProcManager::spawn_all(&["printf 'a\\nb\\nc\\n'".to_string()], &cfg, tx);
        wait_until_dead(&mgr);
        let statuses = mgr.shutdown();
        assert_eq!(statuses[0], Some(ExitStatus::Code(0)));
        let mut seqs: Vec<Seq> = Vec::new();
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut gens: Vec<Gen> = Vec::new();
        for e in rx.try_iter() {
            if let Event::Line {
                seq, bytes, r#gen, ..
            } = e
            {
                seqs.push(seq);
                lines.push(bytes);
                gens.push(r#gen);
            }
        }
        assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        // The initial spawn is generation 0; restarts count up from there.
        assert!(gens.iter().all(|&g| g == 0));
    }

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
        assert_eq!(t.old.unwrap().r#gen, 1);
        assert_eq!(t.new.r#gen, 2);
        assert_eq!(t.new.command, "sleep 30");
        assert_eq!(mgr.current_command(0), "sleep 30");
        shutdown_within(mgr, Duration::from_secs(5));
    }
}
