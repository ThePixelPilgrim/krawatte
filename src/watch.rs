//! Restart-on-change: resolving `watch` entries, debouncing filesystem
//! events, and the watcher thread that turns them into [`Event::Changed`].
//!
//! Resolution and debouncing are pure and unit-tested; only [`start`] talks
//! to `notify`.

#![allow(dead_code)] // wired in by a later task

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::event::ModifyKind;
use notify::{ErrorKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::{ConfigError, Krawattefile, ProcSpec, Watch};
use crate::types::{Changed, Event, ProcId};

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

/// Whether a notify event kind means content changed. Access and metadata
/// (chmod, mtime-only) events are noise.
fn is_change(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

/// Owns the `notify` watcher and the set of directories registered with it.
/// Descends directory trees itself, non-recursively per directory, so that
/// ignored subtrees (`target`, `node_modules`, `.git`) are never registered.
struct Registry {
    watcher: RecommendedWatcher,
    dirs: HashSet<PathBuf>,
    /// Directory roots with their slot's ignore set, for deciding whether a
    /// newly created directory should be descended into.
    roots: Vec<(PathBuf, GlobSet)>,
}

impl Registry {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error> {
        if self.dirs.contains(dir) {
            return Ok(());
        }
        self.watcher.watch(dir, RecursiveMode::NonRecursive)?;
        self.dirs.insert(dir.to_path_buf());
        Ok(())
    }

    /// Register `start` and every non-ignored directory below it (relative
    /// to `root` for ignore matching). Symlinked directories are not
    /// followed, so a link cycle cannot recurse forever.
    fn watch_tree(
        &mut self,
        root: &Path,
        start: &Path,
        ignore: &GlobSet,
    ) -> Result<(), notify::Error> {
        let mut stack = vec![start.to_path_buf()];
        while let Some(dir) = stack.pop() {
            self.watch_dir(&dir)?;
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if !is_dir {
                    continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if !ignored(ignore, rel) {
                    stack.push(path);
                }
            }
        }
        Ok(())
    }

    /// A directory appeared under one of the roots: start watching it too.
    fn on_new_dir(&mut self, path: &Path) {
        let matching: Vec<(PathBuf, GlobSet)> = self
            .roots
            .iter()
            .filter(|(root, ignore)| {
                path.strip_prefix(root)
                    .is_ok_and(|rel| !ignored(ignore, rel))
            })
            .cloned()
            .collect();
        for (root, ignore) in matching {
            // Best effort: a failure here only means a subtree is unwatched;
            // the next change at a higher level still restarts the slot.
            let _ = self.watch_tree(&root, path, &ignore);
        }
    }
}

/// Register every slot's targets, then run the watcher on a detached thread
/// that emits [`Event::Changed`] on `tx` after debouncing. Registration
/// failures are returned so the caller can refuse to start half-watched;
/// the inotify limit gets a message naming the sysctl to raise.
pub fn start(
    slots: Vec<SlotWatch>,
    project_dir: PathBuf,
    quiet: Duration,
    tx: Sender<Event>,
) -> Result<(), ConfigError> {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let watcher = notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    })
    .map_err(|e| watch_error(&project_dir, &project_dir, &e))?;
    let mut registry = Registry {
        watcher,
        dirs: HashSet::new(),
        roots: Vec::new(),
    };

    for slot in &slots {
        for target in &slot.targets {
            let result = match target {
                WatchTarget::Dir(root) => {
                    registry.roots.push((root.clone(), slot.ignore.clone()));
                    registry.watch_tree(root, root, &slot.ignore)
                }
                WatchTarget::File { dir, .. } => registry.watch_dir(dir),
            };
            if let Err(e) = result {
                let shown = match target {
                    WatchTarget::Dir(p) => p.clone(),
                    WatchTarget::File { dir, .. } => dir.clone(),
                };
                return Err(watch_error(&project_dir, &shown, &e));
            }
        }
    }

    std::thread::spawn(move || {
        let mut debouncer = Debouncer::new(quiet);
        loop {
            let timeout = debouncer
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(1));
            match raw_rx.recv_timeout(timeout) {
                Ok(Ok(ev)) => {
                    if !is_change(&ev.kind) {
                        continue;
                    }
                    let now = Instant::now();
                    for path in &ev.paths {
                        if matches!(ev.kind, EventKind::Create(_)) && path.is_dir() {
                            registry.on_new_dir(path);
                        }
                        let rel = path
                            .strip_prefix(&project_dir)
                            .unwrap_or(path)
                            .to_path_buf();
                        for slot in &slots {
                            if slot.matches(path) {
                                debouncer.observe(slot.proc, rel.clone(), now);
                            }
                        }
                    }
                }
                // A backend error for one path is not fatal; the next event
                // for that slot still arrives.
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            for changed in debouncer.due(Instant::now()) {
                if tx.send(Event::Changed(changed)).is_err() {
                    return;
                }
            }
        }
    });
    Ok(())
}

/// A registration failure as a config error: names the path and, for the
/// inotify limit, what to do about it.
fn watch_error(project_dir: &Path, path: &Path, e: &notify::Error) -> ConfigError {
    let shown = path.strip_prefix(project_dir).unwrap_or(path).display();
    let message = match e.kind {
        ErrorKind::MaxFilesWatch => format!(
            "cannot watch {shown:?}: inotify watch limit reached; raise fs.inotify.max_user_watches (sysctl) or narrow the watch paths"
        ),
        _ => format!("cannot watch {shown:?}: {e}"),
    };
    ConfigError {
        path: project_dir.join(crate::config::FILE_NAME),
        line: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn slot_for(root: &Path, proc: ProcId, watch: Watch) -> SlotWatch {
        let kf = file_with(root, vec![spec("s", "x", root, watch, &[])]);
        let mut s = resolve_all(&kf).unwrap().remove(0);
        s.proc = proc;
        s
    }

    fn next_changed(rx: &mpsc::Receiver<Event>, within: Duration) -> Option<Changed> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Changed(c)) => return Some(c),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    #[test]
    fn a_write_under_a_watched_directory_yields_one_changed_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(
            vec![slot_for(&root, 3, paths(&["src"]))],
            root.clone(),
            ms(50),
            tx,
        )
        .unwrap();

        fs::write(root.join("src/nested/a.rs"), "x").unwrap();
        fs::write(root.join("src/nested/a.rs"), "xy").unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("a change");
        assert_eq!(c.proc, 3);
        assert_eq!(c.paths, vec![PathBuf::from("src/nested/a.rs")]);
        assert!(
            next_changed(&rx, ms(300)).is_none(),
            "the burst was coalesced"
        );
    }

    #[test]
    fn a_rename_onto_a_watched_file_is_seen_and_unrelated_files_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(
            vec![slot_for(&root, 0, paths(&["target/debug/app"]))],
            root.clone(),
            ms(50),
            tx,
        )
        .unwrap();

        fs::write(root.join("target/debug/app.d"), "dep info").unwrap();
        assert!(
            next_changed(&rx, ms(400)).is_none(),
            "sibling file is not the target"
        );

        fs::write(root.join("target/debug/app.tmp"), "binary").unwrap();
        fs::rename(
            root.join("target/debug/app.tmp"),
            root.join("target/debug/app"),
        )
        .unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("the rename");
        assert_eq!(c.paths, vec![PathBuf::from("target/debug/app")]);
    }

    #[test]
    fn ignored_paths_produce_nothing_and_new_directories_are_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        let (tx, rx) = mpsc::channel();
        start(
            vec![slot_for(&root, 0, paths(&["."]))],
            root.clone(),
            ms(50),
            tx,
        )
        .unwrap();

        fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(root.join("src/main.rs.swp"), "x").unwrap();
        assert!(next_changed(&rx, ms(400)).is_none());

        fs::create_dir(root.join("src/new")).unwrap();
        // Give the registry a moment to add the new directory before writing into it.
        std::thread::sleep(ms(200));
        let _ = next_changed(&rx, ms(200)); // the directory creation itself is a change; drain it
        fs::write(root.join("src/new/b.rs"), "x").unwrap();
        let c = next_changed(&rx, Duration::from_secs(3)).expect("write in a new subdirectory");
        assert!(c.paths.contains(&PathBuf::from("src/new/b.rs")), "{c:?}");
    }

    #[test]
    fn start_fails_fast_on_an_unwatchable_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let slot = SlotWatch {
            proc: 0,
            targets: vec![WatchTarget::Dir(root.join("vanished"))],
            ignore: ignore_set(&[]).unwrap(),
        };
        let (tx, _rx) = mpsc::channel();
        let err = start(vec![slot], root, ms(50), tx).unwrap_err();
        assert!(err.to_string().contains("cannot watch"), "{err}");
    }
}
