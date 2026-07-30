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
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, setpgid};

use crate::types::{Config, Event, ExitStatus, ProcId, Seq, StreamTag};

/// Per-child state retained by the manager after spawning.
struct Proc {
    /// Full command string (as passed to `sh -c`). Retained for diagnostics.
    #[allow(dead_code)]
    command: String,
    /// Precomputed short display name for the status bar.
    short: String,
    /// Process-group id used for signalling (equal to the child's pid, since it
    /// leads its own group). `None` for a slot that failed to spawn.
    pgid: Option<Pid>,
    /// Set to `true` by the waiter thread once the child has been reaped.
    dead: Arc<AtomicBool>,
    /// Final exit status, filled in by the waiter thread. `None` while running
    /// or for a spawn failure.
    status: Arc<Mutex<Option<ExitStatus>>>,
    /// Join handle for the waiter thread. Safe to join once `dead` is set;
    /// `None` for a slot that failed to spawn or that has already been joined.
    ///
    /// The two reader threads deliberately have no handles here: they block in
    /// `read()` until *every* holder of the pipe's write end closes it, and that
    /// can include processes which outlived the child or escaped its process
    /// group entirely (a `setsid` daemon, say). Shutdown must never wait on an
    /// event krawatte cannot force, so readers are detached and left to be
    /// cleaned up by process exit.
    waiter: Option<JoinHandle<()>>,
    /// Set by [`ProcManager::shutdown`] once this slot's process group has been
    /// confirmed empty, so the drop guard knows not to signal the pgid again
    /// (by then the kernel may have recycled it for an unrelated process).
    finished: bool,
}

/// Manages the full set of child processes and the shared event channel.
pub struct ProcManager {
    procs: Vec<Proc>,
    grace_period: Duration,
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
        // One shared, monotonically increasing sequence counter across every
        // process and both streams, so the all-view can reconstruct arrival
        // order.
        let seq = Arc::new(AtomicU64::new(0));
        let mut procs = Vec::with_capacity(commands.len());

        for (proc, command) in commands.iter().enumerate() {
            let short = short_name_of(command);
            match spawn_one(proc, shell, command, &seq, &tx) {
                Ok((pgid, dead, status, waiter)) => procs.push(Proc {
                    command: command.clone(),
                    short,
                    pgid: Some(pgid),
                    dead,
                    status,
                    waiter: Some(waiter),
                    finished: false,
                }),
                Err(err) => {
                    // Spawn failure: report it and record an immediately-dead slot.
                    let _ = tx.send(Event::SpawnFailed {
                        proc,
                        error: err.to_string(),
                    });
                    procs.push(Proc {
                        command: command.clone(),
                        short,
                        pgid: None,
                        dead: Arc::new(AtomicBool::new(true)),
                        status: Arc::new(Mutex::new(None)),
                        waiter: None,
                        finished: true,
                    });
                }
            }
        }

        ProcManager {
            procs,
            grace_period: config.grace_period,
        }
    }

    /// Number of processes managed.
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// True if no live children remain.
    #[allow(dead_code)]
    pub fn all_dead(&self) -> bool {
        self.procs.iter().all(|p| p.dead.load(Ordering::SeqCst))
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
        let live: Vec<ProcId> = self
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pgid.is_some())
            .map(|(i, _)| i)
            .collect();

        let grace = self.grace_period;
        let mut effects = RealEffects { mgr: self };
        let mut machine = ShutdownMachine::new(live, grace);
        machine.run(&mut effects, Duration::from_millis(20));

        // Join the waiter threads -- bounded, since a waiter is all but finished
        // once it has published `dead` -- and collect the recorded statuses. The
        // reader threads are never joined; see the `waiter` field on `Proc`.
        for p in &mut self.procs {
            if let Some(pgid) = p.pgid {
                p.finished = p.dead.load(Ordering::SeqCst) && group_gone(pgid);
            }
            if p.dead.load(Ordering::SeqCst)
                && let Some(h) = p.waiter.take()
            {
                let _ = h.join();
            }
        }
        self.procs
            .iter()
            .map(|p| *p.status.lock().unwrap())
            .collect()
    }

    /// Whether this slot's command actually spawned (and so has a process
    /// group), as opposed to having failed at `spawn` time.
    pub fn was_started(&self, proc: ProcId) -> bool {
        self.procs[proc].pgid.is_some()
    }

    /// Short display name derived from a process's command line (for the status
    /// bar).
    pub fn short_name(&self, proc: ProcId) -> &str {
        &self.procs[proc].short
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
        for p in &self.procs {
            if let Some(pgid) = p.pgid
                && !p.finished
            {
                let _ = killpg(pgid, Signal::SIGKILL);
            }
        }
        for p in &mut self.procs {
            if p.dead.load(Ordering::SeqCst)
                && let Some(h) = p.waiter.take()
            {
                let _ = h.join();
            }
        }
    }
}

/// Spawn a single child in its own process group, wiring reader threads for
/// stdout/stderr and a waiter thread that reaps and reports the exit.
type SpawnParts = (Pid, Arc<AtomicBool>, Arc<Mutex<Option<ExitStatus>>>, JoinHandle<()>);

fn spawn_one(
    proc: ProcId,
    shell: &str,
    command: &str,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
) -> std::io::Result<SpawnParts> {
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

    // Detached on purpose -- see the `waiter` field on `Proc`.
    spawn_reader(proc, StreamTag::Stdout, stdout, seq.clone(), tx.clone());
    spawn_reader(proc, StreamTag::Stderr, stderr, seq.clone(), tx.clone());

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
        let _ = waiter_tx.send(Event::Exited { proc, status: st });
    });

    Ok((pgid, dead, status, waiter))
}

/// Spawn a detached line-reader thread for one stream, emitting [`Event::Line`]
/// per line. The handle is dropped: nothing may ever block on this thread, since
/// it ends only at pipe EOF, which krawatte cannot force.
fn spawn_reader(
    proc: ProcId,
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
                        emit(proc, stream, &seq, &tx, &mut buf);
                    }
                    break;
                }
                Ok(_) => {
                    if byte[0] == b'\n' {
                        emit(proc, stream, &seq, &tx, &mut buf);
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
fn emit(proc: ProcId, stream: StreamTag, seq: &Arc<AtomicU64>, tx: &Sender<Event>, buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    let s: Seq = seq.fetch_add(1, Ordering::SeqCst);
    let bytes = std::mem::take(buf);
    let _ = tx.send(Event::Line {
        proc,
        stream,
        seq: s,
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

/// Derive a short status-bar name from a command line: the basename of the first
/// whitespace-separated token.
fn short_name_of(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    if base.is_empty() {
        command.to_string()
    } else {
        base.to_string()
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
    #[allow(dead_code)]
    pub fn abandoned(&self) -> Vec<ProcId> {
        let mut a = self.abandoned.clone();
        a.sort();
        a
    }

    /// Current phase.
    #[allow(dead_code)]
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
            let elapsed = effects.now().saturating_duration_since(self.started.unwrap());
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
        if let Some(pgid) = self.mgr.procs[proc].pgid {
            let _ = killpg(pgid, Signal::SIGTERM);
        }
    }

    fn kill(&mut self, proc: ProcId) {
        if let Some(pgid) = self.mgr.procs[proc].pgid {
            let _ = killpg(pgid, Signal::SIGKILL);
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
                p.dead.load(Ordering::SeqCst) && p.pgid.is_none_or(group_gone)
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
        assert_eq!(fx.kill_calls, vec![0], "SIGKILL should still have been tried");
        assert_eq!(m.abandoned(), vec![0]);
    }

    #[test]
    fn short_name_takes_basename_of_first_token() {
        assert_eq!(short_name_of("cargo watch -x check"), "cargo");
        assert_eq!(short_name_of("/usr/bin/python worker.py"), "python");
        assert_eq!(short_name_of("npm run dev"), "npm");
        assert_eq!(short_name_of(""), "");
    }

    #[test]
    fn spawn_failure_reports_dead_slot() {
        // A command that cannot possibly run should still produce a slot; the
        // executed `sh -c` exits non-zero rather than failing to spawn, but the
        // slot must end up dead and status recorded.
        let (tx, rx) = mpsc::channel();
        let cfg = Config::default();
        let mut mgr = ProcManager::spawn_all(
            &["exit 7".to_string()],
            &cfg,
            tx,
        );
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
        assert!(mgr.procs[0].pgid.is_none());
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
        let mgr = ProcManager::spawn_all(
            &["setsid sleep 30 & echo started".to_string()],
            &cfg,
            tx,
        );
        wait_until_dead(&mgr);
        shutdown_within(mgr, Duration::from_secs(5));
    }

    #[test]
    fn line_events_carry_increasing_seq() {
        let (tx, rx) = mpsc::channel();
        let cfg = Config::default();
        let mut mgr = ProcManager::spawn_all(
            &["printf 'a\\nb\\nc\\n'".to_string()],
            &cfg,
            tx,
        );
        wait_until_dead(&mgr);
        let statuses = mgr.shutdown();
        assert_eq!(statuses[0], Some(ExitStatus::Code(0)));
        let mut seqs: Vec<Seq> = Vec::new();
        let mut lines: Vec<Vec<u8>> = Vec::new();
        for e in rx.try_iter() {
            if let Event::Line { seq, bytes, .. } = e {
                seqs.push(seq);
                lines.push(bytes);
            }
        }
        assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }
}
