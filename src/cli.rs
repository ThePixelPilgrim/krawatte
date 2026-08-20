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
use crate::marker::fmt_duration;
use crate::protocol::{Envelope, LogLine, PROTOCOL_VERSION, Request, Response};

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
    Kill {
        slot: String,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Tear a slot down and leave it stopped.
    Stop {
        slot: String,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Start a stopped slot's standard command.
    Start {
        slot: String,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
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
    Quit {
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
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
            Sub::Status { json }
            | Sub::Quit { json, .. }
            | Sub::Logs { json, .. }
            | Sub::Restart { json, .. }
            | Sub::Kill { json, .. }
            | Sub::Stop { json, .. }
            | Sub::Start { json, .. }
            | Sub::Run { json, .. } => *json,
        }
    }

    fn waits(&self) -> bool {
        matches!(
            self,
            Sub::Restart { wait: true, .. }
                | Sub::Kill { wait: true, .. }
                | Sub::Stop { wait: true, .. }
                | Sub::Start { wait: true, .. }
                | Sub::Run { wait: true, .. }
                | Sub::Quit { wait: true, .. }
        )
    }
}

/// The protocol request for a subcommand.
pub fn request_for(sub: &Sub) -> Result<Request, String> {
    Ok(match sub {
        Sub::Status { .. } => Request::Status,
        Sub::Restart { slot, wait, .. } => Request::Restart {
            slot: slot.clone(),
            wait: *wait,
        },
        Sub::Kill { slot, wait, .. } => Request::Kill {
            slot: slot.clone(),
            wait: *wait,
        },
        Sub::Stop { slot, wait, .. } => Request::Stop {
            slot: slot.clone(),
            wait: *wait,
        },
        Sub::Start { slot, wait, .. } => Request::Start {
            slot: slot.clone(),
            wait: *wait,
        },
        Sub::Run {
            slot,
            wrap,
            wait,
            cmd,
            ..
        } => {
            if cmd.is_empty() && wrap.is_none() {
                return Err("run needs a command after `--` or --wrap PREFIX".into());
            }
            Request::Run {
                slot: slot.clone(),
                cmd: cmd.clone(),
                wrap: wrap.clone(),
                wait: *wait,
            }
        }
        Sub::Quit { .. } => Request::Quit,
        Sub::Logs {
            slot,
            tail,
            since,
            color,
            ..
        } => Request::Logs {
            slot: slot.clone(),
            tail: *tail,
            since_ms: match since {
                Some(s) => Some(parse_duration(s)?.as_millis() as u64),
                None => None,
            },
            color: *color,
        },
    })
}

/// The project an instance is keyed by: the given file's directory, else
/// the nearest Krawattefile's, else the cwd itself (an ad-hoc instance).
pub fn project_dir_for(file: Option<&Path>, cwd: &Path) -> Result<PathBuf, String> {
    let dir = match file {
        Some(f) => f
            .canonicalize()
            .map_err(|e| format!("{}: {e}", f.display()))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "file has no parent".to_string())?,
        None => match config::discover(cwd) {
            Some(f) => f
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cwd.to_path_buf()),
            None => cwd.to_path_buf(),
        },
    };
    dir.canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Exit status for a reply: 1 if the instance refused, else 0.
pub fn exit_code(resp: &Response) -> i32 {
    if matches!(resp, Response::Error { .. }) {
        1
    } else {
        0
    }
}

/// Connect, send, receive, print. Returns the process exit code.
pub fn run_client(sub: &Sub, file: Option<&Path>) -> i32 {
    let request = match request_for(sub) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("krawatte: {e}");
            return 2;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("krawatte: {e}");
            return 2;
        }
    };
    let dir = match project_dir_for(file, &cwd) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("krawatte: {e}");
            return 2;
        }
    };
    let path = socket_path(&dir);
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("krawatte: no krawatte running for {}", dir.display());
            return 3;
        }
    };
    let timeout = if sub.waits() {
        Duration::from_secs(600)
    } else {
        Duration::from_secs(10)
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let mut text = serde_json::to_string(&Envelope {
        v: PROTOCOL_VERSION,
        request,
    })
    .expect("serializable");
    text.push('\n');
    if let Err(e) = stream.write_all(text.as_bytes()) {
        eprintln!("krawatte: send: {e}");
        return 1;
    }
    let mut line = String::new();
    match BufReader::new(&stream).read_line(&mut line) {
        Ok(0) if matches!(sub, Sub::Quit { wait: true, .. }) => {
            println!("krawatte: instance exited");
            return 0;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("krawatte: no reply: {e}");
            return 1;
        }
    }
    let resp: Response = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("krawatte: bad reply: {e}: {line}");
            return 1;
        }
    };
    let out = render(sub, &resp);
    if exit_code(&resp) == 0 {
        print!("{out}");
    } else {
        eprint!("{out}");
    }
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
        Response::Status {
            pid, dir, procs, ..
        } => {
            let mut out = format!("krawatte {pid} · {dir}\n");
            let name_w = procs
                .iter()
                .map(|p| p.name.len() + usize::from(p.r#override))
                .max()
                .unwrap_or(0);
            for p in procs {
                let name = if p.r#override {
                    format!("{}*", p.name)
                } else {
                    p.name.clone()
                };
                let state = match p.pid {
                    Some(pid) => format!("{} pid {pid}", p.health),
                    None => p.health.clone(),
                };
                let since = p
                    .since_ms
                    .map(|ms| fmt_duration(Duration::from_millis(ms)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "[{}] {:<name_w$}  {:<18} gen {:<3} {:<7} {}\n",
                    p.index, name, state, p.r#gen, since, p.command
                ));
            }
            out
        }
        Response::Acted {
            started,
            skipped,
            markers,
            ..
        } => {
            let verb = match sub {
                Sub::Kill { .. } => "killing",
                Sub::Stop { .. } => "stopping",
                Sub::Start { .. } => "starting",
                Sub::Run { .. } => "running override in",
                _ => "restarting",
            };
            let mut out = String::new();
            for s in started {
                match s.from_gen {
                    Some(g) => out.push_str(&format!("{}: {verb} (gen {g})\n", s.name)),
                    None => out.push_str(&format!("{}: {verb}\n", s.name)),
                }
            }
            for s in skipped {
                out.push_str(&format!("skipped: {} ({})\n", s.name, s.reason));
            }
            if let Some(m) = markers {
                for line in m {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out
        }
        Response::Logs { lines, .. } => {
            let all = matches!(sub, Sub::Logs { slot: None, .. })
                || matches!(sub, Sub::Logs { slot: Some(s), .. } if s == "all");
            let tz = TimeZone::system();
            let mut out = String::new();
            for l in lines {
                out.push_str(&format_log_line(l, all, &tz));
                out.push('\n');
            }
            out
        }
    }
}

fn format_log_line(l: &LogLine, with_name: bool, tz: &TimeZone) -> String {
    let clock = Timestamp::from_millisecond(l.at_ms as i64)
        .map(|ts| ts.to_zoned(tz.clone()).strftime("%H:%M:%S").to_string())
        .unwrap_or_else(|_| "??:??:??".to_string());
    if with_name {
        format!("{clock} {}│ {}", l.name, l.text)
    } else {
        format!("{clock} {}", l.text)
    }
}

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
        T::try_parse_from(std::iter::once("krawatte").chain(args.iter().copied()))
            .unwrap()
            .sub
    }

    #[test]
    fn subcommands_map_to_requests() {
        assert_eq!(request_for(&sub(&["status"])).unwrap(), Request::Status);
        assert_eq!(
            request_for(&sub(&["restart", "server", "--wait"])).unwrap(),
            Request::Restart {
                slot: "server".into(),
                wait: true
            }
        );
        assert_eq!(
            request_for(&sub(&["stop", "all"])).unwrap(),
            Request::Stop {
                slot: "all".into(),
                wait: false
            }
        );
        assert_eq!(
            request_for(&sub(&[
                "run",
                "server",
                "--",
                "perf",
                "record",
                "-g",
                "target/debug/app"
            ]))
            .unwrap(),
            Request::Run {
                slot: "server".into(),
                cmd: vec![
                    "perf".into(),
                    "record".into(),
                    "-g".into(),
                    "target/debug/app".into()
                ],
                wrap: None,
                wait: false
            }
        );
        assert_eq!(
            request_for(&sub(&["run", "server", "--wrap", "perf record -g"])).unwrap(),
            Request::Run {
                slot: "server".into(),
                cmd: vec![],
                wrap: Some("perf record -g".into()),
                wait: false
            }
        );
        assert!(
            request_for(&sub(&["run", "server"])).is_err(),
            "needs -- or --wrap"
        );
        assert_eq!(
            request_for(&sub(&[
                "logs", "server", "--tail", "5", "--since", "2m", "--color"
            ]))
            .unwrap(),
            Request::Logs {
                slot: Some("server".into()),
                tail: 5,
                since_ms: Some(120_000),
                color: true
            }
        );
        assert_eq!(
            request_for(&sub(&["logs"])).unwrap(),
            Request::Logs {
                slot: None,
                tail: 100,
                since_ms: None,
                color: false
            }
        );
        assert!(request_for(&sub(&["logs", "--since", "5"])).is_err());
    }

    #[test]
    fn project_dir_prefers_file_then_discovery_then_cwd() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("p");
        let deep = project.join("a");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            project_dir_for(None, &deep).unwrap(),
            deep.canonicalize().unwrap(),
            "no file: cwd (ad-hoc instance)"
        );
        std::fs::write(project.join(crate::config::FILE_NAME), "").unwrap();
        assert_eq!(
            project_dir_for(None, &deep).unwrap(),
            project.canonicalize().unwrap()
        );
        let other = root.path().join("q");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("Krawattefile"), "").unwrap();
        assert_eq!(
            project_dir_for(Some(&other.join("Krawattefile")), &deep).unwrap(),
            other.canonicalize().unwrap()
        );
        assert!(project_dir_for(Some(Path::new("/nonexistent/Krawattefile")), &deep).is_err());
    }

    #[test]
    fn human_rendering_and_exit_codes() {
        let status = Response::Status {
            ok: true,
            pid: 48001,
            dir: "/home/c/e".into(),
            procs: vec![
                ProcStatus {
                    index: 1,
                    name: "build".into(),
                    health: "exit 0".into(),
                    r#gen: 4,
                    pid: None,
                    command: "cargo build".into(),
                    standard: "cargo build".into(),
                    r#override: false,
                    since_ms: Some(12_000),
                },
                ProcStatus {
                    index: 2,
                    name: "server".into(),
                    health: "running".into(),
                    r#gen: 3,
                    pid: Some(48213),
                    command: "perf record -g app".into(),
                    standard: "app".into(),
                    r#override: true,
                    since_ms: Some(252_000),
                },
            ],
        };
        let text = render(&sub(&["status"]), &status);
        assert!(text.starts_with("krawatte 48001 · /home/c/e\n"), "{text}");
        assert!(text.contains("[1] build"), "{text}");
        assert!(text.contains("exit 0"), "{text}");
        assert!(text.contains("[2] server*"), "{text}");
        assert!(text.contains("pid 48213"), "{text}");
        assert!(text.contains("4m12s"), "{text}");
        assert!(text.contains("perf record -g app"), "{text}");
        assert_eq!(exit_code(&status), 0);

        let acted = Response::Acted {
            ok: true,
            started: vec![Started {
                proc: 0,
                name: "build".into(),
                from_gen: Some(4),
            }],
            skipped: vec![Skipped {
                proc: 1,
                name: "server".into(),
                reason: "restart in flight".into(),
            }],
            markers: Some(vec!["── restart · gen 4 → 5 · x · cli restart ──".into()]),
        };
        let text = render(&sub(&["restart", "all", "--wait"]), &acted);
        assert!(text.contains("build: restarting (gen 4)"), "{text}");
        assert!(
            text.contains("skipped: server (restart in flight)"),
            "{text}"
        );
        assert!(text.contains("── restart · gen 4 → 5"), "{text}");
        assert_eq!(exit_code(&acted), 0);

        let err = Response::error("unknown slot \"web\"");
        assert_eq!(
            render(&sub(&["restart", "web"]), &err),
            "krawatte: unknown slot \"web\"\n"
        );
        assert_eq!(exit_code(&err), 1);

        let json = render(&sub(&["status", "--json"]), &status);
        assert!(json.starts_with('{') && json.ends_with('\n'), "{json}");
        serde_json::from_str::<Response>(json.trim()).unwrap();
    }

    #[test]
    fn log_lines_render_with_clock_and_name_for_all() {
        let logs = Response::Logs {
            ok: true,
            lines: vec![LogLine {
                seq: 1,
                at_ms: 0,
                r#gen: 0,
                proc: 0,
                name: "build".into(),
                stream: "stdout".into(),
                text: "hi".into(),
            }],
        };
        let single = render(&sub(&["logs", "build"]), &logs);
        assert!(single.ends_with(" hi\n"), "{single}");
        assert!(!single.contains("build│"), "{single}");
        let all = render(&sub(&["logs"]), &logs);
        assert!(all.contains("build│ hi"), "{all}");
    }
}
