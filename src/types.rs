//! Shared vocabulary for krawatte.
//!
//! This module is the contract every other module depends on. It defines the
//! process identifier, the stream tag, the cross-thread event enum carried over
//! the `mpsc` channel, process health, and the runtime configuration. It has no
//! dependencies on `buffer`, `proc`, or `ui`, so it can be built and reasoned
//! about in isolation.

use std::time::{Duration, SystemTime};

/// Stable index identifying a single child process, `0..N` in CLI argument order.
pub type ProcId = usize;

/// Monotonically increasing global sequence number assigned to each line as it
/// arrives, across all processes and both streams. Used to reconstruct arrival
/// order when interleaving buffers in the all-view.
pub type Seq = u64;

/// Generation counter of a slot: `0` for the initial spawn, incremented every
/// time the slot is respawned. Events carry the generation they came from so
/// the UI can drop output from a generation that has since been replaced.
pub type Gen = u32;

/// Which of a child's two pipes a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamTag {
    Stdout,
    Stderr,
    /// A line krawatte inserted into a slot's buffer itself (a restart marker),
    /// not process output. Rendered dim, without the stderr marker.
    Marker,
}

/// The exit outcome of a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Process exited with the given code.
    Code(i32),
    /// Process was terminated by the given signal number.
    Signal(i32),
}

/// Health of a process slot, as shown in the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Process is alive and running.
    Running,
    /// The current generation is being torn down ahead of a respawn.
    // TODO(Task 7): drop the allow once main.rs sets this on restart.
    #[allow(dead_code)]
    Restarting,
    /// Process exited cleanly (`exit 0`).
    ExitedOk,
    /// Process exited with a non-zero code or was signalled.
    ExitedErr(ExitStatus),
    /// The command could not be spawned at all.
    SpawnFailed,
}

/// Events sent by process-manager threads to the UI thread over the shared
/// `mpsc` channel. This is the single message type on the channel.
#[derive(Debug)]
pub enum Event {
    /// A full line arrived from a child stream. `seq` is the global sequence
    /// number; `at` is the wall-clock arrival time, stamped in the reader
    /// thread and used only for display; `bytes` is the raw line without its
    /// trailing newline (ANSI escapes still embedded, parsed downstream by the
    /// buffer).
    Line {
        proc: ProcId,
        #[allow(dead_code)]
        r#gen: Gen,
        stream: StreamTag,
        seq: Seq,
        at: SystemTime,
        bytes: Vec<u8>,
    },
    /// A child process exited and was reaped.
    Exited {
        proc: ProcId,
        #[allow(dead_code)]
        r#gen: Gen,
        status: ExitStatus,
    },
    /// A command failed to spawn.
    SpawnFailed {
        proc: ProcId,
        #[allow(dead_code)]
        r#gen: Gen,
        #[allow(dead_code)]
        error: String,
    },
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Grace period between SIGTERM and SIGKILL during shutdown.
    pub grace_period: Duration,
    /// Maximum number of lines retained per process ring buffer.
    pub buffer_cap: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            grace_period: Duration::from_secs(5),
            buffer_cap: 10_000,
        }
    }
}
