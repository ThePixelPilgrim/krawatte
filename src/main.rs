//! krawatte — multi-process tail TUI.
//!
//! Spawns each CLI argument as a child command (`sh -c`), follows their output
//! in a full-screen ratatui interface, and on `q`/Ctrl-C runs an orderly
//! TERM -> grace -> KILL shutdown. A drop guard always restores the terminal
//! (and kills children) even on panic.

mod buffer;
mod proc;
mod types;
mod ui;

use std::io::{self, Stdout};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::buffer::{BufferSet, StyledLine};
use crate::proc::ProcManager;
use crate::types::{Config, Event, ExitStatus, Health};
use crate::ui::{Action, UiState};

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

fn main() {
    let commands: Vec<String> = std::env::args().skip(1).collect();
    if commands.is_empty() {
        eprintln!("usage: krawatte <command> [<command> ...]");
        eprintln!("  each argument is a shell command run via `sh -c`");
        std::process::exit(2);
    }

    let config = Config::default();
    match run(&commands, &config) {
        Ok((names, statuses)) => {
            print_final_statuses(&names, &statuses);
        }
        Err(e) => {
            eprintln!("krawatte: fatal error: {e}");
            std::process::exit(1);
        }
    }
}

/// Set up the terminal, spawn children, run the event loop, then shut down.
/// Returns the per-process final statuses (indexed by [`ProcId`]) once the
/// terminal has been restored.
type RunResult = (Vec<String>, Vec<Option<ExitStatus>>);

fn run(commands: &[String], config: &Config) -> io::Result<RunResult> {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut manager = ProcManager::spawn_all(commands, config, tx);
    let mut buffers = BufferSet::new(commands.len(), config);

    // Short display names for the status bar (and the final printout). Captured
    // up front so both the live UI and the post-shutdown summary can label slots.
    let names: Vec<String> = (0..manager.len())
        .map(|p| manager.short_name(p).to_string())
        .collect();
    let mut ui = UiState::new(names.clone());

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

    Ok((names, statuses))
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
            && ui.handle_key(key) == Action::Quit
        {
            return Ok(());
        }

        // Drain all currently-available process events into the buffers / UI.
        drain_events(rx, buffers, ui, manager);
    }
}

/// Apply every currently-pending process event to the buffers and UI health.
fn drain_events(
    rx: &mpsc::Receiver<Event>,
    buffers: &mut BufferSet,
    ui: &mut UiState,
    manager: &ProcManager,
) {
    let n = manager.len();
    for ev in rx.try_iter() {
        match ev {
            Event::Line {
                proc,
                stream,
                seq,
                bytes,
            } => {
                if proc < n {
                    buffers.push(StyledLine::parse(proc, stream, seq, &bytes));
                }
            }
            Event::Exited { proc, status } => {
                ui.set_health(proc, health_from_exit(status));
            }
            Event::SpawnFailed { proc, .. } => {
                ui.set_health(proc, Health::SpawnFailed);
            }
        }
    }
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
fn print_final_statuses(names: &[String], statuses: &[Option<ExitStatus>]) {
    println!("krawatte: all children stopped.");
    for (proc, name) in names.iter().enumerate() {
        let status = statuses.get(proc).copied().flatten();
        let desc = match status {
            Some(ExitStatus::Code(0)) => "exit 0".to_string(),
            Some(ExitStatus::Code(c)) => format!("exit {c}"),
            Some(ExitStatus::Signal(s)) => format!("killed by signal {s}"),
            None => "did not start".to_string(),
        };
        println!("  [{}] {:<20} {}", proc + 1, name, desc);
    }
}
