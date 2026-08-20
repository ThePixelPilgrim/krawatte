# Krawattefile Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Red–green TDD is mandatory** (see Global Constraints): test first, watch it fail, then implement. Use superpowers:test-driven-development for every task.

**Goal:** A `Krawattefile` (TOML) describes the cluster — named slots with `cmd`, `cwd`, `env`, `watch`, `ignore` — and a bare `krawatte` anywhere in the project launches it; ad-hoc positional usage is unchanged.

**Architecture:** A new pure `config.rs` module parses and validates the file into `ProcSpec`s (collecting every error, with line numbers from `toml::Spanned`). `ProcManager` gains `spawn_specs(&[ProcSpec])` and applies each spec's `cwd`/`env` on every spawn, including restarts; the old `spawn_all(&[String])` becomes a thin ad-hoc adapter so existing tests and the positional mode keep working. `main.rs` chooses the source (positional → ad-hoc, else `-f` or discovery walking up from cwd) before touching the terminal.

**Tech Stack:** Rust 2024, `toml` 1.x + `serde` derive (new), `tempfile` (new, dev-only), existing `clap` derive.

**Spec:** `docs/superpowers/specs/2026-08-20-krawattefile-design.md`. Roadmap: `docs/superpowers/specs/2026-08-20-roadmap.md`.

## Global Constraints

- **Red–green TDD is mandatory.** For every behavior change: write the test
  first, run it and *observe it fail for the expected reason* (red), write
  the minimal code that makes it pass, run it and observe it pass (green),
  then refactor with the suite green. Never write implementation code before
  the red run has been seen; a test that passes on first run proves nothing —
  fix it until it fails without the implementation. Do not reorder steps or
  batch several tasks' implementation ahead of their tests.
- `gen` is a reserved keyword in edition 2024: the codebase spells the field
  and every binding `r#gen`. Nothing in this plan touches it, but do not
  "fix" it if you see it.
- Baseline at the start: `cargo test -q` → 87 passed, `cargo clippy --all-targets -q` silent, `cargo fmt --check` clean. All three must stay clean after every task; run `cargo fmt` before committing.
- Linux/Unix only. No change to process-group or shutdown behavior.
- Config errors are reported all at once, before the terminal is touched, exit code 2. Every name error says *why* and what to do instead (spec: "a bare 'invalid name' is not acceptable").
- Unknown TOML keys are errors. Bare-string `watch` other than `"self"` is an error. `all`, all-digit names, and names outside `[A-Za-z0-9_-]+` are errors. Duplicate names are errors naming the first occurrence's line.
- A proc without `cwd` runs in the project dir (the Krawattefile's directory), never in krawatte's launch directory. Ad-hoc slots inherit krawatte's cwd.
- Commit after every task; messages in the imperative, as in `git log`. Do not push.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `src/config.rs` (new) | `ProcSpec`, `Watch`, `Krawattefile`, `ConfigError`; `parse`, `discover`, `load`; `short_name_of` (moved here from `proc.rs`). Pure apart from `is_dir`/`read_to_string`. |
| `src/proc.rs` | `Proc.spec: ProcSpec` replaces `standard`/`short`; `spawn_specs`; `spawn_one` applies `cwd`/`env`; `spawn_all(&[String])` delegates via `ProcSpec::adhoc`. |
| `src/main.rs` | `Cli` gains `-f/--file`, optional `-t`, optional positionals; `resolve_specs` picks the mode; `run` takes `&[ProcSpec]`. |
| `Cargo.toml` | `toml`, `serde` deps; `tempfile` dev-dep. |
| `README.md` | Krawattefile section, updated usage table. |

---

### Task 1: `config::parse` — types, parsing, validation

**Files:**
- Create: `src/config.rs`
- Modify: `Cargo.toml`, `src/main.rs` (add `mod config;`), `src/proc.rs` (move `short_name_of` + its test out)

**Interfaces:**
- Produces:
  ```rust
  pub const FILE_NAME: &str = "Krawattefile";
  pub enum Watch { None, SelfBinary, Paths(Vec<String>) }
  pub struct ProcSpec { pub name: String, pub command: String, pub cwd: Option<PathBuf>, pub env: Vec<(String, String)>, pub watch: Watch, pub ignore: Vec<String> }
  impl ProcSpec { pub fn adhoc(command: &str) -> ProcSpec }
  pub struct Krawattefile { pub path: PathBuf, pub project_dir: PathBuf, pub timeout: Option<Duration>, pub procs: Vec<ProcSpec> }
  pub struct ConfigError { pub path: PathBuf, pub line: Option<usize>, pub message: String }   // Display: "path:line: message" / "path: message"
  pub fn parse(text: &str, path: &Path, project_dir: &Path) -> Result<Krawattefile, Vec<ConfigError>>;
  pub fn short_name_of(command: &str) -> String;
  ```

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]` add (keep alphabetical order):

```toml
serde = { version = "1", features = ["derive"] }
toml = "1"
```

and a new section:

```toml
[dev-dependencies]
tempfile = "3"
```

Run `cargo build -q` once so the lock file updates (commit it with this task).

- [ ] **Step 2: Write the failing tests**

Create `src/config.rs` with only the module doc, imports and the test module for now:

```rust
//! Krawattefile: the TOML description of a cluster, its discovery, parsing
//! and validation.
//!
//! Parsing collects every error it can find and reports them together, with
//! line numbers taken from `toml`'s spans, so one round trip fixes the file.
//! Apart from checking that a `cwd` exists this module is pure.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in(dir: &Path, text: &str) -> Result<Krawattefile, Vec<ConfigError>> {
        parse(text, Path::new("Krawattefile"), dir)
    }

    fn messages(errs: Vec<ConfigError>) -> Vec<String> {
        errs.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn parses_the_spec_example() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("frontend")).unwrap();
        let text = r#"
[settings]
timeout = 2.5

[[proc]]
name  = "build"
cmd   = "cargo build -p erhebimus"
watch = ["platform/server/src", "platform/server/migrations"]

[[proc]]
name  = "server"
cmd   = "target/debug/erhebimus"
env   = { RUST_LOG = "debug,sqlx=warn" }
watch = "self"

[[proc]]
name   = "web"
cmd    = "npm run dev:debug"
cwd    = "frontend"
ignore = ["*.log"]
"#;
        let kf = parse_in(dir.path(), text).unwrap();
        assert_eq!(kf.project_dir, dir.path());
        assert_eq!(kf.timeout, Some(Duration::from_secs_f64(2.5)));
        assert_eq!(kf.procs.len(), 3);

        let build = &kf.procs[0];
        assert_eq!(build.name, "build");
        assert_eq!(build.command, "cargo build -p erhebimus");
        // No cwd: the project dir, never "inherit".
        assert_eq!(build.cwd.as_deref(), Some(dir.path()));
        assert!(build.env.is_empty());
        assert_eq!(
            build.watch,
            Watch::Paths(vec![
                "platform/server/src".to_string(),
                "platform/server/migrations".to_string()
            ])
        );

        let server = &kf.procs[1];
        assert_eq!(server.env, vec![("RUST_LOG".to_string(), "debug,sqlx=warn".to_string())]);
        assert_eq!(server.watch, Watch::SelfBinary);

        let web = &kf.procs[2];
        assert_eq!(web.cwd.as_deref(), Some(dir.path().join("frontend").as_path()));
        assert_eq!(web.watch, Watch::None);
        assert_eq!(web.ignore, vec!["*.log".to_string()]);
    }

    #[test]
    fn a_path_literally_named_self_goes_in_the_array() {
        let dir = tempfile::tempdir().unwrap();
        let kf = parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncmd = \"x\"\nwatch = [\"self\", \"src\"]\n").unwrap();
        assert_eq!(kf.procs[0].watch, Watch::Paths(vec!["self".to_string(), "src".to_string()]));
    }

    #[test]
    fn bare_string_watch_other_than_self_is_rejected_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncmd = \"x\"\nwatch = \"src\"\n").unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].starts_with("Krawattefile:4: "), "{}", errs[0]);
        assert!(errs[0].contains("watch = [\"src\"]"), "{}", errs[0]);
        assert!(errs[0].contains("reserved for the keyword"), "{}", errs[0]);
    }

    #[test]
    fn name_errors_say_why() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
[[proc]]
name = "build"
cmd = "x"

[[proc]]
name = "build"
cmd = "x"

[[proc]]
name = "all"
cmd = "x"

[[proc]]
name = "12"
cmd = "x"

[[proc]]
name = "my proc"
cmd = "x"
"#;
        let errs = messages(parse_in(dir.path(), text).unwrap_err());
        assert_eq!(errs.len(), 4, "{errs:#?}");
        assert_eq!(
            errs[0],
            "Krawattefile:7: proc name \"build\" is already used by the proc on line 3"
        );
        assert_eq!(
            errs[1],
            "Krawattefile:11: proc name \"all\" is reserved (it addresses every slot in `krawatte restart all` and friends); pick another name"
        );
        assert_eq!(
            errs[2],
            "Krawattefile:15: proc name \"12\" must contain a letter, `_` or `-` (all-digit names would be mistaken for slot indices)"
        );
        assert_eq!(
            errs[3],
            "Krawattefile:19: proc name \"my proc\" may only contain letters, digits, `_` and `-`"
        );
    }

    #[test]
    fn missing_cwd_is_reported_with_the_name_and_the_path_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "[[proc]]\nname = \"server\"\ncmd = \"x\"\ncwd = \"platform/srv\"\n").unwrap_err());
        assert_eq!(
            errs,
            vec!["Krawattefile:4: proc \"server\": cwd \"platform/srv\" does not exist".to_string()]
        );
    }

    #[test]
    fn unknown_keys_and_missing_required_keys_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncomand = \"x\"\n").unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("comand"), "{}", errs[0]);

        let errs = messages(parse_in(dir.path(), "[[proc]]\ncmd = \"x\"\n").unwrap_err());
        assert!(errs[0].contains("name"), "{}", errs[0]);

        let errs = messages(parse_in(dir.path(), "[settings]\ntimeou = 1\n[[proc]]\nname = \"a\"\ncmd = \"x\"\n").unwrap_err());
        assert!(errs[0].contains("timeou"), "{}", errs[0]);
    }

    #[test]
    fn env_values_must_be_strings() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncmd = \"x\"\nenv = { PORT = 8080 }\n").unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].starts_with("Krawattefile:4:"), "{}", errs[0]);
    }

    #[test]
    fn empty_file_and_negative_timeout_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "").unwrap_err());
        assert_eq!(errs, vec!["Krawattefile: no [[proc]] entries: a Krawattefile needs at least one process".to_string()]);

        let errs = messages(parse_in(dir.path(), "[settings]\ntimeout = -1\n[[proc]]\nname = \"a\"\ncmd = \"x\"\n").unwrap_err());
        assert_eq!(errs, vec!["Krawattefile: settings.timeout must not be negative (got -1)".to_string()]);
    }

    #[test]
    fn several_errors_are_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[[proc]]\nname = \"all\"\ncmd = \"x\"\ncwd = \"nope\"\n\n[[proc]]\nname = \"b\"\ncmd = \"y\"\nwatch = \"src\"\n";
        let errs = parse_in(dir.path(), text).unwrap_err();
        assert_eq!(errs.len(), 3, "{:#?}", messages(errs));
    }

    #[test]
    fn adhoc_spec_uses_the_basename_and_inherits_everything() {
        let s = ProcSpec::adhoc("/usr/bin/python worker.py");
        assert_eq!(s.name, "python");
        assert_eq!(s.command, "/usr/bin/python worker.py");
        assert_eq!(s.cwd, None);
        assert!(s.env.is_empty());
        assert_eq!(s.watch, Watch::None);
    }

    #[test]
    fn short_name_takes_basename_of_first_token() {
        assert_eq!(short_name_of("cargo watch -x check"), "cargo");
        assert_eq!(short_name_of("/usr/bin/python worker.py"), "python");
        assert_eq!(short_name_of("npm run dev"), "npm");
        assert_eq!(short_name_of(""), "");
    }
}
```

Add `mod config;` to `src/main.rs` after `mod buffer;`.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -q config::`
Expected: compile errors — `parse`, `Krawattefile`, `ConfigError`, `Watch`, `ProcSpec`, `short_name_of` not found.

- [ ] **Step 4: Implement**

Insert between the imports and `#[cfg(test)]` in `src/config.rs`:

```rust
/// The file name looked for in the current directory and its parents.
pub const FILE_NAME: &str = "Krawattefile";

/// What a slot watches for restarts. Interpreted by the file-watching spec;
/// here it is only parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    /// No `watch` key.
    None,
    /// `watch = "self"`: the file named by the command's first token.
    SelfBinary,
    /// `watch = [...]`: paths, as written. A path literally called `self`
    /// goes here too — the bare-string form is the only keyword form.
    Paths(Vec<String>),
}

/// One slot as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSpec {
    /// Status-bar name; unique within a file.
    pub name: String,
    /// Run via `sh -c`.
    pub command: String,
    /// Absolute working directory. For a Krawattefile slot this is always
    /// set (the project dir when the file gives no `cwd`); `None` only for an
    /// ad-hoc slot, which inherits krawatte's own cwd.
    pub cwd: Option<PathBuf>,
    /// Set on top of the inherited environment.
    pub env: Vec<(String, String)>,
    pub watch: Watch,
    /// Glob patterns, as written; interpreted by the file-watching spec.
    pub ignore: Vec<String>,
}

impl ProcSpec {
    /// A slot for a positional CLI command: named by its basename, inheriting
    /// cwd and environment, watching nothing.
    pub fn adhoc(command: &str) -> ProcSpec {
        ProcSpec {
            name: short_name_of(command),
            command: command.to_string(),
            cwd: None,
            env: Vec::new(),
            watch: Watch::None,
            ignore: Vec::new(),
        }
    }
}

/// A parsed, validated Krawattefile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Krawattefile {
    /// The file, as the user referred to it (used in error messages).
    pub path: PathBuf,
    /// Directory containing the file; base for every relative path in it.
    pub project_dir: PathBuf,
    /// `settings.timeout`, if given.
    pub timeout: Option<Duration>,
    pub procs: Vec<ProcSpec>,
}

/// One problem with a Krawattefile. Displays as `path:line: message`, or
/// `path: message` when no line applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.path.display(), line, self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

// --- raw TOML shape --------------------------------------------------------
//
// Deserialised with `deny_unknown_fields` so a typo is an error, not a slot
// that silently runs nothing. `Spanned` keeps byte offsets for the values the
// validation pass reports on, so those errors carry a line number.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    settings: Option<RawSettings>,
    #[serde(default)]
    proc: Vec<RawProc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    timeout: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProc {
    name: Spanned<String>,
    cmd: String,
    cwd: Option<Spanned<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    watch: Option<Spanned<RawWatch>>,
    #[serde(default)]
    ignore: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawWatch {
    One(String),
    Many(Vec<String>),
}

/// 1-based line of a byte offset into `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Parse and validate `text` (the contents of `path`, whose directory is
/// `project_dir`). Returns every error found, not just the first.
pub fn parse(text: &str, path: &Path, project_dir: &Path) -> Result<Krawattefile, Vec<ConfigError>> {
    let err = |line: Option<usize>, message: String| ConfigError {
        path: path.to_path_buf(),
        line,
        message,
    };

    // Shape errors (syntax, unknown keys, wrong types, missing required keys)
    // come from toml one at a time; they stop parsing, so report and return.
    let raw: RawFile = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => {
            let line = e.span().map(|s| line_of(text, s.start));
            return Err(vec![err(line, e.message().to_string())]);
        }
    };

    let mut errors = Vec::new();

    let timeout = match raw.settings.and_then(|s| s.timeout) {
        Some(t) if t < 0.0 => {
            errors.push(err(None, format!("settings.timeout must not be negative (got {t})")));
            None
        }
        Some(t) => Some(Duration::from_secs_f64(t)),
        None => None,
    };

    if raw.proc.is_empty() {
        errors.push(err(
            None,
            "no [[proc]] entries: a Krawattefile needs at least one process".to_string(),
        ));
    }

    // (name, line) of every proc seen so far, for duplicate reporting.
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut procs = Vec::with_capacity(raw.proc.len());

    for p in raw.proc {
        let name_line = line_of(text, p.name.span().start);
        let name = p.name.into_inner();

        if let Some(problem) = name_problem(&name) {
            errors.push(err(Some(name_line), format!("proc name {name:?} {problem}")));
        }
        if let Some((_, first)) = seen.iter().find(|(n, _)| *n == name) {
            errors.push(err(
                Some(name_line),
                format!("proc name {name:?} is already used by the proc on line {first}"),
            ));
        }
        seen.push((name.clone(), name_line));

        let cwd = match p.cwd {
            None => project_dir.to_path_buf(),
            Some(spanned) => {
                let line = line_of(text, spanned.span().start);
                let as_written = spanned.into_inner();
                let dir = project_dir.join(&as_written);
                if !dir.is_dir() {
                    errors.push(err(
                        Some(line),
                        format!("proc {name:?}: cwd {as_written:?} does not exist"),
                    ));
                }
                dir
            }
        };

        let watch = match p.watch {
            None => Watch::None,
            Some(spanned) => {
                let line = line_of(text, spanned.span().start);
                match spanned.into_inner() {
                    RawWatch::One(s) if s == "self" => Watch::SelfBinary,
                    RawWatch::One(s) => {
                        errors.push(err(
                            Some(line),
                            format!(
                                "proc {name:?}: watch = {s:?} — the bare-string form is reserved for the keyword \"self\"; write watch = [{s:?}] to watch a path"
                            ),
                        ));
                        Watch::None
                    }
                    RawWatch::Many(paths) => Watch::Paths(paths),
                }
            }
        };

        procs.push(ProcSpec {
            name,
            command: p.cmd,
            cwd: Some(cwd),
            env: p.env.into_iter().collect(),
            watch,
            ignore: p.ignore,
        });
    }

    if errors.is_empty() {
        Ok(Krawattefile {
            path: path.to_path_buf(),
            project_dir: project_dir.to_path_buf(),
            timeout,
            procs,
        })
    } else {
        Err(errors)
    }
}

/// Why a proc name is unacceptable, phrased to follow `proc name "x" …`.
fn name_problem(name: &str) -> Option<&'static str> {
    if name == "all" {
        return Some(
            "is reserved (it addresses every slot in `krawatte restart all` and friends); pick another name",
        );
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.is_empty() || !name.chars().all(allowed) {
        return Some("may only contain letters, digits, `_` and `-`");
    }
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Some("must contain a letter, `_` or `-` (all-digit names would be mistaken for slot indices)");
    }
    None
}

/// Derive a short status-bar name from a command line: the basename of the
/// first whitespace-separated token.
pub fn short_name_of(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    if base.is_empty() {
        command.to_string()
    } else {
        base.to_string()
    }
}
```

In `src/proc.rs`: delete the private `short_name_of` function and its test `short_name_takes_basename_of_first_token` (both now live in `config.rs`), and add `use crate::config::short_name_of;` next to the other `crate::` import. (`fs` is imported in `config.rs` for Task 2; if clippy complains it is unused after this task, leave the import out until Task 2.)

**If `Spanned<RawWatch>` refuses to deserialise** (toml's `Spanned` wraps through a special struct name and some versions reject an untagged enum inside it), fall back to `watch: Option<RawWatch>` and report the watch error on `name_line` instead; adjust `bare_string_watch_other_than_self_is_rejected_with_guidance` to expect `Krawattefile:2:`. Note the fallback in your report.

- [ ] **Step 5: Run the tests**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 98 passed (87 − 1 moved + 12 new), no warnings. `Krawattefile`, `parse`, `FILE_NAME`, `ConfigError` have no non-test caller until Tasks 2–4; if clippy flags them, add `#[allow(dead_code)]` with a comment `// wired in by a later task` and remove it in the task that uses them.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/config.rs src/main.rs src/proc.rs
git commit -m "Add Krawattefile parsing and validation"
```

---

### Task 2: `config::discover` and `config::load`

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `pub fn discover(start: &Path) -> Option<PathBuf>` (walks up, returns the file path); `pub fn load(path: &Path) -> Result<Krawattefile, Vec<ConfigError>>`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `src/config.rs`:

```rust
    #[test]
    fn discover_walks_up_and_prefers_the_nearest_file() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let deep = project.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(discover(&deep), None);

        fs::write(project.join(FILE_NAME), "").unwrap();
        assert_eq!(discover(&deep), Some(project.join(FILE_NAME)));
        assert_eq!(discover(&project), Some(project.join(FILE_NAME)));

        // A nearer file shadows the outer one.
        fs::write(project.join("a").join(FILE_NAME), "").unwrap();
        assert_eq!(discover(&deep), Some(project.join("a").join(FILE_NAME)));
    }

    #[test]
    fn discover_ignores_a_directory_named_like_the_file() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(FILE_NAME)).unwrap();
        assert_eq!(discover(root.path()), None);
    }

    #[test]
    fn load_reads_the_file_and_resolves_the_project_dir() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("frontend")).unwrap();
        let file = project.join(FILE_NAME);
        fs::write(&file, "[[proc]]\nname = \"web\"\ncmd = \"npm run dev\"\ncwd = \"frontend\"\n").unwrap();

        let kf = load(&file).unwrap();
        let canon = project.canonicalize().unwrap();
        assert_eq!(kf.project_dir, canon);
        assert_eq!(kf.procs[0].cwd.as_deref(), Some(canon.join("frontend").as_path()));
        // Errors keep the path as the user gave it.
        assert_eq!(kf.path, file);
    }

    #[test]
    fn load_reports_an_unreadable_file_as_a_config_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("nope").join(FILE_NAME);
        let errs = messages(load(&missing).unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].starts_with(&format!("{}: cannot read", missing.display())), "{}", errs[0]);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q config::discover config::load`
Expected: compile errors — `discover`, `load` not found.

- [ ] **Step 3: Implement**

After `parse` in `src/config.rs`:

```rust
/// The nearest `Krawattefile` in `start` or any of its parents, like `git`
/// finds `.git`. A directory by that name does not count.
pub fn discover(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(FILE_NAME))
        .find(|candidate| candidate.is_file())
}

/// Read and parse the file at `path`. The project dir is the file's
/// (canonical) parent; `path` itself is kept as given for messages.
pub fn load(path: &Path) -> Result<Krawattefile, Vec<ConfigError>> {
    let unreadable = |e: std::io::Error| {
        vec![ConfigError {
            path: path.to_path_buf(),
            line: None,
            message: format!("cannot read: {e}"),
        }]
    };
    let text = fs::read_to_string(path).map_err(unreadable)?;
    let canonical = path.canonicalize().map_err(unreadable)?;
    let project_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    parse(&text, path, &project_dir)
}
```

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 102 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "Add Krawattefile discovery and loading"
```

---

### Task 3: Spawn from `ProcSpec` with `cwd` and `env`

**Files:**
- Modify: `src/proc.rs` (`Proc`, `spawn_all`, new `spawn_specs`, `spawn_all_with_shell`, `spawn_one`, `kill`, `current_command`, `short_name`, `complete`, tests)

**Interfaces:**
- Produces: `pub fn spawn_specs(specs: &[ProcSpec], config: &Config, tx: Sender<Event>) -> ProcManager`. `spawn_all(&[String], …)` keeps its signature and delegates.
- Consumes: `config::ProcSpec`, `ProcSpec::adhoc`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `src/proc.rs`, after `read_pid_line`:

```rust
    /// The text of the next `n` `Event::Line`s (skipping other events), bounded.
    fn read_lines(rx: &mpsc::Receiver<Event>, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < deadline {
            if let Ok(Event::Line { bytes, .. }) = rx.recv_timeout(Duration::from_millis(100)) {
                out.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        out
    }

    #[test]
    fn spawn_specs_applies_cwd_and_env_and_restart_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let spec = ProcSpec {
            name: "probe".to_string(),
            command: "pwd; echo $KRAWATTE_TEST".to_string(),
            cwd: Some(cwd.clone()),
            env: vec![("KRAWATTE_TEST".to_string(), "42".to_string())],
            watch: Watch::None,
            ignore: Vec::new(),
        };
        let (tx, rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_specs(std::slice::from_ref(&spec), &short_grace(), tx);
        assert_eq!(mgr.short_name(0), "probe");
        assert_eq!(mgr.current_command(0), "pwd; echo $KRAWATTE_TEST");
        wait_until_dead(&mgr);
        assert_eq!(read_lines(&rx, 2), vec![cwd.display().to_string(), "42".to_string()]);

        // A restarted generation runs in the same directory and environment.
        assert!(mgr.replace(0, spec.command.clone()));
        tick_until_transition(&mut mgr, Duration::from_secs(5));
        wait_until_dead(&mgr);
        assert_eq!(read_lines(&rx, 2), vec![cwd.display().to_string(), "42".to_string()]);
        mgr.shutdown();
    }

    #[test]
    fn adhoc_slots_inherit_krawattes_cwd() {
        let here = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut mgr = ProcManager::spawn_all(&["pwd".to_string()], &short_grace(), tx);
        wait_until_dead(&mgr);
        assert_eq!(read_lines(&rx, 1), vec![here.display().to_string()]);
        mgr.shutdown();
    }
```

Add `use crate::config::{ProcSpec, Watch};` to the test module's imports if `use super::*` does not already bring them in (it will once the implementation imports them at module level).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q spawn_specs_applies`
Expected: compile error — no function `spawn_specs`.

- [ ] **Step 3: Implement**

Module-level import in `src/proc.rs`:

```rust
use crate::config::{ProcSpec, short_name_of};
```

(`short_name_of` is no longer needed in `proc.rs` once `ProcSpec::adhoc` exists — drop it from the import if the compiler says it is unused.)

Replace the `standard` and `short` fields of `Proc`:

```rust
/// Per-slot state: the configured slot and its current generation.
struct Proc {
    /// The slot as configured: name, standard command, cwd, env.
    spec: ProcSpec,
    /// Number of the most recent generation; `0` for the initial spawn.
    r#gen: Gen,
    /// The most recent generation, or `None` if the slot has never spawned
    /// successfully.
    live: Option<Generation>,
    /// Teardown in progress, if any. A slot has at most one.
    restart: Option<Restart>,
}
```

Spawning:

```rust
    /// Spawn every positional command (each a string run via `sh -c`) as an
    /// ad-hoc slot: named by its basename, inheriting krawatte's cwd and
    /// environment. See [`spawn_specs`](Self::spawn_specs).
    pub fn spawn_all(commands: &[String], config: &Config, tx: Sender<Event>) -> ProcManager {
        let specs: Vec<ProcSpec> = commands.iter().map(|c| ProcSpec::adhoc(c)).collect();
        Self::spawn_specs(&specs, config, tx)
    }

    /// Spawn every slot, wiring reader and waiter threads that emit [`Event`]s
    /// on `tx`. Spawn failures are reported as [`Event::SpawnFailed`] rather
    /// than aborting the whole set.
    pub fn spawn_specs(specs: &[ProcSpec], config: &Config, tx: Sender<Event>) -> ProcManager {
        Self::spawn_all_with_shell(specs, config, tx, "sh")
    }

    /// Like [`spawn_specs`](Self::spawn_specs) but with an explicit shell
    /// program. Exists so tests can point at a non-existent program and
    /// exercise the genuine spawn-failure (`Event::SpawnFailed` / dead slot)
    /// code path.
    fn spawn_all_with_shell(
        specs: &[ProcSpec],
        config: &Config,
        tx: Sender<Event>,
        shell: &str,
    ) -> ProcManager {
        let mut mgr = ProcManager {
            procs: Vec::with_capacity(specs.len()),
            grace_period: config.grace_period,
            shell: shell.to_string(),
            seq: Arc::new(AtomicU64::new(0)),
            tx,
        };
        for (proc, spec) in specs.iter().enumerate() {
            let live = match spawn_one(proc, 0, &mgr.shell, &spec.command, spec, &mgr.seq, &mgr.tx) {
                Ok(generation) => Some(generation),
                Err(err) => {
                    // Spawn failure: report it and record a slot with no generation.
                    let _ = mgr.tx.send(Event::SpawnFailed {
                        proc,
                        r#gen: 0,
                        error: err.to_string(),
                    });
                    None
                }
            };
            mgr.procs.push(Proc {
                spec: spec.clone(),
                r#gen: 0,
                live,
                restart: None,
            });
        }
        mgr
    }
```

The test `genuine_spawn_failure_reports_dead_slot` calls `spawn_all_with_shell(&["whatever".to_string()], …)`; change it to `spawn_all_with_shell(&[ProcSpec::adhoc("whatever")], …)`. Same for `restart_of_never_started_slot_reports_no_old_generation`.

`kill`, `short_name`, `current_command`:

```rust
    pub fn kill(&mut self, proc: ProcId) -> bool {
        let standard = self.procs[proc].spec.command.clone();
        self.replace(proc, standard)
    }
```

```rust
    /// Display name for the status bar: the configured name, or the command's
    /// basename for an ad-hoc slot.
    pub fn short_name(&self, proc: ProcId) -> &str {
        &self.procs[proc].spec.name
    }
```

```rust
    pub fn current_command(&self, proc: ProcId) -> &str {
        let p = &self.procs[proc];
        p.live.as_ref().map_or(&p.spec.command, |g| &g.command)
    }
```

In `complete`, the respawn:

```rust
        let spec = &self.procs[proc].spec;
        let spawn = match spawn_one(proc, r#gen, &self.shell, &command, spec, &self.seq, &self.tx) {
```

(the immutable borrow of `spec` ends at the call; the `Ok` arm's `self.procs[proc].live = Some(g)` compiles as before).

`spawn_one` gains `spec: &ProcSpec` after `command` and applies it right after building the command:

```rust
fn spawn_one(
    proc: ProcId,
    r#gen: Gen,
    shell: &str,
    command: &str,
    spec: &ProcSpec,
    seq: &Arc<AtomicU64>,
    tx: &Sender<Event>,
) -> std::io::Result<Generation> {
    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A Krawattefile slot always has a cwd (the project dir by default); an
    // ad-hoc slot inherits krawatte's. Env entries layer over the inherited
    // environment rather than replacing it.
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
```

Remove any `#[allow(dead_code)]` Task 1 put on `ProcSpec`-related items now that they are used.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 104 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/proc.rs
git commit -m "Spawn slots from ProcSpec with cwd and env"
```

---

### Task 4: CLI modes, `run` on specs, README, smoke test

**Files:**
- Modify: `src/main.rs` (`Cli`, `main`, new `resolve_specs`, `run`, tests)
- Modify: `README.md`

**Interfaces:**
- Consumes: `config::{discover, load, ProcSpec, FILE_NAME}`, `ProcManager::spawn_specs`.
- Produces: `fn resolve_specs(cli: &Cli) -> Result<(Vec<ProcSpec>, Option<Duration>), Vec<String>>`.

- [ ] **Step 1: Write the failing tests**

In `src/main.rs` `mod tests`:

```rust
    use crate::config::{FILE_NAME, Watch};
    use std::fs;
    use std::path::PathBuf;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("krawatte").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_accepts_no_arguments_and_rejects_file_with_commands() {
        assert!(cli(&[]).commands.is_empty());
        assert_eq!(cli(&["-t", "2", "a", "b"]).commands, vec!["a", "b"]);
        assert_eq!(cli(&["-f", "x/Krawattefile"]).file, Some(PathBuf::from("x/Krawattefile")));
        assert!(Cli::try_parse_from(["krawatte", "-f", "x", "cmd"]).is_err());
    }

    #[test]
    fn positional_commands_become_adhoc_specs() {
        let (specs, timeout) = resolve_specs(&cli(&["npm run dev", "cargo check"])).unwrap();
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
        fs::write(&file, "[settings]\ntimeout = 1.5\n[[proc]]\nname = \"a\"\ncmd = \"true\"\nwatch = \"self\"\n").unwrap();
        let (specs, timeout) = resolve_specs(&cli(&["-f", file.to_str().unwrap()])).unwrap();
        assert_eq!(timeout, Some(Duration::from_secs_f64(1.5)));
        assert_eq!(specs[0].name, "a");
        assert_eq!(specs[0].cwd.as_deref(), Some(dir.path().canonicalize().unwrap().as_path()));
        assert_eq!(specs[0].watch, Watch::SelfBinary);
    }

    #[test]
    fn config_errors_are_returned_as_messages() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        fs::write(&file, "[[proc]]\nname = \"all\"\ncmd = \"true\"\n").unwrap();
        let errs = resolve_specs(&cli(&["-f", file.to_str().unwrap()])).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("is reserved"), "{}", errs[0]);

        let errs = resolve_specs(&cli(&["-f", "/nonexistent/Krawattefile"])).unwrap_err();
        assert!(errs[0].contains("cannot read"), "{}", errs[0]);
    }

    #[test]
    fn grace_period_prefers_cli_then_file_then_default() {
        assert_eq!(grace_period(Some(2.0), Some(Duration::from_secs(7))), Duration::from_secs(2));
        assert_eq!(grace_period(None, Some(Duration::from_secs(7))), Duration::from_secs(7));
        assert_eq!(grace_period(None, None), Duration::from_secs(5));
        assert_eq!(grace_period(Some(-1.0), None), Duration::ZERO);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -q cli_accepts`
Expected: compile errors — `Cli` has no field `file`, `resolve_specs`/`grace_period` not found.

- [ ] **Step 3: Implement**

Imports in `src/main.rs`:

```rust
use std::path::PathBuf;

use crate::config::ProcSpec;
```

`Cli`:

```rust
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
    #[arg(value_name = "COMMAND", trailing_var_arg = true, allow_hyphen_values = true)]
    commands: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let (specs, file_timeout) = match resolve_specs(&cli) {
        Ok(resolved) => resolved,
        Err(messages) => {
            for m in messages {
                eprintln!("krawatte: {m}");
            }
            std::process::exit(2);
        }
    };
    let config = Config {
        grace_period: grace_period(cli.timeout, file_timeout),
        ..Config::default()
    };
    match run(&specs, &config) {
        Ok((names, started, statuses)) => {
            print_final_statuses(&names, &started, &statuses);
        }
        Err(e) => {
            eprintln!("krawatte: fatal error: {e}");
            std::process::exit(1);
        }
    }
}

/// Where the cluster comes from: positional commands run ad hoc; otherwise
/// the Krawattefile given with `-f`, or the nearest one above the current
/// directory. Errors are complete, user-facing messages (without the
/// `krawatte:` prefix), all of them at once.
fn resolve_specs(cli: &Cli) -> Result<(Vec<ProcSpec>, Option<Duration>), Vec<String>> {
    if !cli.commands.is_empty() {
        let specs = cli.commands.iter().map(|c| ProcSpec::adhoc(c)).collect();
        return Ok((specs, None));
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
    let file = config::load(&path).map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    Ok((file.procs, file.timeout))
}

/// An explicit `-t` wins over the file's `settings.timeout`, which wins over
/// the default of five seconds. Negative values clamp to zero.
fn grace_period(cli_secs: Option<f64>, file_timeout: Option<Duration>) -> Duration {
    cli_secs
        .map(|t| Duration::from_secs_f64(t.max(0.0)))
        .or(file_timeout)
        .unwrap_or(Duration::from_secs(5))
}
```

`run`:

```rust
fn run(specs: &[ProcSpec], config: &Config) -> io::Result<RunResult> {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut manager = ProcManager::spawn_specs(specs, config, tx);
    let mut buffers = BufferSet::new(specs.len(), config);
```

(the rest of `run` is unchanged). Remove any remaining `#[allow(dead_code)] // wired in by a later task` left in `config.rs`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -q && cargo clippy --all-targets -q && cargo fmt --check`
Expected: 109 passed, no warnings.

- [ ] **Step 5: Manual smoke test**

```bash
cargo build --release -q
d=$(mktemp -d) && mkdir -p "$d/sub/dir" "$d/web" && cat > "$d/Krawattefile" <<'EOF'
[settings]
timeout = 1

[[proc]]
name = "ticker"
cmd  = "while true; do echo tick $(pwd); sleep 1; done"

[[proc]]
name = "web"
cmd  = "pwd; echo PORT=$PORT; sleep 100"
cwd  = "web"
env  = { PORT = "5174" }
EOF
```

1. `cd "$d/sub/dir" && /path/to/target/release/krawatte` — status bar shows `[1] ticker ● [2] web ●`; pane 1 prints `tick <d>` (the project dir, not `sub/dir`); pane 2 printed `<d>/web` and `PORT=5174`. Press `2`, `r`: the marker block appears and the new generation prints the same two lines. `q` exits; the final printout uses `ticker`/`web`.
2. `krawatte -f "$d/Krawattefile"` from anywhere behaves the same.
3. `cd /tmp && krawatte` (no file above `/tmp`) prints the "no Krawattefile found … or any parent directory" message, exit 2, terminal untouched.
4. Put `name = "all"` into the file and run: the reserved-name message with a line number, exit 2.
5. `krawatte "echo adhoc"` still works with a Krawattefile present.

- [ ] **Step 6: Document**

In `README.md`:

Replace the Usage block and option table with:

```markdown
## Usage

Ad hoc — each argument is one command, run via `sh -c` in its own process
group:

```
krawatte "cargo watch -x check" "npm run dev" "python worker.py"
```

From a project — with no commands, krawatte looks for a `Krawattefile` in the
current directory or any parent and launches it:

```
krawatte
```

| Option | Meaning |
|---|---|
| `-t`, `--timeout <SECS>` | grace period between SIGTERM and SIGKILL on shutdown (overrides the file's `settings.timeout`; default `5`) |
| `-f`, `--file <PATH>` | launch this Krawattefile instead of searching for one; cannot be combined with commands |
```

Add a section after Keys:

```markdown
## Krawattefile

TOML. Every relative path is resolved against the directory containing the
file (the *project dir*), and a process without `cwd` runs there — never in
the directory krawatte happened to be started from.

```toml
[settings]
timeout = 5.0                 # optional; seconds of grace on shutdown

[[proc]]
name = "build"                # required; unique; letters, digits, `_`, `-`; not all digits; not `all`
cmd  = "cargo build -p app"   # required; run via `sh -c`

[[proc]]
name = "web"
cmd  = "npm run dev"
cwd  = "frontend"             # optional; relative to the project dir; must exist
env  = { PORT = "5174" }      # optional; added to the inherited environment
```

Unknown keys are errors. All problems in the file are reported together,
with line numbers, before the terminal is touched (exit code 2). Slots
appear in file order and the status bar shows their names.

`watch` and `ignore` keys are accepted and reserved for restart-on-change;
see `docs/superpowers/specs/2026-08-20-file-watching-design.md`.
```

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/main.rs README.md
git commit -m "Launch a Krawattefile when no commands are given"
```

---

## Self-review

**Spec coverage.**
- Entry points table (`krawatte`, `-f`, positional, `-f`+positional error) → Task 4 (`conflicts_with`, `resolve_specs`, tests).
- Discovery walks up, exact name, "no Krawattefile found" exit 2 → Tasks 2, 4.
- File format keys, required/optional, defaults, `deny_unknown_fields`, at least one proc, file order → Task 1.
- Name rules with the spec's exact messages → Task 1 (`name_errors_say_why` asserts the literal strings).
- `cwd` default = project dir; relative `cwd`; must exist → Tasks 1, 3 (`spawn_specs_applies_cwd_and_env…`, `adhoc_slots_inherit_krawattes_cwd`), 4 smoke step 1.
- `env` layered over inherited → Task 3 (`cmd.envs`, test).
- `watch` bare-string-is-keyword rule, `["self"]` is a path, other bare strings rejected with guidance → Task 1.
- Every generation, including restarts, uses cwd/env → Task 3 (`complete` passes `spec`; test restarts and re-checks).
- Names in status bar and final printout → Task 3 (`short_name` returns `spec.name`; `run` already derives `names` from it).
- `-t` overrides `settings.timeout` overrides 5 → Task 4 (`grace_period` test).
- Errors all at once, exit 2, before the terminal → Task 1 (collection), Task 4 (`main` exits before `run`).
- README → Task 4.

**Placeholder scan.** None.

**Type consistency.** `ProcSpec { name, command, cwd: Option<PathBuf>, env: Vec<(String,String)>, watch: Watch, ignore: Vec<String> }` identical in Tasks 1, 3, 4. `parse(text, path, project_dir)`, `discover(&Path) -> Option<PathBuf>`, `load(&Path) -> Result<Krawattefile, Vec<ConfigError>>` consistent across Tasks 1, 2, 4. `spawn_specs(&[ProcSpec], &Config, Sender<Event>)` defined in Task 3, used in Task 4. `resolve_specs` / `grace_period` signatures match their tests. Test counts: 87 → 98 → 102 → 104 → 109.
