//! Shared vocabulary for krawatte.
//!
//! This module is the contract every other module depends on. It defines the
//! process identifier, the stream tag, the cross-thread event enum carried over
//! the `mpsc` channel, process health, and the runtime configuration. It has no
//! dependencies on `buffer`, `proc`, or `ui`, so it can be built and reasoned
//! about in isolation.

use std::time::Duration;

/// Stable index identifying a single child process, `0..N` in CLI argument order.
pub type ProcId = usize;

/// Monotonically increasing global sequence number assigned to each line as it
/// arrives, across all processes and both streams. Used to reconstruct arrival
/// order when interleaving buffers in the all-view.
pub type Seq = u64;

/// Which of a child's two pipes a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamTag {
    Stdout,
    Stderr,
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
    /// number; `bytes` is the raw line without its trailing newline (ANSI
    /// escapes still embedded, parsed downstream by the buffer).
    Line {
        proc: ProcId,
        stream: StreamTag,
        seq: Seq,
        bytes: Vec<u8>,
    },
    /// A child process exited and was reaped.
    Exited { proc: ProcId, status: ExitStatus },
    /// A command failed to spawn.
    SpawnFailed {
        proc: ProcId,
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
