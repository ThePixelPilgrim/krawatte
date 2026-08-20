//! Krawattefile: the TOML description of a cluster, its discovery, parsing
//! and validation.
//!
//! Parsing collects every error it can find and reports them together, with
//! line numbers taken from `toml`'s spans, so one round trip fixes the file.
//! Apart from checking that a `cwd` exists this module is pure.

// Wired in by a later task: nothing outside the tests calls into this module
// until discovery (Task 2) and the CLI (Task 4) land. Remove then.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

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
pub fn parse(
    text: &str,
    path: &Path,
    project_dir: &Path,
) -> Result<Krawattefile, Vec<ConfigError>> {
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
            errors.push(err(
                None,
                format!("settings.timeout must not be negative (got {t})"),
            ));
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
            errors.push(err(
                Some(name_line),
                format!("proc name {name:?} {problem}"),
            ));
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
        return Some(
            "must contain a letter, `_` or `-` (all-digit names would be mistaken for slot indices)",
        );
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
        assert_eq!(
            server.env,
            vec![("RUST_LOG".to_string(), "debug,sqlx=warn".to_string())]
        );
        assert_eq!(server.watch, Watch::SelfBinary);

        let web = &kf.procs[2];
        assert_eq!(
            web.cwd.as_deref(),
            Some(dir.path().join("frontend").as_path())
        );
        assert_eq!(web.watch, Watch::None);
        assert_eq!(web.ignore, vec!["*.log".to_string()]);
    }

    #[test]
    fn a_path_literally_named_self_goes_in_the_array() {
        let dir = tempfile::tempdir().unwrap();
        let kf = parse_in(
            dir.path(),
            "[[proc]]\nname = \"a\"\ncmd = \"x\"\nwatch = [\"self\", \"src\"]\n",
        )
        .unwrap();
        assert_eq!(
            kf.procs[0].watch,
            Watch::Paths(vec!["self".to_string(), "src".to_string()])
        );
    }

    #[test]
    fn bare_string_watch_other_than_self_is_rejected_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(
            parse_in(
                dir.path(),
                "[[proc]]\nname = \"a\"\ncmd = \"x\"\nwatch = \"src\"\n",
            )
            .unwrap_err(),
        );
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
        let errs = messages(
            parse_in(
                dir.path(),
                "[[proc]]\nname = \"server\"\ncmd = \"x\"\ncwd = \"platform/srv\"\n",
            )
            .unwrap_err(),
        );
        assert_eq!(
            errs,
            vec![
                "Krawattefile:4: proc \"server\": cwd \"platform/srv\" does not exist".to_string()
            ]
        );
    }

    #[test]
    fn unknown_keys_and_missing_required_keys_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let errs =
            messages(parse_in(dir.path(), "[[proc]]\nname = \"a\"\ncomand = \"x\"\n").unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("comand"), "{}", errs[0]);

        let errs = messages(parse_in(dir.path(), "[[proc]]\ncmd = \"x\"\n").unwrap_err());
        assert!(errs[0].contains("name"), "{}", errs[0]);

        let errs = messages(
            parse_in(
                dir.path(),
                "[settings]\ntimeou = 1\n[[proc]]\nname = \"a\"\ncmd = \"x\"\n",
            )
            .unwrap_err(),
        );
        assert!(errs[0].contains("timeou"), "{}", errs[0]);
    }

    #[test]
    fn env_values_must_be_strings() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(
            parse_in(
                dir.path(),
                "[[proc]]\nname = \"a\"\ncmd = \"x\"\nenv = { PORT = 8080 }\n",
            )
            .unwrap_err(),
        );
        assert_eq!(errs.len(), 1);
        assert!(errs[0].starts_with("Krawattefile:4:"), "{}", errs[0]);
    }

    #[test]
    fn empty_file_and_negative_timeout_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let errs = messages(parse_in(dir.path(), "").unwrap_err());
        assert_eq!(
            errs,
            vec![
                "Krawattefile: no [[proc]] entries: a Krawattefile needs at least one process"
                    .to_string()
            ]
        );

        let errs = messages(
            parse_in(
                dir.path(),
                "[settings]\ntimeout = -1\n[[proc]]\nname = \"a\"\ncmd = \"x\"\n",
            )
            .unwrap_err(),
        );
        assert_eq!(
            errs,
            vec!["Krawattefile: settings.timeout must not be negative (got -1)".to_string()]
        );
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
        fs::write(
            &file,
            "[[proc]]\nname = \"web\"\ncmd = \"npm run dev\"\ncwd = \"frontend\"\n",
        )
        .unwrap();

        let kf = load(&file).unwrap();
        let canon = project.canonicalize().unwrap();
        assert_eq!(kf.project_dir, canon);
        assert_eq!(
            kf.procs[0].cwd.as_deref(),
            Some(canon.join("frontend").as_path())
        );
        // Errors keep the path as the user gave it.
        assert_eq!(kf.path, file);
    }

    #[test]
    fn load_reports_an_unreadable_file_as_a_config_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("nope").join(FILE_NAME);
        let errs = messages(load(&missing).unwrap_err());
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].starts_with(&format!("{}: cannot read", missing.display())),
            "{}",
            errs[0]
        );
    }
}
