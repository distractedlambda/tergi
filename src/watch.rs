//! Watching the working tree and the git directory for changes.
//!
//! In the git directory, we watch the folders that hold refs, the index and
//! the state of a rebase or merge.
//!
//! For the working tree, git tells us what it ignores. Each time the app
//! loads the status, it passes the ignored paths on to us through
//! `Updates`, and we drop events from them. On Linux, inotify watches
//! single folders, so we also walk the working tree and watch each folder
//! that isn't ignored. Build output folders like `target/` or
//! `node_modules/` are never watched, which keeps us well under the inotify
//! limit and avoids refresh storms during builds. On macOS, FSEvents watches
//! the whole tree cheaply, so we watch the root.
//!
//! We don't watch the files that ignore rules come from, apart from
//! `.gitignore` files and `info/exclude`. When rules change elsewhere, the
//! next status picks that up.
//!
//! Events are collected for a short while and then reported as a set of
//! flags saying what needs to be reloaded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gix::bstr::BString;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::git::status::Status;
use crate::util::fs_path;

pub const WORKTREE: u8 = 1;
pub const INDEX: u8 = 2;
pub const REFS: u8 = 4;
pub const STATE: u8 = 8;

pub enum Event {
    Changed(u8),
    /// Watching the working tree failed. Changes there will only be picked
    /// up by polling.
    Degraded(String),
}

pub struct Handle {
    _watcher: Arc<Mutex<RecommendedWatcher>>,
    updates: Updates,
}

impl Handle {
    pub fn updates(&self) -> Updates {
        self.updates.clone()
    }
}

/// Tells the watcher what git ignores. The app sends every status it loads.
#[derive(Clone)]
pub struct Updates(UnboundedSender<Vec<BString>>);

impl Updates {
    pub fn send(&self, status: &Status) {
        let _ = self.0.send(status.ignored.clone());
    }
}

/// Wait this long after the last event before reporting...
const QUIET: Duration = Duration::from_millis(120);
/// ...but never longer than this after the first one.
const MAX_DELAY: Duration = Duration::from_millis(600);

#[derive(Clone)]
struct Roots {
    workdir: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

/// Start watching. Returns a handle that keeps the watches alive, and a
/// future that reports changes through `report`. The caller should spawn
/// the future. It ends when the handle is dropped.
pub fn start(
    workdir: &Path,
    git_dir: &Path,
    common_dir: &Path,
    report: impl Fn(Event) + Send + 'static,
) -> Result<(Handle, impl Future<Output = ()> + Send + 'static)> {
    let roots = Roots {
        workdir: workdir.canonicalize().context("bad workdir")?,
        git_dir: git_dir.canonicalize().context("bad git dir")?,
        common_dir: common_dir.canonicalize().context("bad common dir")?,
    };
    let (tx, mut rx) = unbounded_channel::<notify::Result<notify::Event>>();
    let watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("could not start file watcher")?;
    let watcher = Arc::new(Mutex::new(watcher));
    let (updates_tx, mut updates_rx) = unbounded_channel::<Vec<BString>>();

    {
        let mut w = watcher
            .lock()
            .map_err(|_| anyhow::anyhow!("watcher lock"))?;
        for dir in [&roots.git_dir, &roots.common_dir] {
            w.watch(dir, RecursiveMode::NonRecursive)
                .context("could not watch the git directory")?;
            let refs = dir.join("refs");
            if refs.is_dir() {
                let _ = w.watch(&refs, RecursiveMode::Recursive);
            }
            // Repositories that use the reftable format keep all their refs
            // and reflogs in here. A linked worktree has its own folder for
            // its HEAD.
            let reftable = dir.join("reftable");
            if reftable.is_dir() {
                let _ = w.watch(&reftable, RecursiveMode::NonRecursive);
            }
        }
        let info = roots.common_dir.join("info");
        if info.is_dir() {
            let _ = w.watch(&info, RecursiveMode::NonRecursive);
        }
    }
    let mut watches = Watches {
        reflogs: watch_reflogs(&watcher, &roots.common_dir),
        ..Watches::default()
    };

    // On Linux we watch the rest of the tree once we know what git ignores.
    let mode = if cfg!(target_os = "linux") {
        RecursiveMode::NonRecursive
    } else {
        RecursiveMode::Recursive
    };
    let watched_root = watcher
        .lock()
        .map_err(|_| "watcher lock poisoned".to_string())
        .and_then(|mut w| w.watch(&roots.workdir, mode).map_err(|e| e.to_string()));
    let mut degraded = false;
    match watched_root {
        Ok(()) => {
            watches.tree.insert(roots.workdir.clone());
        }
        Err(e) => {
            degraded = true;
            report(Event::Degraded(e));
        }
    }

    // Hold the watcher weakly, so dropping the handle stops notify and ends
    // this future.
    let w2 = Arc::downgrade(&watcher);
    let run = async move {
        let mut pending: u8 = 0;
        let mut first: Option<Instant> = None;
        let mut new_dirs: HashSet<PathBuf> = HashSet::new();
        loop {
            let timeout = first.map_or(QUIET, |t| QUIET.min(MAX_DELAY.saturating_sub(t.elapsed())));
            // Adding watches walks folders, so we do that off the async
            // threads. Both branches that do it hand back an error if
            // adding a watch failed.
            let failed = tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(Ok(ev)) => {
                            if matches!(ev.kind, EventKind::Access(_)) {
                                continue;
                            }
                            // A folder can also appear by being moved in.
                            let appeared = matches!(
                                ev.kind,
                                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
                            );
                            for p in &ev.paths {
                                let flags = classify(&roots, p, &watches.ignored);
                                if flags == 0 {
                                    continue;
                                }
                                if cfg!(target_os = "linux")
                                    && flags == WORKTREE
                                    && appeared
                                    && p.is_dir()
                                {
                                    new_dirs.insert(p.clone());
                                }
                                pending |= flags;
                                first.get_or_insert_with(Instant::now);
                            }
                            None
                        }
                        Some(Err(_)) => None,
                        None => break,
                    }
                }
                Some(paths) = updates_rx.recv() => {
                    let Some(w) = w2.upgrade() else { break };
                    let workdir = roots.workdir.clone();
                    let mut ws = std::mem::take(&mut watches);
                    let (ws, r) = crate::util::blocking(move || {
                        let r = ws.update(&w, &workdir, &paths);
                        (ws, r)
                    })
                    .await;
                    watches = ws;
                    match r {
                        Ok(true) => {
                            // Folders that stopped being ignored may have
                            // changed after git looked at them and before we
                            // watched them.
                            pending |= WORKTREE;
                            first.get_or_insert_with(Instant::now);
                            None
                        }
                        Ok(false) => None,
                        Err(e) => Some(e),
                    }
                }
                () = tokio::time::sleep(timeout), if first.is_some() => {
                    let Some(w) = w2.upgrade() else { break };
                    // A fresh repository has no reflogs until its first
                    // commit, which also changes a ref.
                    let check_reflogs = !watches.reflogs && pending & REFS != 0;
                    let mut failed = None;
                    if check_reflogs || !new_dirs.is_empty() {
                        let common_dir = roots.common_dir.clone();
                        let workdir = roots.workdir.clone();
                        let dirs: Vec<PathBuf> = new_dirs.drain().collect();
                        let mut ws = std::mem::take(&mut watches);
                        let (ws, r) = crate::util::blocking(move || {
                            if check_reflogs {
                                ws.reflogs = watch_reflogs(&w, &common_dir);
                            }
                            // Watch new folders before reporting, so that
                            // nothing written to them after the status
                            // looked at them goes unnoticed.
                            let mut r = Ok(());
                            for d in dirs {
                                r = r.and(ws.watch_tree(&w, &workdir, &d).map(|_| ()));
                            }
                            (ws, r)
                        })
                        .await;
                        watches = ws;
                        failed = r.err();
                    }
                    if pending != 0 {
                        report(Event::Changed(pending));
                    }
                    pending = 0;
                    first = None;
                    failed
                }
            };
            if let Some(e) = failed
                && !degraded
            {
                degraded = true;
                report(Event::Degraded(format!(
                    "Can't watch every folder ({e}). The working directory will be checked every few seconds instead."
                )));
            }
        }
    };

    let handle = Handle {
        _watcher: watcher,
        updates: Updates(updates_tx),
    };
    Ok((handle, run))
}

/// Watch the folder that holds the stash reflog. Dropping a stash other
/// than the newest one doesn't change what `refs/stash` points at, and
/// tools built on libgit2 only rewrite the reflog. The stash is shared
/// between worktrees, so its reflog is in the common directory. Returns
/// whether the folder exists and is watched.
fn watch_reflogs(watcher: &Arc<Mutex<RecommendedWatcher>>, common_dir: &Path) -> bool {
    let dir = common_dir.join("logs/refs");
    dir.is_dir()
        && watcher
            .lock()
            .is_ok_and(|mut w| w.watch(&dir, RecursiveMode::NonRecursive).is_ok())
}

/// What git ignores in the working tree, as of the last status. The paths
/// are absolute.
#[derive(Default, Debug, PartialEq, Eq)]
struct Ignored {
    /// Ignored folders, with everything in them.
    dirs: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
}

impl Ignored {
    /// Read the ignored paths from a status. A folder ends in a slash.
    fn new(workdir: &Path, paths: &[BString], old: &Ignored) -> Ignored {
        let mut new = Ignored::default();
        for p in paths {
            match p.strip_suffix(b"/") {
                Some(dir) => new.dirs.insert(workdir.join(fs_path(dir))),
                None => new.files.insert(workdir.join(fs_path(p))),
            };
        }
        // Git can only list folders that exist. A build tool may have just
        // deleted its output folder and will soon make it again. If we
        // forgot that the folder is ignored, we would watch it and
        // everything the build puts in it until the next status. So we
        // keep ignored folders that are gone.
        for d in &old.dirs {
            if std::fs::symlink_metadata(d).is_err() {
                new.dirs.insert(d.clone());
            }
        }
        new
    }

    /// Whether `path` in the working tree is ignored.
    fn covers(&self, workdir: &Path, path: &Path) -> bool {
        self.files.contains(path)
            || path
                .ancestors()
                .take_while(|a| *a != workdir)
                .any(|a| self.dirs.contains(a))
    }
}

/// What we watch besides the git directory. We move this to a blocking
/// thread whenever we change the watches.
#[derive(Default)]
struct Watches {
    /// Whether the folder with the stash reflog is watched.
    reflogs: bool,
    ignored: Ignored,
    /// Whether the app has told us what git ignores yet.
    informed: bool,
    /// The folders of the working tree that we watch. On macOS that is only
    /// the root, which we watch with all its subfolders.
    tree: HashSet<PathBuf>,
}

impl Watches {
    /// Take in the ignored paths from a new status. On Linux, walk the tree
    /// again if the ignored folders changed. Returns whether that added
    /// watches to folders we didn't watch before, not counting the first
    /// walk.
    fn update(
        &mut self,
        watcher: &Arc<Mutex<RecommendedWatcher>>,
        workdir: &Path,
        paths: &[BString],
    ) -> std::result::Result<bool, String> {
        let ignored = Ignored::new(workdir, paths, &self.ignored);
        let walk =
            cfg!(target_os = "linux") && (!self.informed || ignored.dirs != self.ignored.dirs);
        let first = !self.informed;
        self.ignored = ignored;
        self.informed = true;
        if !walk {
            return Ok(false);
        }
        Ok(self.rewatch_tree(watcher, workdir)? && !first)
    }

    /// Walk the whole working tree again. Watch the folders that are no
    /// longer ignored, and stop watching the ones that now are. Returns
    /// whether we added any watches.
    fn rewatch_tree(
        &mut self,
        watcher: &Arc<Mutex<RecommendedWatcher>>,
        workdir: &Path,
    ) -> std::result::Result<bool, String> {
        let mut now = HashSet::new();
        let walked = walk(
            watcher,
            workdir,
            workdir,
            &self.ignored,
            &self.tree,
            &mut now,
        );
        let added = match walked {
            Ok(added) => added,
            Err(e) => {
                // We didn't get through the walk, so we can't tell which
                // folders to stop watching.
                self.tree.extend(now);
                return Err(e);
            }
        };
        if let Ok(mut w) = watcher.lock() {
            for dir in self.tree.difference(&now) {
                let _ = w.unwatch(dir);
            }
        }
        self.tree = now;
        Ok(added)
    }

    /// Watch `start` and every folder under it that git doesn't ignore.
    fn watch_tree(
        &mut self,
        watcher: &Arc<Mutex<RecommendedWatcher>>,
        workdir: &Path,
        start: &Path,
    ) -> std::result::Result<bool, String> {
        let mut found = HashSet::new();
        let r = walk(
            watcher,
            workdir,
            start,
            &self.ignored,
            &self.tree,
            &mut found,
        );
        self.tree.extend(found);
        r
    }
}

/// Watch `start` and every folder under it, except for ignored folders and
/// nested repositories. Folders in `watched` are already watched. Puts every
/// folder that should be watched in `found`, and returns whether any of them
/// is new.
fn walk(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    workdir: &Path,
    start: &Path,
    ignored: &Ignored,
    watched: &HashSet<PathBuf>,
    found: &mut HashSet<PathBuf>,
) -> std::result::Result<bool, String> {
    let mut w = watcher
        .lock()
        .map_err(|_| "watcher lock poisoned".to_string())?;
    let mut added = false;
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if ignored.covers(workdir, &dir) || dir.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if dir != workdir && dir.join(".git").exists() {
            // A nested repository or submodule. Its changes don't show up in
            // our status anyway, apart from the commit it points at.
            continue;
        }
        if !watched.contains(&dir) {
            match w.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => added = true,
                // It was deleted while we walked.
                Err(e) if matches!(e.kind, notify::ErrorKind::PathNotFound) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        // We don't follow symlinks, because git doesn't.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
        found.insert(dir);
    }
    Ok(added)
}

fn classify(roots: &Roots, path: &Path, ignored: &Ignored) -> u8 {
    for dir in [&roots.git_dir, &roots.common_dir] {
        if let Ok(rel) = path.strip_prefix(dir) {
            return classify_git(rel);
        }
    }
    if path.strip_prefix(&roots.workdir).is_ok() {
        if path.components().any(|c| c.as_os_str() == ".git") {
            return 0;
        }
        if ignored.covers(&roots.workdir, path) {
            return 0;
        }
        return WORKTREE;
    }
    0
}

fn classify_git(rel: &Path) -> u8 {
    let s = rel.to_string_lossy();
    if s.ends_with(".lock") || s.is_empty() {
        return 0;
    }
    let first = s.split('/').next().unwrap_or("");
    match first {
        "index" => INDEX,
        "HEAD" | "ORIG_HEAD" | "FETCH_HEAD" | "packed-refs" | "refs" => REFS | INDEX,
        // Remotes and upstreams live in the config. It can also name a
        // different global excludes file.
        "config" | "config.worktree" => REFS | WORKTREE,
        "info" if s == "info/exclude" => WORKTREE,
        // The other reflogs only change along with their refs.
        "logs" if s == "logs/refs/stash" => REFS,
        // Every change to a reftable, including reflog changes, ends with
        // replacing the list of tables.
        "reftable" if s == "reftable/tables.list" => REFS | INDEX,
        "rebase-merge" | "rebase-apply" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        | "sequencer" => STATE | REFS | INDEX,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::{Receiver, channel};

    type Watched = (
        tempfile::TempDir,
        Handle,
        Receiver<u8>,
        tokio::runtime::Runtime,
    );

    /// Watch a new scratch repository and collect the flags it reports.
    fn watched() -> Watched {
        watched_with(&[]).expect("git init failed")
    }

    /// Like `watched`, with extra options for `git init`. Returns `None` if
    /// this git doesn't understand them.
    fn watched_with(init: &[&str]) -> Option<Watched> {
        let dir = tempfile::tempdir().unwrap();
        if !git(dir.path(), &[&["init", "-q"], init].concat()) {
            return None;
        }
        Some(watch_repo(dir))
    }

    /// Watch an existing scratch repository, and tell the watcher what git
    /// ignores, as the app does at startup.
    fn watch_repo(dir: tempfile::TempDir) -> Watched {
        let (tx, rx) = channel();
        let git_dir = dir.path().join(".git");
        let (handle, run) = start(dir.path(), &git_dir, &git_dir, move |e| {
            if let Event::Changed(flags) = e {
                let _ = tx.send(flags);
            }
        })
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.spawn(run);
        tell(dir.path(), &handle);
        (dir, handle, rx, rt)
    }

    /// Load the status and send it to the watcher, as the app does after
    /// each refresh.
    fn tell(dir: &Path, handle: &Handle) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(Status::ARGS)
            .output()
            .unwrap();
        assert!(out.status.success());
        handle
            .updates()
            .send(&crate::git::status::parse(&out.stdout));
    }

    /// Run git quietly and say whether it succeeded.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// Wait until the watcher has been quiet for a second.
    fn settle(rx: &Receiver<u8>) {
        while rx.recv_timeout(Duration::from_secs(1)).is_ok() {}
    }

    /// Write a file and say what the watcher reports for it, if anything.
    fn write(rx: &Receiver<u8>, path: &Path) -> Option<u8> {
        settle(rx);
        std::fs::write(path, "x").unwrap();
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watches_a_directory_that_is_moved_in() {
        let (dir, _handle, rx, _rt) = watched();
        // Next to the repository, so the move stays on one file system.
        let outside = tempfile::tempdir_in(dir.path().parent().unwrap()).unwrap();
        std::fs::create_dir(outside.path().join("sub")).unwrap();
        std::fs::rename(outside.path().join("sub"), dir.path().join("sub")).unwrap();
        let moved = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(moved.map(|f| f & WORKTREE), Ok(WORKTREE));
        // The directory got its watch before the move was reported.
        std::fs::write(dir.path().join("sub/file.txt"), "x").unwrap();
        let written = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(written.map(|f| f & WORKTREE), Ok(WORKTREE));
    }

    #[test]
    fn notices_a_dropped_stash_below_the_top() {
        let (dir, _handle, rx, _rt) = watched();
        drop_older_stash(dir.path(), &rx);
    }

    #[test]
    fn notices_ref_changes_in_a_reftable_repository() {
        let Some((dir, _handle, rx, _rt)) = watched_with(&["--ref-format=reftable"]) else {
            eprintln!("skipped: this git can't create reftable repositories");
            return;
        };
        drop_older_stash(dir.path(), &rx);
        let git = |args: &[&str]| assert!(git(dir.path(), args), "git {args:?} failed");
        settle(&rx);
        git(&["commit", "-q", "--allow-empty", "-m", "b"]);
        let committed = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(committed.map(|f| f & (REFS | INDEX)), Ok(REFS | INDEX));
        settle(&rx);
        git(&["branch", "other"]);
        let branched = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(branched.map(|f| f & REFS), Ok(REFS));
    }

    /// Make two stashes in a repository with one commit, then delete the
    /// older one and check that the watcher reports it.
    fn drop_older_stash(dir: &Path, rx: &Receiver<u8>) {
        let git = |args: &[&str]| assert!(git(dir, args), "git {args:?} failed");
        std::fs::write(dir.join("a"), "1").unwrap();
        git(&["add", "a"]);
        git(&["commit", "-qm", "a"]);
        // Let the watcher pick up the new reflog folder before stashing.
        settle(rx);
        for n in ["2", "3"] {
            std::fs::write(dir.join("a"), n).unwrap();
            git(&["stash", "-q"]);
        }
        settle(rx);
        // `git stash drop` also rewrites `refs/stash`, so it would be noticed
        // anyway. Other tools, such as anything built on libgit2, only
        // rewrite the reflog, like this does.
        git(&["reflog", "delete", "stash@{1}"]);
        let dropped = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(dropped.map(|f| f & REFS), Ok(REFS));
    }

    #[test]
    fn git_paths() {
        assert_eq!(classify_git(Path::new("index")), INDEX);
        assert_eq!(classify_git(Path::new("index.lock")), 0);
        assert_eq!(classify_git(Path::new("refs/heads/main")), REFS | INDEX);
        assert_eq!(classify_git(Path::new("refs/heads/main.lock")), 0);
        assert_eq!(classify_git(Path::new("objects/ab/cdef")), 0);
        assert_eq!(
            classify_git(Path::new("rebase-merge/msgnum")),
            STATE | REFS | INDEX
        );
        assert_eq!(classify_git(Path::new("logs/HEAD")), 0);
        assert_eq!(classify_git(Path::new("logs/refs/heads/main")), 0);
        assert_eq!(classify_git(Path::new("logs/refs/stash")), REFS);
        assert_eq!(classify_git(Path::new("logs/refs/stash.lock")), 0);
        assert_eq!(classify_git(Path::new("config")), REFS | WORKTREE);
        assert_eq!(classify_git(Path::new("config.worktree")), REFS | WORKTREE);
        assert_eq!(classify_git(Path::new("info/exclude")), WORKTREE);
        assert_eq!(classify_git(Path::new("info/attributes")), 0);
        assert_eq!(
            classify_git(Path::new("reftable/tables.list")),
            REFS | INDEX
        );
        assert_eq!(classify_git(Path::new("reftable/tables.list.lock")), 0);
        assert_eq!(
            classify_git(Path::new("reftable/0x01-0x01-1a2b3c4d.ref")),
            0
        );
    }

    #[test]
    fn notices_changes_to_info_exclude() {
        let (dir, _handle, rx, _rt) = watched();
        let excluded = write(&rx, &dir.path().join(".git/info/exclude"));
        assert_eq!(excluded.map(|f| f & WORKTREE), Some(WORKTREE));
    }

    #[test]
    fn follows_folders_that_stop_or_start_being_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &["init", "-q"]));
        std::fs::write(dir.path().join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir(dir.path().join("gen")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        // Git only lists folders that hold something.
        std::fs::write(dir.path().join("gen/a"), "x").unwrap();
        std::fs::write(dir.path().join("src/a"), "x").unwrap();
        let (dir, handle, rx, _rt) = watch_repo(dir);
        assert_eq!(write(&rx, &dir.path().join("gen/a")), None);
        std::fs::write(dir.path().join(".gitignore"), "src/\n").unwrap();
        let edited = rx.recv_timeout(Duration::from_secs(2));
        assert_eq!(edited.map(|f| f & WORKTREE), Ok(WORKTREE));
        // The app loads the status after that.
        tell(dir.path(), &handle);
        let generated = write(&rx, &dir.path().join("gen/a"));
        assert_eq!(generated.map(|f| f & WORKTREE), Some(WORKTREE));
        assert_eq!(write(&rx, &dir.path().join("src/a")), None);
    }

    #[test]
    fn ignores_ignored_files_next_to_tracked_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &["init", "-q"]));
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("debug.log"), "x").unwrap();
        let (dir, _handle, rx, _rt) = watch_repo(dir);
        assert_eq!(write(&rx, &dir.path().join("debug.log")), None);
        let source = write(&rx, &dir.path().join("a.rs"));
        assert_eq!(source.map(|f| f & WORKTREE), Some(WORKTREE));
    }

    #[test]
    fn watches_tracked_files_in_ignored_folders() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &["init", "-q"]));
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("build/x")).unwrap();
        std::fs::write(dir.path().join("build/x/keep"), "x").unwrap();
        assert!(git(dir.path(), &["add", "-f", "build/x/keep"]));
        let (dir, _handle, rx, _rt) = watch_repo(dir);
        let kept = write(&rx, &dir.path().join("build/x/keep"));
        assert_eq!(kept.map(|f| f & WORKTREE), Some(WORKTREE));
    }

    #[test]
    fn drops_a_build_folder_once_git_says_it_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &["init", "-q"]));
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let (dir, handle, rx, _rt) = watch_repo(dir);
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        // We don't know yet that it's ignored.
        assert!(write(&rx, &target.join("a")).is_some());
        tell(dir.path(), &handle);
        assert_eq!(write(&rx, &target.join("b")), None);
        // A clean build deletes the folder and makes it again.
        std::fs::remove_dir_all(&target).unwrap();
        tell(dir.path(), &handle);
        settle(&rx);
        std::fs::create_dir(&target).unwrap();
        assert_eq!(write(&rx, &target.join("c")), None);
    }

    #[test]
    fn remembers_ignored_folders_that_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("out")).unwrap();
        let paths = |ps: &[&[u8]]| ps.iter().map(|&p| BString::from(p)).collect::<Vec<_>>();
        let first = Ignored::new(
            root,
            &paths(&[b"target/", b"out/", b"a.log", b"caf\xe9/"]),
            &Ignored::default(),
        );
        assert!(first.covers(root, &root.join("target/debug/x")));
        // A folder whose name isn't UTF-8 is still the folder git meant,
        // and not the one with a similar name that is.
        assert!(first.covers(root, &root.join(fs_path(b"caf\xe9/x"))));
        assert!(!first.covers(root, &root.join("caf\u{e9}/x")));
        assert!(first.covers(root, &root.join("a.log")));
        assert!(!first.covers(root, &root.join("src/a.rs")));
        // `target` doesn't exist, so it stays. `out` and `a.log` exist and
        // aren't ignored any more.
        let second = Ignored::new(root, &[], &first);
        assert!(second.covers(root, &root.join("target/debug/x")));
        assert!(!second.covers(root, &root.join("out/x")));
        assert!(!second.covers(root, &root.join("a.log")));
    }

    #[test]
    fn worktree_paths() {
        let roots = Roots {
            workdir: PathBuf::from("/r"),
            git_dir: PathBuf::from("/r/.git"),
            common_dir: PathBuf::from("/r/.git"),
        };
        let ignored = Ignored {
            dirs: HashSet::from([PathBuf::from("/r/target")]),
            files: HashSet::from([PathBuf::from("/r/src/a.log")]),
        };
        assert_eq!(
            classify(&roots, Path::new("/r/src/a.rs"), &ignored),
            WORKTREE
        );
        assert_eq!(classify(&roots, Path::new("/r/src/a.log"), &ignored), 0);
        assert_eq!(classify(&roots, Path::new("/r/target/x"), &ignored), 0);
        assert_eq!(classify(&roots, Path::new("/r/target"), &ignored), 0);
        assert_eq!(
            classify(&roots, Path::new("/r/.git/index"), &ignored),
            INDEX
        );
        assert_eq!(classify(&roots, Path::new("/elsewhere"), &ignored), 0);
    }
}
