//! The control socket: where it lives, how it is bound and served, and how
//! a request is answered. `handle` is pure given the manager and is the
//! unit-test surface; the socket code is a thin threaded shell around it.
#![allow(dead_code)] // wired into the main loop by a later task

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::buffer::{BufferSet, StyledLine};
use crate::proc::{GenKind, ProcManager};
use crate::protocol::{
    Envelope, LogLine, PROTOCOL_VERSION, ProcStatus, Request, Response, Skipped, Started,
};
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
    let key = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    runtime_dir().join(format!(
        "{:016x}.sock",
        fnv1a64(key.as_os_str().as_encoded_bytes())
    ))
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
            BindError::AnotherInstance(p) => write!(
                f,
                "another krawatte is already listening on {}",
                p.display()
            ),
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
        Ok(Listener {
            path: path.to_path_buf(),
            listener: Some(listener),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept connections on a detached thread. Each connection gets its own
    /// thread that reads one request, forwards it as [`Event::Control`], waits
    /// for the reply and writes it. Nothing here touches manager state.
    pub fn serve(&mut self, tx: Sender<Event>) {
        let Some(listener) = self.listener.take() else {
            return;
        };
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
    let response = match BufReader::new(&stream)
        .take(MAX_REQUEST)
        .read_line(&mut line)
    {
        Err(e) => Response::error(format!("read: {e}")),
        Ok(_) => match serde_json::from_str::<Envelope>(&line) {
            Err(e) => Response::error(format!("bad request: {e}")),
            Ok(env) if env.v != PROTOCOL_VERSION => Response::error(format!(
                "unsupported protocol version {} (this krawatte speaks {PROTOCOL_VERSION})",
                env.v
            )),
            Ok(env) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx
                    .send(Event::Control {
                        request: env.request,
                        reply: reply_tx,
                    })
                    .is_err()
                {
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
    AfterTransitions {
        procs: HashSet<ProcId>,
        partial: Response,
    },
    Quit(Response),
}

/// Slots named by `slot`: `all`, a 1-based index, or a name.
pub fn resolve_slot(manager: &ProcManager, slot: &str) -> Result<Vec<ProcId>, String> {
    let n = manager.len();
    if slot == "all" {
        return Ok((0..n).collect());
    }
    if let Ok(i) = slot.parse::<usize>() {
        return if (1..=n).contains(&i) {
            Ok(vec![i - 1])
        } else {
            Err(format!("slot index {i} out of range (1-{n})"))
        };
    }
    if let Some(p) = (0..n).find(|&p| manager.short_name(p) == slot) {
        return Ok(vec![p]);
    }
    let names: Vec<&str> = (0..n).map(|p| manager.short_name(p)).collect();
    Err(format!(
        "unknown slot {slot:?}; slots are: {} (1-{n})",
        names.join(", ")
    ))
}

/// `30s`, `5m`, `1h30m`, `250ms`. A bare number is rejected rather than
/// guessed at.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration {s:?}: use units like 30s, 5m, 1h30m");
    if s.is_empty() {
        return Err(bad());
    }
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let value: u64 = num.parse().map_err(|_| bad())?;
        num.clear();
        let unit = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            "ms"
        } else {
            match c {
                's' => "s",
                'm' => "m",
                'h' => "h",
                _ => return Err(bad()),
            }
        };
        total += match unit {
            "ms" => Duration::from_millis(value),
            "s" => Duration::from_secs(value),
            "m" => Duration::from_secs(value * 60),
            _ => Duration::from_secs(value * 3600),
        };
    }
    if !num.is_empty() {
        return Err(bad());
    }
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

/// Why a slot was skipped when a teardown is already in flight. The only
/// skip that makes a reply an error: the other reasons (`already stopped`,
/// `already running`) mean the slot is in the requested state already.
const IN_FLIGHT: &str = "restart in flight";

pub fn handle(request: &Request, ctx: Ctx<'_>) -> Handled {
    match request {
        Request::Status => Handled::Now(status(&ctx)),
        Request::Quit => Handled::Quit(Response::done()),
        Request::Logs {
            slot,
            tail,
            since_ms,
            color,
        } => Handled::Now(logs(&ctx, slot.as_deref(), *tail, *since_ms, *color)),
        Request::Restart { slot, wait } => act(ctx, slot, *wait, "restart", |m, p| {
            let cmd = m.current_command(p).to_string();
            m.replace(p, cmd, Trigger::Cli("restart".into()))
                .then_some(())
                .ok_or(IN_FLIGHT)
        }),
        Request::Kill { slot, wait } => act(ctx, slot, *wait, "kill", |m, p| {
            m.kill(p, Trigger::Cli("kill".into()))
                .then_some(())
                .ok_or(IN_FLIGHT)
        }),
        Request::Stop { slot, wait } => act(ctx, slot, *wait, "stop", |m, p| {
            if m.is_restarting(p) {
                return Err(IN_FLIGHT);
            }
            if m.is_dead(p) {
                return Err("already stopped");
            }
            m.stop(p, Trigger::Cli("stop".into()))
                .then_some(())
                .ok_or(IN_FLIGHT)
        }),
        Request::Start { slot, wait } => act(ctx, slot, *wait, "start", |m, p| {
            if m.is_restarting(p) {
                return Err(IN_FLIGHT);
            }
            if !m.is_dead(p) {
                return Err("already running");
            }
            let std_cmd = m.standard_command(p).to_string();
            m.replace_with(p, std_cmd, GenKind::Standard, Trigger::Cli("start".into()))
                .then_some(())
                .ok_or(IN_FLIGHT)
        }),
        Request::Run {
            slot,
            cmd,
            wrap,
            wait,
        } => {
            if slot == "all" {
                return Handled::Now(Response::error("run takes a single slot, not all"));
            }
            let command = match (cmd.is_empty(), wrap) {
                (false, None) => cmd.join(" "),
                (true, Some(prefix)) => match resolve_slot(ctx.manager, slot) {
                    Ok(procs) => format!("{prefix} {}", ctx.manager.standard_command(procs[0])),
                    Err(e) => return Handled::Now(Response::error(e)),
                },
                _ => {
                    return Handled::Now(Response::error(
                        "run needs exactly one of a command (after --) or --wrap",
                    ));
                }
            };
            act(ctx, slot, *wait, "run", move |m, p| {
                m.replace_with(
                    p,
                    command.clone(),
                    GenKind::Override,
                    Trigger::Cli("run".into()),
                )
                .then_some(())
                .ok_or(IN_FLIGHT)
            })
        }
    }
}

fn status(ctx: &Ctx<'_>) -> Response {
    let procs = (0..ctx.manager.len())
        .map(|p| {
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
        })
        .collect();
    Response::Status {
        ok: true,
        pid: std::process::id(),
        dir: ctx.project_dir.display().to_string(),
        procs,
    }
}

/// Apply `op` to every slot `slot` names. Slots the op refuses are listed
/// as skipped; the reply is an error only if nothing was started and some
/// slot was refused for being in flight (a no-op on every slot is fine).
fn act(
    ctx: Ctx<'_>,
    slot: &str,
    wait: bool,
    verb: &str,
    mut op: impl FnMut(&mut ProcManager, ProcId) -> Result<(), &'static str>,
) -> Handled {
    let targets = match resolve_slot(ctx.manager, slot) {
        Ok(t) => t,
        Err(e) => return Handled::Now(Response::error(e)),
    };
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    for p in targets {
        let name = ctx.manager.short_name(p).to_string();
        let from_gen = ctx
            .manager
            .was_started(p)
            .then(|| ctx.manager.current_gen(p));
        match op(ctx.manager, p) {
            Ok(()) => {
                ctx.ui.set_health(p, Health::Restarting);
                started.push(Started {
                    proc: p,
                    name,
                    from_gen,
                });
            }
            Err(reason) => skipped.push(Skipped {
                proc: p,
                name,
                reason: reason.to_string(),
            }),
        }
    }
    if started.is_empty() && skipped.iter().any(|s| s.reason == IN_FLIGHT) {
        let reasons: Vec<String> = skipped
            .iter()
            .map(|s| format!("{} ({})", s.name, s.reason))
            .collect();
        return Handled::Now(Response::error(format!(
            "{verb}: nothing to do: {}",
            reasons.join(", ")
        )));
    }
    let partial = Response::Acted {
        ok: true,
        started: started.clone(),
        skipped,
        markers: None,
    };
    if wait {
        Handled::AfterTransitions {
            procs: started.iter().map(|s| s.proc).collect(),
            partial,
        }
    } else {
        Handled::Now(partial)
    }
}

fn logs(
    ctx: &Ctx<'_>,
    slot: Option<&str>,
    tail: usize,
    since_ms: Option<u64>,
    color: bool,
) -> Response {
    let lines: Vec<&StyledLine> = match slot {
        None | Some("all") => ctx.buffers.interleaved(),
        Some(s) => match resolve_slot(ctx.manager, s) {
            Ok(procs) => ctx.buffers.buffer(procs[0]).iter().collect(),
            Err(e) => return Response::error(e),
        },
    };
    let cutoff = since_ms.map(|ms| SystemTime::now() - Duration::from_millis(ms));
    let recent: Vec<&StyledLine> = lines
        .into_iter()
        .filter(|l| cutoff.is_none_or(|c| l.at >= c))
        .collect();
    let start = recent.len().saturating_sub(tail);
    let out = recent[start..]
        .iter()
        .map(|l| LogLine {
            seq: l.seq,
            at_ms: l
                .at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            r#gen: l.r#gen,
            proc: l.proc,
            name: ctx.manager.short_name(l.proc).to_string(),
            stream: match l.stream {
                StreamTag::Stdout => "stdout",
                StreamTag::Stderr => "stderr",
                StreamTag::Marker => "marker",
            }
            .to_string(),
            text: if color {
                String::from_utf8_lossy(&l.raw).into_owned()
            } else {
                l.plain()
            },
        })
        .collect();
    Response::Logs {
        ok: true,
        lines: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        std::os::unix::net::UnixListener::bind(&path)
            .map(drop)
            .unwrap();
        assert!(path.exists());
        // Other tests fork children; between fork and exec a child briefly
        // holds a copy of the (close-on-exec) listener fd we just dropped,
        // during which the probe connect still succeeds. Allow that window.
        let deadline = Instant::now() + Duration::from_secs(2);
        let again = loop {
            match Listener::bind(&path) {
                Ok(l) => break l,
                Err(BindError::AnotherInstance(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("stale socket is replaced: {e:?}"),
            }
        };
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
            let Event::Control { request, reply } = ev else {
                panic!("not a control event")
            };
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
        let config = crate::types::Config {
            grace_period: Duration::from_millis(200),
            ..Default::default()
        };
        let cmds: Vec<String> = cmds.iter().map(|s| s.to_string()).collect();
        let manager = ProcManager::spawn_all(&cmds, &config, tx);
        let names = (0..manager.len())
            .map(|p| manager.short_name(p).to_string())
            .collect();
        World {
            manager,
            buffers: BufferSet::new(cmds.len(), &config),
            ui: UiState::new(names),
            rx,
            dir: PathBuf::from("/p"),
        }
    }

    impl World {
        fn handle(&mut self, r: Request) -> Handled {
            handle(
                &r,
                Ctx {
                    manager: &mut self.manager,
                    buffers: &self.buffers,
                    ui: &mut self.ui,
                    project_dir: &self.dir,
                },
            )
        }
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if !self.manager.tick().is_empty() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("no transition");
        }
        fn drain(&mut self) {
            for ev in self.rx.try_iter() {
                if let Event::Line {
                    proc,
                    r#gen,
                    stream,
                    seq,
                    at,
                    bytes,
                } = ev
                    && self.manager.is_current(proc, r#gen)
                {
                    self.buffers
                        .push(StyledLine::parse(proc, r#gen, stream, seq, at, &bytes));
                }
            }
        }
    }

    fn now(h: Handled) -> Response {
        match h {
            Handled::Now(r) => r,
            other => panic!("expected Now, got {other:?}"),
        }
    }

    #[test]
    fn resolve_slot_by_name_index_and_all() {
        let mut w = world(&["sleep 30", "sleep 31"]);
        assert_eq!(
            resolve_slot(&w.manager, "sleep").unwrap(),
            vec![0],
            "first name match"
        );
        assert_eq!(resolve_slot(&w.manager, "2").unwrap(), vec![1]);
        assert_eq!(resolve_slot(&w.manager, "all").unwrap(), vec![0, 1]);
        let err = resolve_slot(&w.manager, "web").unwrap_err();
        assert!(err.contains("unknown slot \"web\""), "{err}");
        assert!(err.contains("sleep"), "lists the slots: {err}");
        assert!(resolve_slot(&w.manager, "3").is_err());
        assert!(resolve_slot(&w.manager, "0").is_err());
        w.manager.shutdown();
    }

    #[test]
    fn status_reports_every_slot() {
        let mut w = world(&["sleep 30", "exit 3"]);
        std::thread::sleep(Duration::from_millis(100));
        // Mirror what the main loop would do for the exited slot.
        w.ui.set_health(1, Health::ExitedErr(crate::types::ExitStatus::Code(3)));
        let Response::Status {
            ok,
            pid,
            dir,
            procs,
        } = now(w.handle(Request::Status))
        else {
            panic!()
        };
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
        let h = w.handle(Request::Restart {
            slot: "all".into(),
            wait: true,
        });
        let Handled::AfterTransitions { procs, partial } = h else {
            panic!("{h:?}")
        };
        assert_eq!(procs, HashSet::from([0]));
        let Response::Acted {
            started, skipped, ..
        } = partial
        else {
            panic!()
        };
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
        let r = now(w.handle(Request::Restart {
            slot: "all".into(),
            wait: false,
        }));
        assert!(matches!(r, Response::Error { .. }), "{r:?}");
        w.settle();
        w.manager.shutdown();
    }

    #[test]
    fn stop_start_kill_and_run_apply_the_right_primitives() {
        let mut w = world(&["sleep 30"]);
        let r = now(w.handle(Request::Stop {
            slot: "1".into(),
            wait: false,
        }));
        assert!(matches!(r, Response::Acted { .. }));
        w.settle();
        assert!(w.manager.is_dead(0));

        let r = now(w.handle(Request::Stop {
            slot: "1".into(),
            wait: false,
        }));
        let Response::Acted { skipped, .. } = r else {
            panic!()
        };
        assert_eq!(skipped[0].reason, "already stopped");

        let r = now(w.handle(Request::Start {
            slot: "sleep".into(),
            wait: false,
        }));
        assert!(matches!(r, Response::Acted { .. }));
        w.settle();
        assert!(!w.manager.is_dead(0));
        let Response::Acted { skipped, .. } = now(w.handle(Request::Start {
            slot: "1".into(),
            wait: false,
        })) else {
            panic!()
        };
        assert_eq!(skipped[0].reason, "already running");

        let r = now(w.handle(Request::Run {
            slot: "1".into(),
            cmd: vec![],
            wrap: Some("env FOO=1".into()),
            wait: false,
        }));
        assert!(matches!(r, Response::Acted { .. }), "{r:?}");
        w.settle();
        assert!(w.manager.is_override(0));
        assert_eq!(w.manager.current_command(0), "env FOO=1 sleep 30");

        let r = now(w.handle(Request::Run {
            slot: "all".into(),
            cmd: vec!["x".into()],
            wrap: None,
            wait: false,
        }));
        assert!(matches!(r, Response::Error { .. }), "run needs one slot");
        let r = now(w.handle(Request::Run {
            slot: "1".into(),
            cmd: vec![],
            wrap: None,
            wait: false,
        }));
        assert!(matches!(r, Response::Error { .. }), "run needs cmd or wrap");

        let r = now(w.handle(Request::Kill {
            slot: "1".into(),
            wait: false,
        }));
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
        while (w.buffers.buffer(0).len() < 2 || w.buffers.buffer(1).is_empty())
            && Instant::now() < deadline
        {
            w.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        let Response::Logs { lines, .. } = now(w.handle(Request::Logs {
            slot: Some("1".into()),
            tail: 100,
            since_ms: None,
            color: false,
        })) else {
            panic!()
        };
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(lines[0].name, "printf");
        assert_eq!(lines[0].stream, "stdout");

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs {
            slot: Some("1".into()),
            tail: 1,
            since_ms: None,
            color: true,
        })) else {
            panic!()
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "\x1b[31mb\x1b[0m");

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs {
            slot: None,
            tail: 100,
            since_ms: None,
            color: false,
        })) else {
            panic!()
        };
        assert_eq!(lines.len(), 3);
        assert!(
            lines.windows(2).all(|p| p[0].seq < p[1].seq),
            "all = arrival order"
        );

        let Response::Logs { lines, .. } = now(w.handle(Request::Logs {
            slot: None,
            tail: 100,
            since_ms: Some(0),
            color: false,
        })) else {
            panic!()
        };
        assert!(lines.is_empty(), "since 0 ms ago excludes everything");

        assert!(matches!(
            now(w.handle(Request::Logs {
                slot: Some("nope".into()),
                tail: 1,
                since_ms: None,
                color: false,
            })),
            Response::Error { .. }
        ));
        w.manager.shutdown();
    }

    #[test]
    fn quit_is_reported_as_such() {
        let mut w = world(&["sleep 30"]);
        assert!(matches!(
            w.handle(Request::Quit),
            Handled::Quit(Response::Done { ok: true })
        ));
        w.manager.shutdown();
    }
}
