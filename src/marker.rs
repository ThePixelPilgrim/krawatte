//! Text of the marker block a slot transition writes into its buffer.
//!
//! Pure formatting: one topic per line so no single line grows long. The only
//! unbounded field, the command, gets a line of its own and is clipped or
//! wrapped by the UI like any other line.

use std::time::Duration;

use crate::proc::{Outcome, Transition};
use crate::types::{ExitStatus, Trigger};

/// The lines describing a completed transition, in buffer order. `clock` is
/// the already-formatted local time of the transition (`HH:MM:SS`);
/// formatting time is the UI's job, since only it knows the timezone.
pub fn restart_block(t: &Transition, clock: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(5);

    let header = match &t.old {
        Some(o) => format!("restart · gen {} → {}", o.r#gen, t.new.r#gen),
        None => format!("start · gen {}", t.new.r#gen),
    };
    lines.push(rule(&format!(
        "{header} · {clock} · {}",
        trigger_label(&t.trigger)
    )));

    if let Trigger::Watch { paths, more } = &t.trigger {
        let listed: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let suffix = if *more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        lines.push(rule(&format!("changed: {}{suffix}", listed.join(", "))));
    }

    match &t.old {
        Some(o) => {
            let outcome = match o.outcome {
                Outcome::Exited(ExitStatus::Code(c)) => format!("exit {c}"),
                Outcome::Exited(ExitStatus::Signal(s)) => format!("killed by signal {s}"),
                Outcome::Abandoned => "abandoned".to_string(),
            };
            lines.push(rule(&format!(
                "gen {}: pid {} · {} · ran {}",
                o.r#gen,
                o.pid,
                outcome,
                fmt_duration(o.ran)
            )));
        }
        None => {
            let previous = t.new.r#gen.saturating_sub(1);
            lines.push(rule(&format!("gen {previous}: never started")));
        }
    }

    let n = &t.new;
    match &n.spawn {
        Ok(pid) => lines.push(rule(&format!("gen {}: pid {}", n.r#gen, pid))),
        Err(e) => lines.push(rule(&format!("gen {}: spawn failed: {}", n.r#gen, e))),
    }
    lines.push(rule(&format!("cmd: {}", n.command)));
    lines
}

fn rule(text: &str) -> String {
    format!("── {text} ──")
}

/// Short trigger text for the header: `key r`, `key k`, `watch`.
fn trigger_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Key(c) => format!("key {c}"),
        Trigger::Watch { .. } => "watch".to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{NewGen, OldGen};
    use crate::types::Trigger;
    use std::path::PathBuf;

    fn old(r#gen: u32, outcome: Outcome, ran_secs: u64) -> OldGen {
        OldGen {
            r#gen,
            pid: 47105,
            outcome,
            ran: Duration::from_secs(ran_secs),
        }
    }

    fn new(r#gen: u32, spawn: Result<i32, String>) -> NewGen {
        NewGen {
            r#gen,
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
        assert_eq!(
            restart_block(&one, "x")[1],
            "── changed: target/debug/erhebimus ──"
        );
    }

    #[test]
    fn restart_block_covers_every_old_outcome() {
        let exit = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Exited(ExitStatus::Code(101)), 3)),
            new: new(1, Ok(1)),
            trigger: Trigger::Key('r'),
        };
        assert_eq!(
            restart_block(&exit, "x")[1],
            "── gen 0: pid 47105 · exit 101 · ran 3s ──"
        );
        let abandoned = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Abandoned, 7322)),
            new: new(1, Ok(1)),
            trigger: Trigger::Key('r'),
        };
        assert_eq!(
            restart_block(&abandoned, "x")[1],
            "── gen 0: pid 47105 · abandoned · ran 2h02m ──"
        );
        let never = Transition {
            proc: 0,
            old: None,
            new: new(1, Ok(1)),
            trigger: Trigger::Key('r'),
        };
        let lines = restart_block(&never, "x");
        assert_eq!(lines[0], "── start · gen 1 · x · key r ──");
        assert_eq!(lines[1], "── gen 0: never started ──");
    }

    #[test]
    fn restart_block_reports_spawn_failure() {
        let failed = Transition {
            proc: 0,
            old: Some(old(0, Outcome::Exited(ExitStatus::Code(0)), 1)),
            new: new(1, Err("No such file or directory".to_string())),
            trigger: Trigger::Key('r'),
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
