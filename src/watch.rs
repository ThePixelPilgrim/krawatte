//! Restart-on-change: resolving `watch` entries, debouncing filesystem
//! events, and the watcher thread that turns them into [`Event::Changed`].
//!
//! Resolution and debouncing are pure and unit-tested; only [`start`] talks
//! to `notify`.

#![allow(dead_code)] // wired in by a later task

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{ConfigError, Krawattefile, ProcSpec, Watch};
use crate::types::{Changed, ProcId};

/// Patterns every slot ignores. Editor temp files, VCS and build trees: the
/// things that change constantly and never mean "restart me". Directory
/// names here are also not descended into by the watcher.
pub const DEFAULT_IGNORE: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "*.swp",
    "*.swx",
    "*~",
    ".#*",
    "#*#",
    "4913",
    ".DS_Store",
];

/// How much of the filesystem one `watch` entry covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTarget {
    /// A directory, watched recursively (minus ignored subtrees).
    Dir(PathBuf),
    /// One file, observed through its parent directory so that a write-then-
    /// rename into place is seen and the file is complete when it is.
    File { dir: PathBuf, name: OsString },
}

/// Everything one slot watches.
#[derive(Debug, Clone)]
pub struct SlotWatch {
    pub proc: ProcId,
    pub targets: Vec<WatchTarget>,
    pub ignore: GlobSet,
}

impl SlotWatch {
    /// Whether a change at `path` (absolute) concerns this slot. Directory
    /// targets cover descendants that are not ignored; file targets match
    /// exactly that file and are never ignored.
    pub fn matches(&self, path: &Path) -> bool {
        self.targets.iter().any(|t| match t {
            WatchTarget::Dir(root) => path
                .strip_prefix(root)
                .is_ok_and(|rel| !ignored(&self.ignore, rel)),
            WatchTarget::File { dir, name } => {
                path.parent() == Some(dir.as_path()) && path.file_name() == Some(name.as_os_str())
            }
        })
    }
}

/// True if the relative path, or any single component of it, matches the
/// set. Matching components makes `target` mean "a directory called target
/// anywhere", as `.gitignore` users expect, without matching `targets`.
pub fn ignored(set: &GlobSet, relative: &Path) -> bool {
    set.is_match(relative)
        || relative
            .components()
            .any(|c| set.is_match(Path::new(c.as_os_str())))
}

/// The default patterns plus a slot's own.
fn ignore_set(extra: &[String]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for pat in DEFAULT_IGNORE
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
    {
        let glob = Glob::new(&pat).map_err(|e| format!("ignore pattern {pat:?}: {e}"))?;
        b.add(glob);
    }
    b.build().map_err(|e| e.to_string())
}

/// Resolve every slot's `watch` entries against its working directory.
/// Slots without `watch` are skipped. All errors are returned together so
/// they can join the other config errors.
pub fn resolve_all(file: &Krawattefile) -> Result<Vec<SlotWatch>, Vec<ConfigError>> {
    let mut slots = Vec::new();
    let mut errors = Vec::new();
    let err = |message: String| ConfigError {
        path: file.path.clone(),
        line: None,
        message,
    };
    for (proc, spec) in file.procs.iter().enumerate() {
        let base = spec.cwd.clone().unwrap_or_else(|| file.project_dir.clone());
        let entries: Vec<PathBuf> = match &spec.watch {
            Watch::None => continue,
            Watch::SelfBinary => match self_target(spec) {
                Ok(p) => vec![p],
                Err(m) => {
                    errors.push(err(m));
                    Vec::new()
                }
            },
            Watch::Paths(paths) => paths.iter().map(PathBuf::from).collect(),
        };
        let mut targets = Vec::with_capacity(entries.len());
        for entry in entries {
            let abs = base.join(&entry);
            if abs.is_dir() {
                targets.push(WatchTarget::Dir(abs));
            } else if let (Some(dir), Some(name)) = (abs.parent(), abs.file_name())
                && dir.is_dir()
            {
                targets.push(WatchTarget::File {
                    dir: dir.to_path_buf(),
                    name: name.to_os_string(),
                });
            } else {
                errors.push(err(format!(
                    "proc {:?}: watch path {:?} does not exist and neither does its parent directory",
                    spec.name,
                    entry.display()
                )));
            }
        }
        match ignore_set(&spec.ignore) {
            Ok(ignore) => slots.push(SlotWatch {
                proc,
                targets,
                ignore,
            }),
            Err(m) => errors.push(err(format!("proc {:?}: {m}", spec.name))),
        }
    }
    if errors.is_empty() {
        Ok(slots)
    } else {
        Err(errors)
    }
}

/// The path `watch = "self"` means: the command's first token, which must
/// look like a path. A bare program name is found through `$PATH` and is not
/// what "watch myself" means.
fn self_target(spec: &ProcSpec) -> Result<PathBuf, String> {
    let first = spec.command.split_whitespace().next().unwrap_or("");
    if first.contains('/') {
        Ok(PathBuf::from(first))
    } else {
        Err(format!(
            "proc {:?}: watch = \"self\" needs a path as the command's first token (got {first:?}); \"self\" cannot watch a program found through $PATH",
            spec.name
        ))
    }
}

/// How many changed paths a marker names; the rest are counted.
pub const SHOWN_PATHS: usize = 3;

/// Per-slot pending change.
#[derive(Debug)]
struct Pending {
    paths: Vec<PathBuf>,
    more: usize,
    last: Instant,
}

/// Quiet-period debounce: a slot's restart fires once `quiet` has passed
/// with no further event for it. Driven by an explicit clock so it can be
/// tested without sleeping, like the shutdown machine.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    pending: BTreeMap<ProcId, Pending>,
}

impl Debouncer {
    pub fn new(quiet: Duration) -> Debouncer {
        Debouncer {
            quiet,
            pending: BTreeMap::new(),
        }
    }

    /// Record a change at `path` (project-relative) for `proc` at time `now`.
    pub fn observe(&mut self, proc: ProcId, path: PathBuf, now: Instant) {
        let p = self.pending.entry(proc).or_insert_with(|| Pending {
            paths: Vec::new(),
            more: 0,
            last: now,
        });
        p.last = now;
        if p.paths.contains(&path) {
            return;
        }
        if p.paths.len() < SHOWN_PATHS {
            p.paths.push(path);
        } else {
            p.more += 1;
        }
    }

    /// When the earliest pending slot would fire if nothing else happens.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.last + self.quiet).min()
    }

    /// Every slot whose quiet period has elapsed by `now`, in slot order;
    /// they are removed from the pending set.
    pub fn due(&mut self, now: Instant) -> Vec<Changed> {
        let ready: Vec<ProcId> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.last) >= self.quiet)
            .map(|(&proc, _)| proc)
            .collect();
        ready
            .into_iter()
            .map(|proc| {
                let p = self.pending.remove(&proc).expect("listed as ready");
                Changed {
                    proc,
                    paths: p.paths,
                    more: p.more,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn spec(name: &str, command: &str, cwd: &Path, watch: Watch, ignore: &[&str]) -> ProcSpec {
        ProcSpec {
            name: name.to_string(),
            command: command.to_string(),
            cwd: Some(cwd.to_path_buf()),
            env: Vec::new(),
            watch,
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn file_with(project: &Path, procs: Vec<ProcSpec>) -> Krawattefile {
        Krawattefile {
            path: project.join("Krawattefile"),
            project_dir: project.to_path_buf(),
            timeout: None,
            debounce: None,
            procs,
        }
    }

    fn paths(v: &[&str]) -> Watch {
        Watch::Paths(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn directories_files_and_absent_files_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        let kf = file_with(
            &root,
            vec![
                spec(
                    "a",
                    "x",
                    &root,
                    paths(&["src", "config.toml", "target/debug/app"]),
                    &[],
                ),
                spec("b", "y", &root, Watch::None, &[]),
            ],
        );
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(slots.len(), 1, "slots without watch are skipped");
        assert_eq!(slots[0].proc, 0);
        assert_eq!(
            slots[0].targets,
            vec![
                WatchTarget::Dir(root.join("src")),
                WatchTarget::File {
                    dir: root.clone(),
                    name: "config.toml".into()
                },
                WatchTarget::File {
                    dir: root.join("target/debug"),
                    name: "app".into()
                },
            ]
        );
    }

    #[test]
    fn entries_resolve_against_the_slots_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("frontend/src")).unwrap();
        let kf = file_with(
            &root,
            vec![spec(
                "web",
                "npm run dev",
                &root.join("frontend"),
                paths(&["src"]),
                &[],
            )],
        );
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(
            slots[0].targets,
            vec![WatchTarget::Dir(root.join("frontend/src"))]
        );
    }

    #[test]
    fn self_watches_the_commands_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let kf = file_with(
            &root,
            vec![spec(
                "server",
                "target/debug/app --port 1",
                &root,
                Watch::SelfBinary,
                &[],
            )],
        );
        let slots = resolve_all(&kf).unwrap();
        assert_eq!(
            slots[0].targets,
            vec![WatchTarget::File {
                dir: root.join("target/debug"),
                name: "app".into()
            }]
        );

        let abs = file_with(
            &root,
            vec![spec(
                "s",
                &format!("{}/target/debug/app", root.display()),
                &root,
                Watch::SelfBinary,
                &[],
            )],
        );
        assert_eq!(resolve_all(&abs).unwrap()[0].targets, slots[0].targets);
    }

    #[test]
    fn self_requires_a_path_like_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let kf = file_with(
            &root,
            vec![spec("web", "npm run dev", &root, Watch::SelfBinary, &[])],
        );
        let errs = resolve_all(&kf).unwrap_err();
        assert_eq!(errs.len(), 1);
        let msg = errs[0].to_string();
        assert!(msg.contains("proc \"web\""), "{msg}");
        assert!(msg.contains("\"npm\""), "{msg}");
        assert!(msg.contains("$PATH"), "{msg}");
    }

    #[test]
    fn missing_parent_and_bad_glob_are_errors_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let kf = file_with(
            &root,
            vec![spec(
                "a",
                "x",
                &root,
                paths(&["nope/deeper/file"]),
                &["[unclosed"],
            )],
        );
        let errs = resolve_all(&kf).unwrap_err();
        assert_eq!(errs.len(), 2, "{errs:#?}");
        assert!(
            errs[0]
                .to_string()
                .contains("neither does its parent directory"),
            "{}",
            errs[0]
        );
        assert!(
            errs[1].to_string().contains("ignore pattern"),
            "{}",
            errs[1]
        );
    }

    #[test]
    fn default_and_custom_ignores_match_components_and_names() {
        let set = ignore_set(&["*.log".to_string()]).unwrap();
        assert!(ignored(&set, Path::new("node_modules/x/index.js")));
        assert!(ignored(&set, Path::new("src/.git/HEAD")));
        assert!(ignored(&set, Path::new("src/main.rs.swp")));
        assert!(ignored(&set, Path::new("src/4913")));
        assert!(ignored(&set, Path::new("src/.#main.rs")));
        assert!(ignored(&set, Path::new("logs/app.log")));
        assert!(!ignored(&set, Path::new("src/main.rs")));
        assert!(
            !ignored(&set, Path::new("targets/main.rs")),
            "only whole components match"
        );
    }

    #[test]
    fn slot_matches_dir_descendants_unless_ignored_and_exact_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let kf = file_with(
            &root,
            vec![spec(
                "a",
                "x",
                &root,
                paths(&["src", "target/debug/app"]),
                &[],
            )],
        );
        let slot = resolve_all(&kf).unwrap().remove(0);
        assert!(slot.matches(&root.join("src/a/b.rs")));
        assert!(!slot.matches(&root.join("src/x.swp")));
        assert!(!slot.matches(&root.join("srcs/x.rs")));
        assert!(slot.matches(&root.join("target/debug/app")));
        assert!(!slot.matches(&root.join("target/debug/app.d")));
        assert!(!slot.matches(&root.join("target/debug/deps/app")));
    }

    fn t0() -> Instant {
        Instant::now()
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn fires_once_the_slot_has_been_quiet() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        assert_eq!(d.next_deadline(), None);
        d.observe(0, "a.rs".into(), start);
        assert_eq!(d.next_deadline(), Some(start + ms(100)));
        assert!(d.due(start + ms(99)).is_empty());
        assert_eq!(
            d.due(start + ms(100)),
            vec![Changed {
                proc: 0,
                paths: vec!["a.rs".into()],
                more: 0
            }]
        );
        assert!(d.due(start + ms(1000)).is_empty(), "fires once");
        assert_eq!(d.next_deadline(), None);
    }

    #[test]
    fn a_burst_extends_the_quiet_period_and_coalesces_paths() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        d.observe(0, "a".into(), start);
        d.observe(0, "b".into(), start + ms(50));
        d.observe(0, "a".into(), start + ms(80)); // duplicate, not counted again
        d.observe(0, "c".into(), start + ms(120));
        d.observe(0, "d".into(), start + ms(130));
        d.observe(0, "e".into(), start + ms(140));
        assert!(
            d.due(start + ms(200)).is_empty(),
            "still within 100ms of the last event"
        );
        assert_eq!(
            d.due(start + ms(240)),
            vec![Changed {
                proc: 0,
                paths: vec!["a".into(), "b".into(), "c".into()],
                more: 2
            }]
        );
    }

    #[test]
    fn slots_are_independent_and_returned_in_slot_order() {
        let start = t0();
        let mut d = Debouncer::new(ms(100));
        d.observe(2, "x".into(), start);
        d.observe(1, "y".into(), start + ms(50));
        assert_eq!(d.next_deadline(), Some(start + ms(100)));
        let first = d.due(start + ms(100));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].proc, 2);
        assert_eq!(d.next_deadline(), Some(start + ms(150)));
        d.observe(0, "z".into(), start + ms(50));
        let rest = d.due(start + ms(150));
        assert_eq!(rest.iter().map(|c| c.proc).collect::<Vec<_>>(), vec![0, 1]);
    }
}
