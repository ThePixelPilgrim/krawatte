# Krawattefile (spec B)

Spec B of the [roadmap](2026-08-20-roadmap.md). Depends on A (restart core)
only in that restarts must respawn with the slot's `cwd`/`env`.

## Problem

A cluster is a shell line of quoted commands, repeated in a Makefile or a
shell history. Slots have no names, no working directory, no environment,
and nothing to hang per-slot settings (like watch paths, spec D) on.

## Goal

A `Krawattefile` in the project describes the cluster. A bare `krawatte` in
that project launches it. Ad-hoc positional usage keeps working unchanged.

## Behavior

### Entry points

| Invocation | Meaning |
|---|---|
| `krawatte` | find a `Krawattefile` (see discovery), launch it |
| `krawatte -f PATH` / `--file PATH` | launch that file; its directory is the project dir |
| `krawatte "cmd one" "cmd two"` | ad-hoc cluster exactly as today; a `Krawattefile` in cwd is ignored |
| `krawatte -f PATH "cmd"` | error: `-f` and positional commands are mutually exclusive |

Spec C adds subcommands (`status`, `restart`, …). From then on a first
positional equal to a subcommand name is that subcommand; an ad-hoc command
literally called `status` needs `krawatte -- status`. Stated here so B's
grammar does not have to change later.

**Discovery** walks up from the current directory to `/` looking for a file
named `Krawattefile` (exact case) — like `git` finds `.git` — so the command
works from any subdirectory of the project. The directory containing the file
is the *project dir*; all relative paths in the file resolve against it. No
file found: `krawatte: no Krawattefile found in /a/b/c or any parent` on
stderr, exit 2, before touching the terminal.

### File format

TOML. Example for the erhebimus server:

```toml
[settings]
timeout = 5.0                          # grace seconds; `-t` on the CLI overrides

[[proc]]
name  = "build"
cmd   = "cargo build -p erhebimus"
watch = ["platform/server/src", "platform/server/migrations"]   # spec D

[[proc]]
name  = "server"
cmd   = "target/debug/erhebimus"
env   = { RUST_LOG = "debug,sqlx=warn" }
watch = "self"                         # spec D

[[proc]]
name  = "web"
cmd   = "npm run dev:debug"
cwd   = "frontend"
```

| Key | Required | Meaning |
|---|---|---|
| `settings.timeout` | no | grace period in seconds, default `5`; an explicit `-t` on the CLI wins |
| `proc.name` | yes | unique; `[A-Za-z0-9_-]+`; not all digits (indices address slots in spec C); not `all` |
| `proc.cmd` | yes | run via `sh -c`, as today |
| `proc.cwd` | no | working directory, relative to the project dir (absolute allowed); must exist at load. **Default: the project dir itself** — never the directory krawatte was launched from, which with walk-up discovery may be any subdirectory |
| `proc.env` | no | table of string → string, set on top of the inherited environment |
| `proc.watch` | no | the bare string `"self"`, or an array of path strings (a path literally named `self` goes in the array); any other bare string is an error; interpreted in spec D |
| `proc.ignore` | no | array of glob strings; stored here, interpreted in spec D |

Unknown keys anywhere are errors (`deny_unknown_fields`), so a typo like
`comand` cannot silently produce a slot that runs nothing. At least one
`[[proc]]` is required. Slot order is file order; slot indices (`1`–`9`,
status bar) follow it.

Every validation error is reported at once, one per line, prefixed with the
file path and the TOML line where available:

```
krawatte: Krawattefile:9: proc "server": cwd "platform/srv" does not exist
krawatte: Krawattefile:14: proc name "build" is used twice
```

Exit 2, terminal untouched.

### Runtime

- The status bar and the final status printout show `name` instead of the
  command's basename. Ad-hoc slots keep the basename.
- Every generation of a slot — initial, `r`, `k`, later watch and override —
  spawns with the slot's `cwd` and `env`. An override command (spec C) runs
  in the same directory and environment as the standard command.
- Every slot has a working directory: `cwd` if set, else the project dir.
  `sh -c` runs there, so a relative path in `cmd` is relative to it. A
  Krawattefile therefore behaves the same whether `krawatte` is started in
  the project dir or three levels below it. krawatte's own process does not
  `chdir`; only children do.
- Ad-hoc slots (positional commands) keep inheriting krawatte's cwd, as
  today.

## Design

### Module: `config.rs` (new)

Pure parsing and validation, no I/O beyond what the caller hands in:

```rust
pub struct ProcSpec {
    pub name: String,
    pub command: String,
    /// Absolute, already resolved against the project dir; the project dir
    /// itself when the file sets no `cwd`. `None` only for ad-hoc slots,
    /// which inherit krawatte's cwd.
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// As written: the `self` keyword, or path entries. Spec D resolves them.
    pub watch: Watch,            // enum Watch { None, SelfBinary, Paths(Vec<String>) }
    pub ignore: Vec<String>,
}

pub struct Krawattefile {
    pub project_dir: PathBuf,
    pub timeout: Option<Duration>,
    pub procs: Vec<ProcSpec>,
}

pub fn parse(text: &str, path: &Path, project_dir: &Path) -> Result<Krawattefile, Vec<ConfigError>>;
pub fn discover(start: &Path) -> Option<PathBuf>;          // walks up
pub fn load(path: &Path) -> Result<Krawattefile, Vec<ConfigError>>;  // read + parse
```

`ConfigError` carries `path`, `line: Option<usize>`, `message` and implements
`Display` in the format above. `parse` collects every error it can rather
than stopping at the first. Existence of `cwd` is checked in `parse` against
the resolved path (it needs the filesystem, but it is a load-time check the
user expects to see with the other errors).

Ad-hoc mode builds `ProcSpec`s too: `name = short_name_of(cmd)`, no cwd, no
env, so there is one spawn path.

### `proc.rs`

`ProcManager::spawn_all(specs: &[ProcSpec], …)`. `Proc` keeps its `ProcSpec`
(`standard` becomes `spec.command`); `spawn_one` takes the spec and applies
`Command::current_dir` / `envs`. `short_name` returns `spec.name`.

### `main.rs`

```
parse CLI
  ├─ positional → specs from strings, timeout from -t
  └─ none       → discover / -f → load → specs; timeout = -t if given, else settings, else 5
run(specs, config)
```

Config errors print and exit 2 before `TerminalGuard::enter`.

### Dependencies

`toml` and `serde` (derive). Both are small and standard; hand-parsing TOML
is not worth it.

## Testing

- `config::parse` against inline strings: the example above (a proc without
  `cwd` resolves to the project dir; `cwd = "frontend"` resolves under it);
  missing `name`;
  missing `cmd`; duplicate name; invalid name (`"my proc"`, `"12"`, `"all"`);
  unknown key at top level and inside a proc; `watch = "self"`, `watch = ["self", "src"]`,
  and bare `watch = "src"` rejected;
  `env` with a non-string value; nonexistent `cwd` (tempdir); several errors
  in one file all reported; empty file → "no [[proc]]".
- `config::discover` with a tempdir tree: found from a grandchild dir; not
  found; the nearest file wins.
- Spawn with `cwd` and `env`: a spec running `pwd; echo $KRAWATTE_TEST` in a
  tempdir produces both expected lines; after `replace`, the new generation
  prints the same (cwd/env survive restart).
- Launch from a subdirectory: `discover` from `project/a/b` finds the file
  and a proc without `cwd` prints `project` for `pwd`, not `project/a/b`.
- `main`: CLI argument combinations (`-f` + positional rejected) via clap's
  `try_parse_from`.

## Out of scope

Interpreting `watch`/`ignore` (D), any socket or subcommand (C), per-slot
`timeout`, dependency ordering, variable interpolation in the file, a
`krawatte init` generator.

## Decisions made in this spec (to confirm)

- File name is exactly `Krawattefile`, TOML inside, no extension. Editors
  will need a modeline for highlighting; the upside is that it reads as a
  project marker like `Makefile`/`justfile`.
- Discovery walks up the directory tree rather than checking only cwd.
- A slot's working directory defaults to the project dir, not to where
  krawatte was launched; relative `cwd` resolves against the project dir.
- Unknown keys are hard errors.
- `-f` with positional commands is an error rather than "positional wins".
- The ad-hoc form stays positional; an ad-hoc command named like a future
  subcommand needs `--`.
