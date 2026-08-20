//! krawatte — multi-process tail TUI.
//!
//! Spawns each CLI argument as a child command (`sh -c`), follows their output
//! in a full-screen ratatui interface, and on `q`/Ctrl-C runs an orderly
//! TERM -> grace -> KILL shutdown. A drop guard always restores the terminal
//! (and kills children) even on panic.

mod buffer;
mod config;
mod marker;
mod proc;
mod types;
mod ui;
mod watch;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event as CtEvent, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::buffer::{BufferSet, StyledLine};
use crate::config::ProcSpec;
use crate::proc::{Outcome, ProcManager, Transition};
use crate::types::{Config, Event, ExitStatus, Health, Trigger};
use crate::ui::{Action, UiState};
use crate::watch::SlotWatch;

/// RAII guard that restores the terminal to a sane state (leave alternate
/// screen, disable raw mode) on drop. Constructed after entering raw mode /
/// alternate screen so that any later panic still unwinds through this drop and
/// leaves the user's terminal usable.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best effort: never panic in a drop.
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Full-screen terminal multi-tail: run several programs, follow their output
/// interleaved or per pane, shut them all down with one Ctrl-C.
///
/// With no COMMAND arguments, launches the nearest Krawattefile (in the
/// current directory or a parent) or the one given with --file.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Grace period in seconds between SIGTERM and SIGKILL during shutdown.
    /// Overrides a Krawattefile's settings.timeout [default: 5].
    #[arg(short, long, value_name = "SECS")]
    timeout: Option<f64>,

    /// Krawattefile to launch instead of searching for one.
    #[arg(short, long, value_name = "PATH", conflicts_with = "commands")]
    file: Option<PathBuf>,

    /// Shell commands to run ad hoc; each argument is passed to `sh -c`.
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    commands: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let launch = match resolve_launch(&cli) {
        Ok(launch) => launch,
        Err(messages) => {
            for m in messages {
                eprintln!("krawatte: {m}");
            }
            std::process::exit(2);
        }
    };
    let config = Config {
        grace_period: grace_period(cli.timeout, launch.timeout),
        ..Config::default()
    };
    let (tx, rx) = mpsc::channel::<Event>();
    // Watches are registered before anything is spawned or drawn, so a
    // registration failure is a clean exit 2 and the build slot's first
    // output is already observed.
    if let (Some(project_dir), false) = (&launch.project_dir, launch.watches.is_empty())
        && let Err(e) = watch::start(
            launch.watches.clone(),
            project_dir.clone(),
            launch.debounce,
            tx.clone(),
        )
    {
        eprintln!("krawatte: {e}");
        std::process::exit(2);
    }
    let watched: Vec<bool> = (0..launch.specs.len())
        .map(|p| launch.watches.iter().any(|w| w.proc == p))
        .collect();
    match run(&launch.specs, &watched, &config, tx, rx) {
        Ok((names, started, statuses)) => {
            print_final_statuses(&names, &started, &statuses);
        }
        Err(e) => {
            eprintln!("krawatte: fatal error: {e}");
            std::process::exit(1);
        }
    }
}

/// Everything `main` decides before the terminal is touched.
#[derive(Debug)]
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

/// Where the cluster comes from: positional commands run ad hoc; otherwise
/// the Krawattefile given with `-f`, or the nearest one above the current
/// directory. Errors are complete, user-facing messages (without the
/// `krawatte:` prefix), all of them at once; watch resolution only runs on
/// a file that parsed cleanly.
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
    let path = match &cli.file {
        Some(path) => path.clone(),
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| vec![format!("cannot determine the current directory: {e}")])?;
            config::discover(&cwd).ok_or_else(|| {
                vec![format!(
                    "no {} found in {} or any parent directory (pass commands to run ad hoc, or -f PATH)",
                    config::FILE_NAME,
                    cwd.display()
                )]
            })?
        }
    };
    let as_messages = |errors: Vec<config::ConfigError>| {
        errors.iter().map(ToString::to_string).collect::<Vec<_>>()
    };
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

/// An explicit `-t` wins over the file's `settings.timeout`, which wins over
/// the default of five seconds. Negative values clamp to zero.
fn grace_period(cli_secs: Option<f64>, file_timeout: Option<Duration>) -> Duration {
    cli_secs
        .map(|t| Duration::from_secs_f64(t.max(0.0)))
        .or(file_timeout)
        .unwrap_or(Duration::from_secs(5))
}

/// Set up the terminal, spawn children, run the event loop, then shut down.
/// Returns the per-process final statuses (indexed by [`ProcId`]) once the
/// terminal has been restored.
type RunResult = (Vec<String>, Vec<bool>, Vec<Option<ExitStatus>>);

fn run(
    specs: &[ProcSpec],
    watched: &[bool],
    config: &Config,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
) -> io::Result<RunResult> {
    let mut manager = ProcManager::spawn_specs(specs, config, tx);
    let mut buffers = BufferSet::new(specs.len(), config);

    // Short display names for the status bar (and the final printout). Captured
    // up front so both the live UI and the post-shutdown summary can label slots.
    let names: Vec<String> = (0..manager.len())
        .map(|p| manager.short_name(p).to_string())
        .collect();
    let started: Vec<bool> = (0..manager.len()).map(|p| manager.was_started(p)).collect();
    let mut ui = UiState::new(names.clone());
    ui.set_watched(watched.to_vec());

    // Enter raw mode + alternate screen; the guard restores them on any exit
    // path including panic. Children are killed by `manager.shutdown()` below,
    // which runs before the manager is dropped on the normal path.
    let statuses = {
        let _guard = TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;

        event_loop(&mut terminal, &mut manager, &mut buffers, &mut ui, &rx)?;

        // Orderly shutdown while still inside the alternate screen; collect
        // final statuses, then drop the guard to restore the terminal.
        manager.shutdown()
    };

    Ok((names, started, statuses))
}

/// The main event loop: redraw, then wait briefly for a crossterm input event
/// and drain any pending process events. Exits when the user quits.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    manager: &mut ProcManager,
    buffers: &mut BufferSet,
    ui: &mut UiState,
    rx: &mpsc::Receiver<Event>,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui.render(frame, buffers))?;

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
                    if manager.replace(p, command, Trigger::Key('r')) {
                        ui.set_health(p, Health::Restarting);
                    }
                }
                Action::Kill(p) => {
                    if manager.kill(p, Trigger::Key('k')) {
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
    }
}

/// Apply every currently-pending process event to the buffers and UI health.
///
/// Events from a generation other than the slot's current one are stale --
/// late output from a replaced process, or from a grandchild that escaped its
/// group and still holds the old pipe -- and are dropped. While a teardown is
/// in flight the dying generation's lines are still shown (its shutdown output
/// is real), but its exit is not: the slot is `Restarting`, not `✖ sig 15`.
/// A `Changed` event restarts its slot with the configured command unless a
/// restart is already in flight, in which case it is dropped.
fn drain_events(
    rx: &mpsc::Receiver<Event>,
    buffers: &mut BufferSet,
    ui: &mut UiState,
    manager: &mut ProcManager,
) {
    for ev in rx.try_iter() {
        match ev {
            Event::Line {
                proc,
                r#gen,
                stream,
                seq,
                at,
                bytes,
            } => {
                if manager.is_current(proc, r#gen) {
                    buffers.push(StyledLine::parse(proc, r#gen, stream, seq, at, &bytes));
                }
            }
            Event::Exited {
                proc,
                r#gen,
                status,
            } => {
                if manager.is_current(proc, r#gen) && !manager.is_restarting(proc) {
                    ui.set_health(proc, health_from_exit(status));
                }
            }
            Event::SpawnFailed { proc, r#gen, .. } => {
                if manager.is_current(proc, r#gen) {
                    ui.set_health(proc, Health::SpawnFailed);
                }
            }
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
        buffers.push(StyledLine::marker(
            t.proc,
            manager.current_gen(t.proc),
            manager.next_seq(),
            at,
            text,
        ));
    }
    let health = match (&t.new, &t.old) {
        (Some(n), _) => match n.spawn {
            Ok(_) => Health::Running,
            Err(_) => Health::SpawnFailed,
        },
        // Stopped: show how the retired generation ended. An abandoned one
        // was sent SIGKILL, the closest thing the bar can say.
        (None, Some(o)) => match o.outcome {
            Outcome::Exited(status) => health_from_exit(status),
            Outcome::Abandoned => Health::ExitedErr(ExitStatus::Signal(9)),
        },
        (None, None) => Health::SpawnFailed,
    };
    ui.set_health(t.proc, health);
}

/// Map a terminal exit status to a [`Health`] for the status bar.
fn health_from_exit(status: ExitStatus) -> Health {
    match status {
        ExitStatus::Code(0) => Health::ExitedOk,
        other => Health::ExitedErr(other),
    }
}

/// After the terminal is restored, print each child's final status to the
/// normal screen.
fn print_final_statuses(names: &[String], started: &[bool], statuses: &[Option<ExitStatus>]) {
    println!("krawatte: all children stopped.");
    for (proc, name) in names.iter().enumerate() {
        let status = statuses.get(proc).copied().flatten();
        let desc = status_label(started.get(proc).copied().unwrap_or(false), status);
        println!("  [{}] {:<20} {}", proc + 1, name, desc);
    }
}

/// Describe one slot's outcome. A missing status means the child never spawned
/// (`started == false`) or survived even SIGKILL and was abandoned so that
/// shutdown could finish (`started == true`).
fn status_label(started: bool, status: Option<ExitStatus>) -> String {
    match status {
        Some(ExitStatus::Code(c)) => format!("exit {c}"),
        Some(ExitStatus::Signal(s)) => format!("killed by signal {s}"),
        None if started => "did not exit (abandoned)".to_string(),
        None => "did not start".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FILE_NAME, Watch};
    use crate::types::{Changed, Gen, StreamTag, Trigger};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("krawatte").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_accepts_no_arguments_and_rejects_file_with_commands() {
        assert!(cli(&[]).commands.is_empty());
        assert_eq!(cli(&["-t", "2", "a", "b"]).commands, vec!["a", "b"]);
        assert_eq!(
            cli(&["-f", "x/Krawattefile"]).file,
            Some(PathBuf::from("x/Krawattefile"))
        );
        assert!(Cli::try_parse_from(["krawatte", "-f", "x", "cmd"]).is_err());
    }

    #[test]
    fn positional_commands_become_adhoc_specs() {
        let launch = resolve_launch(&cli(&["npm run dev", "cargo check"])).unwrap();
        let (specs, timeout) = (launch.specs, launch.timeout);
        assert_eq!(timeout, None);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "npm");
        assert_eq!(specs[0].cwd, None);
        assert_eq!(specs[1].command, "cargo check");
    }

    #[test]
    fn explicit_file_is_loaded_and_its_timeout_returned() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        fs::write(
            &file,
            "[settings]\ntimeout = 1.5\n[[proc]]\nname = \"a\"\ncmd = \"./app\"\nwatch = \"self\"\n",
        )
        .unwrap();
        let launch = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap();
        let (specs, timeout) = (launch.specs, launch.timeout);
        assert_eq!(timeout, Some(Duration::from_secs_f64(1.5)));
        assert_eq!(specs[0].name, "a");
        assert_eq!(
            specs[0].cwd.as_deref(),
            Some(dir.path().canonicalize().unwrap().as_path())
        );
        assert_eq!(specs[0].watch, Watch::SelfBinary);
    }

    #[test]
    fn config_errors_are_returned_as_messages() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        fs::write(&file, "[[proc]]\nname = \"all\"\ncmd = \"true\"\n").unwrap();
        let errs = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("is reserved"), "{}", errs[0]);

        let errs = resolve_launch(&cli(&["-f", "/nonexistent/Krawattefile"])).unwrap_err();
        assert!(errs[0].contains("cannot read"), "{}", errs[0]);
    }

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
        fs::write(
            &file,
            "[[proc]]\nname = \"all\"\ncmd = \"npm x\"\nwatch = \"self\"\n",
        )
        .unwrap();
        let errs = resolve_launch(&cli(&["-f", file.to_str().unwrap()])).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].contains("is reserved"),
            "parse errors come first and stop resolution: {errs:#?}"
        );

        fs::write(
            &file,
            "[[proc]]\nname = \"web\"\ncmd = \"npm x\"\nwatch = \"self\"\n",
        )
        .unwrap();
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
        let changed = || {
            Event::Changed(Changed {
                proc: 0,
                paths: vec!["src/a.rs".into()],
                more: 0,
            })
        };

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
        assert_eq!(
            t.trigger,
            Trigger::Watch {
                paths: vec!["src/a.rs".into()],
                more: 0
            }
        );
        assert_eq!(t.new.unwrap().command, "sleep 30");
        assert!(
            !manager.is_restarting(0),
            "the dropped change did not queue another"
        );
        manager.shutdown();
    }

    #[test]
    fn grace_period_prefers_cli_then_file_then_default() {
        assert_eq!(
            grace_period(Some(2.0), Some(Duration::from_secs(7))),
            Duration::from_secs(2)
        );
        assert_eq!(
            grace_period(None, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(grace_period(None, None), Duration::from_secs(5));
        assert_eq!(grace_period(Some(-1.0), None), Duration::ZERO);
    }

    fn line(proc: usize, r#gen: Gen, text: &str) -> Event {
        Event::Line {
            proc,
            r#gen,
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

        assert!(manager.replace(0, "sleep 30".to_string(), Trigger::Key('r')));
        ui.set_health(0, Health::Restarting);

        // Mid-teardown: output from the dying generation is still shown, but
        // its exit must not flip the health away from Restarting.
        tx.send(line(0, 0, "shutting down")).unwrap();
        tx.send(Event::Exited {
            proc: 0,
            r#gen: 0,
            status: ExitStatus::Signal(15),
        })
        .unwrap();
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        assert_eq!(buffers.buffer(0).len(), 1);
        assert_eq!(ui.health(0), Health::Restarting);

        // Drive the restart to completion.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut transitions = Vec::new();
        while transitions.is_empty() && Instant::now() < deadline {
            drain_events(&rx, &mut buffers, &mut ui, &mut manager);
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
        drain_events(&rx, &mut buffers, &mut ui, &mut manager);
        assert_eq!(buffers.buffer(0).len(), 6);
        assert_eq!(ui.health(0), Health::Running);

        manager.shutdown();
    }

    #[test]
    fn status_label_distinguishes_never_started_from_never_exited() {
        // A slot with no status can mean two very different things, and calling
        // an abandoned process "did not start" misreports a process that may
        // have been running the whole session.
        assert_eq!(status_label(false, None), "did not start");
        assert_eq!(status_label(true, None), "did not exit (abandoned)");
        assert_eq!(status_label(true, Some(ExitStatus::Code(0))), "exit 0");
        assert_eq!(status_label(true, Some(ExitStatus::Code(3))), "exit 3");
        assert_eq!(
            status_label(true, Some(ExitStatus::Signal(9))),
            "killed by signal 9"
        );
    }
}
