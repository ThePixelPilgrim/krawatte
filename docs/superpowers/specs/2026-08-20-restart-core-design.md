# Restart core (`r` / `k` hotkeys, generations)

Spec A of the [roadmap](2026-08-20-roadmap.md).

## Problem

A slot spawns once and dies once. Restarting a process means quitting
krawatte and relaunching the whole cluster, or wrapping the command in
watchexec — which puts the real process outside the group krawatte signals.

## Goal

A slot can be torn down and respawned in place, keeping its index, name and
buffer, without blocking the UI. The same primitive later serves
watch-triggered restarts and one-shot overrides, so it is modelled for those
callers now even though this spec only wires the two hotkeys.

## Behavior

- `r` in a single pane restarts that slot: TERM → grace → KILL the current
  generation, then spawn the same command again. The grace period is the
  existing `--timeout`.
- `k` in a single pane kills the current generation and applies its on-exit
  policy. In this spec every generation is a standard run, whose policy after
  an explicit kill is "respawn", so `k` and `r` behave identically; they
  diverge once overrides exist (see below).
- Both keys are silent no-ops in the all-view and while a restart is already
  in flight for that slot.
- Restarting a slot that is already dead — exited on its own, or never spawned
  (`SpawnFailed`, which may have been transient) — skips straight to spawning.
- A standard run that exits on its own stays dead. No crash-restart.
- The status bar shows `↻` for a slot whose generation is being torn down
  (`Health::Restarting`). Once the new generation is spawned it is `●` again;
  the old generation's signalled exit never shows as `✖`.
- The buffer is kept. When the new generation spawns, a block of *marker
  lines* is appended to the slot's buffer — dim, without a stream tag —
  exposing everything relevant about the transition. One topic per line so
  no line grows long; the only unbounded field, the command, gets its own
  line and is clipped/wrapped like any other line:

  ```
  ── restart · gen 2 → 3 · 14:02:11 ──
  ── gen 2: pid 47105 · killed by signal 15 · ran 4m12s ──
  ── gen 3: pid 48213 ──
  ── cmd: target/debug/erhebimus ──
  ```

  The old-generation line reports how it ended (`exit N`, `killed by signal
  N`, `abandoned` if it survived SIGKILL, `never started` for a slot that had
  no live generation) and how long it ran. If the new generation fails to
  spawn, its line reads `── gen 3: spawn failed: <error> ──`. Marker lines
  appear in the pane and the all-view like any other line and are scrolled,
  timestamped and wrapped the same way. Spec C adds a `trigger` field
  (`key r`, `key k`, `watch`, `cli`) to the header line once there is more
  than one way to get here; in this spec the trigger is always a key and is
  omitted.
- `q`/Ctrl-C during an in-flight restart shuts down normally and within the
  usual bound.

## Design

### Generations

Every spawn of a slot produces a *generation*: the pgid, the `dead` flag, the
status cell and the waiter handle that `Proc` holds today, plus a `gen: u32`
counter and the command string that was run. `Proc` becomes

```
struct Proc {
    standard: String,            // command from CLI/Krawattefile
    short: String,
    gen: u32,                    // current generation number
    live: Option<Generation>,    // None once confirmed gone or never spawned
    restart: Option<Restart>,    // in-flight teardown, see below
    on_exit: OnExit,             // policy for the current generation
}
```

`Event::Line`, `Event::Exited` and `Event::SpawnFailed` gain `gen`. The
reader and waiter threads capture the gen at spawn time. `drain_events` drops
any event whose `gen` is not the slot's current one. That is what keeps late
output from a killed process — or from a grandchild that escaped its group
and still holds the pipe — from appearing after the separator, and keeps the
old generation's `Exited(Signal 15)` from flipping the health to `✖`.

Invariant: a slot owns at most one generation that may still be alive. The
new generation is spawned only after the old one's group is confirmed gone.
This is why global shutdown needs no special case for a slot mid-restart: it
sees whatever pgid the slot currently holds.

### The restart primitive

```
pub enum OnExit { StayDead, SpawnStandard }

impl ProcManager {
    /// Tear down the current generation (if alive) and then spawn `command`
    /// in this slot. No-op if a restart is already in flight.
    pub fn replace(&mut self, proc: ProcId, command: String, on_exit: OnExit);
    /// Tear down the current generation and let its `on_exit` policy decide
    /// what runs next.
    pub fn kill(&mut self, proc: ProcId);
    /// Step every in-flight restart; spawn new generations whose teardown
    /// completed. Returns the slots that were respawned this tick so the
    /// caller can append separators and update health.
    pub fn tick(&mut self) -> Vec<Respawned>;
}
```

`r` = `replace(p, current_command, current_on_exit)`. `k` = `kill(p)`. In
this spec the only policy is `SpawnStandard` for explicitly killed standard
runs, so `kill` on a standard run is `replace(p, standard, SpawnStandard)`.
The override (spec C) will call `replace(p, wrapped, SpawnStandard)` and set
the slot's policy so that `kill` resumes the standard command instead; the
self-exit path will also consult `on_exit` then. `StayDead` is listed now so
the enum is complete; this spec's callers never pass it.

### Non-blocking teardown

`Restart` holds a single-process `ShutdownMachine` — the existing state
machine that global shutdown uses, with the same `ShutdownEffects` trait and
the same grace — plus the command and policy to apply when it finishes. The
main loop already wakes every 50 ms; it calls `manager.tick()` after draining
events. `tick` steps each machine with the real effects; when a machine
reports done (group gone, or abandoned after SIGKILL failed) the slot's old
generation is dropped, `gen` is incremented, the new command is spawned, and
the slot is reported back so `main` can push the separator and set health.

A slot with no live generation (dead, spawn-failed, or never started) has
nothing to tear down: `replace` spawns immediately in the same call and
reports through the next `tick`, so the caller has one code path.

Abandoned groups (a process that survives SIGKILL) are treated as done after
the existing bound, exactly as global shutdown does, so a restart can never
hang the UI. The new generation is spawned regardless; if the old one still
holds a port the new one fails on its own terms and that shows as `✖`.

### UI

`Action` gains `Restart(ProcId)` and `Kill(ProcId)`, returned by `handle_key`
only from a single-pane view; the all-view returns `Continue`. `Health` gains
`Restarting`, rendered `↻`. Each marker line is a `StyledLine` with a new
`StreamTag::Marker` (no tag column, dim style) so the buffer needs no
special-casing beyond rendering the tag. `Respawned` (returned by `tick`)
carries the old generation's pid, outcome and runtime, the new pid or spawn
error, and the command, so `main` can format the block without reaching into
the manager.

### Testing

- `ShutdownMachine` is already unit-tested through `StubEffects`; the restart
  path reuses it and adds no new machine logic.
- Integration-style tests with real `sh` children, as the existing
  `proc.rs` tests do:
  - restart of a live slot yields a new pid in the same slot and the old
    group is gone;
  - restart of a dead slot spawns without waiting out the grace;
  - events carrying a stale `gen` are dropped;
  - `r` during an in-flight restart is ignored;
  - global shutdown started mid-restart returns within the bound;
  - a background job left in the old group is killed by the restart.
- UI tests: `r`/`k` return `Continue` in the all-view and the right action in
  a pane; `Restarting` health renders; marker lines render without a tag, and
  the block's formatting covers every outcome (exit, signal, abandoned, never
  started, spawn failed).

## Out of scope

Overrides' trigger and the `OnExit` divergence of `k` (spec C), file watching
(spec D), any change to the buffer cap or storage.
