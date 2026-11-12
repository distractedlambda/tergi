//! Watching the working tree and the git directory for changes.
//!
//! On Linux, inotify watches single directories, so we walk the working tree
//! and watch each directory that git doesn't ignore. Build output folders
//! like `target/` or `node_modules/` are never watched, which keeps us well
//! under the inotify limit and avoids refresh storms during builds. New
//! directories get watches as they appear.
//!
//! On macOS, FSEvents watches a whole tree cheaply, so we watch the root and
//! drop events from ignored directories. Before reporting a change, we ask
//! git whether the changed paths are ignored, because the list of ignored
//! directories can go out of date.
//!
//! On both, we also watch `info/exclude` and the global excludes file,
//! since changing them changes which untracked files git shows. When the
//! ignore rules change, we walk the tree again on Linux to watch the right
//! directories, and rebuild the list of ignored directories on macOS. We
//! also watch the user's global git config, which can name a different
//! excludes file.
//!
//! Events are collected for a short while and then reported as a set of
//! flags saying what needs to be reloaded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc::unbounded_channel;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

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

    {
        let mut w = watcher.lock().map_err(|_| anyhow::anyhow!("watcher lock"))?;
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
    }
    let mut watches = Watches {
        reflogs: watch_reflogs(&watcher, &roots.common_dir),
        ..Watches::default()
    };
    watches.configs.set(global_config_files());
    watches.configs.watch_missing(&watcher);
    watches.rules.set(rules_files(&roots.workdir, &roots.common_dir));
    watches.rules.watch_missing(&watcher);
    let config = roots.common_dir.join("config");

    let mut ignored_dirs = if cfg!(target_os = "linux") {
        Vec::new()
    } else {
        ignored_directories(&roots.workdir)
    };

    let degraded = if cfg!(target_os = "linux") {
        watch_tree(&watcher, &roots.workdir, &roots.workdir, &mut watches.tree).err()
    } else {
        watcher
            .lock()
            .ok()
            .and_then(|mut w| w.watch(&roots.workdir, RecursiveMode::Recursive).err())
            .map(|e| e.to_string())
    };
    if let Some(msg) = degraded {
        report(Event::Degraded(msg));
    }

    // Hold the watcher weakly, so dropping the handle stops notify and ends
    // this future.
    let w2 = Arc::downgrade(&watcher);
    let run = async move {
        let mut pending: u8 = 0;
        let mut first: Option<Instant> = None;
        let mut new_dirs: HashSet<PathBuf> = HashSet::new();
        let mut changed: HashSet<PathBuf> = HashSet::new();
        let mut rules_changed = false;
        let mut config_changed = false;
        loop {
            let timeout = match first {
                Some(t) => QUIET.min(MAX_DELAY.saturating_sub(t.elapsed())),
                None => Duration::from_secs(3600),
            };
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(Ok(ev))) => {
                    if matches!(ev.kind, EventKind::Access(_)) {
                        continue;
                    }
                    // A directory can also appear by being moved in.
                    let appeared = matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
                    );
                    for p in &ev.paths {
                        if watches.rules.contains(p) {
                            rules_changed = true;
                            first.get_or_insert_with(Instant::now);
                            continue;
                        }
                        // The config can name a different global excludes
                        // file.
                        if watches.configs.contains(p) {
                            config_changed = true;
                            first.get_or_insert_with(Instant::now);
                            continue;
                        }
                        config_changed |= *p == config;
                        let flags = classify(&roots, p, &ignored_dirs);
                        if flags == 0 {
                            continue;
                        }
                        if flags == WORKTREE && p.file_name().is_some_and(|n| n == ".gitignore") {
                            rules_changed = true;
                        }
                        if flags == WORKTREE && !cfg!(target_os = "linux") {
                            // Whether this counts depends on whether git
                            // ignores it, which we check in one go below.
                            changed.insert(p.clone());
                        } else {
                            if cfg!(target_os = "linux")
                                && flags == WORKTREE
                                && appeared
                                && p.is_dir()
                            {
                                new_dirs.insert(p.clone());
                            }
                            pending |= flags;
                        }
                        first.get_or_insert_with(Instant::now);
                    }
                }
                Ok(Some(Err(_))) => {}
                Err(_) => {
                    // Adding watches walks directories and runs git, so do
                    // it off the async threads.
                    let Some(w) = w2.upgrade() else { break };
                    let common_dir = roots.common_dir.clone();
                    let workdir = roots.workdir.clone();
                    let dirs: Vec<PathBuf> = new_dirs.drain().collect();
                    let paths: Vec<PathBuf> = changed.drain().collect();
                    // A fresh repository has no reflogs until its first
                    // commit, which also changes a ref.
                    let check_reflogs = !watches.reflogs && pending & REFS != 0;
                    let mut ws = std::mem::take(&mut watches);
                    let mut reread_rules = std::mem::take(&mut config_changed);
                    let mut rules_now = std::mem::take(&mut rules_changed);
                    let (ws, rules_now, (worktree_changed, new_ignored)) =
                        crate::util::blocking(move || {
                            if check_reflogs {
                                ws.reflogs = watch_reflogs(&w, &common_dir);
                            }
                            // A config file that just appeared may name a
                            // global excludes file.
                            reread_rules |= ws.configs.watch_missing(&w);
                            if reread_rules {
                                rules_now |= ws.rules.set(rules_files(&workdir, &common_dir));
                            }
                            rules_now |= ws.rules.watch_missing(&w);
                            if rules_now && cfg!(target_os = "linux") {
                                // This also finds the new folders.
                                ws.rewatch_tree(&w, &workdir);
                            } else {
                                for d in dirs {
                                    if !is_ignored(&workdir, &d) {
                                        let _ = watch_tree(&w, &workdir, &d, &mut ws.tree);
                                    }
                                }
                            }
                            let checked = if cfg!(target_os = "linux") {
                                (false, None)
                            } else {
                                check_changes(&workdir, &paths, rules_now)
                            };
                            (ws, rules_now, checked)
                        })
                        .await;
                    watches = ws;
                    // Different rules can change which untracked files we
                    // show.
                    if worktree_changed || rules_now {
                        pending |= WORKTREE;
                    }
                    if let Some(dirs) = new_ignored {
                        ignored_dirs = dirs;
                    }
                    if pending != 0 {
                        report(Event::Changed(pending));
                    }
                    pending = 0;
                    first = None;
                }
                Ok(None) => break,
            }
        }
    };

    Ok((Handle { _watcher: watcher }, run))
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

/// Watch `start` and every directory under it that git doesn't ignore, and
/// add them to `watched`.
fn watch_tree(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    workdir: &Path,
    start: &Path,
    watched: &mut HashSet<PathBuf>,
) -> std::result::Result<(), String> {
    let walk = ignore::WalkBuilder::new(start)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    let mut w = watcher.lock().map_err(|_| "watcher lock poisoned".to_string())?;
    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let p = entry.path();
        if p != workdir && p.join(".git").exists() {
            // A nested repository or submodule. Its changes don't show up in
            // our status anyway, apart from the commit it points at.
            continue;
        }
        if let Err(e) = w.watch(p, RecursiveMode::NonRecursive) {
            return Err(format!(
                "Can't watch every folder ({e}). The working directory will be checked every few seconds instead."
            ));
        }
        watched.insert(p.to_path_buf());
    }
    Ok(())
}

fn is_ignored(workdir: &Path, path: &Path) -> bool {
    std::process::Command::new("git")
        .current_dir(workdir)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Ignored directories, for filtering events on platforms where we watch the
/// whole tree.
fn ignored_directories(workdir: &Path) -> Vec<PathBuf> {
    let Ok(out) = std::process::Command::new("git")
        .current_dir(workdir)
        .args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| s.ends_with('/'))
        .map(|s| workdir.join(s.trim_end_matches('/')))
        .collect()
}

/// What we watch besides the git directory and, on macOS, the working
/// tree. We move this to a blocking thread whenever we change the watches.
#[derive(Default)]
struct Watches {
    /// Whether the folder with the stash reflog is watched.
    reflogs: bool,
    /// Files with ignore rules that git reads besides the .gitignore files in
    /// the working tree.
    rules: WatchedFiles,
    /// The user's global git config files, which can name a different
    /// global excludes file.
    configs: WatchedFiles,
    /// The folders of the working tree that we watch on Linux.
    tree: HashSet<PathBuf>,
}

impl Watches {
    /// Walk the whole working tree again after the ignore rules changed.
    /// Watch the folders that are no longer ignored, and stop watching the
    /// ones that now are.
    fn rewatch_tree(&mut self, watcher: &Arc<Mutex<RecommendedWatcher>>, workdir: &Path) {
        let mut now = HashSet::new();
        if watch_tree(watcher, workdir, workdir, &mut now).is_err() {
            // We didn't get through the walk, so we can't tell which folders
            // are ignored now.
            self.tree.extend(now);
            return;
        }
        // A folder can hold a watched file as well, in unusual setups.
        let keep: HashSet<&Path> = self.rules.targets().chain(self.configs.targets()).collect();
        if let Ok(mut w) = watcher.lock() {
            for dir in self.tree.difference(&now) {
                if !keep.contains(dir.as_path()) {
                    let _ = w.unwatch(dir);
                }
            }
        }
        self.tree = now;
    }
}

/// Single files we watch outside the working tree. For each file we keep
/// its real path, which is what events use, and the path we gave the
/// watcher, once it is watched.
#[derive(Default)]
struct WatchedFiles(Vec<(PathBuf, Option<(PathBuf, PathBuf)>)>);

impl WatchedFiles {
    fn contains(&self, path: &Path) -> bool {
        self.0.iter().any(|(_, w)| w.as_ref().is_some_and(|(real, _)| real == path))
    }

    fn targets(&self) -> impl Iterator<Item = &Path> {
        self.0.iter().filter_map(|(_, w)| w.as_ref().map(|(_, target)| target.as_path()))
    }

    /// Switch to `files`. Returns whether they are different from before.
    /// We don't stop watching the old files, which only costs a few events.
    fn set(&mut self, files: Vec<PathBuf>) -> bool {
        if files.iter().eq(self.0.iter().map(|(f, _)| f)) {
            return false;
        }
        self.0 = files.into_iter().map(|f| (f, None)).collect();
        true
    }

    /// Watch the files that aren't watched yet. Returns whether we started
    /// watching a file, because it may have changed just before.
    fn watch_missing(&mut self, watcher: &Arc<Mutex<RecommendedWatcher>>) -> bool {
        let mut started = false;
        for (file, w) in &mut self.0 {
            if w.is_none() {
                *w = watch_file(watcher, file);
                started |= w.is_some();
            }
        }
        started
    }
}

/// The folder for the user's config files, the way git finds it on every
/// platform, macOS included.
fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

fn global_config_files() -> Vec<PathBuf> {
    if let Some(file) = std::env::var_os("GIT_CONFIG_GLOBAL").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(file)];
    }
    config_home()
        .map(|c| c.join("git/config"))
        .into_iter()
        .chain(std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gitconfig")))
        .collect()
}

/// The repository's `info/exclude` and the user's global excludes file.
fn rules_files(workdir: &Path, common_dir: &Path) -> Vec<PathBuf> {
    let configured = std::process::Command::new("git")
        .current_dir(workdir)
        .args(["config", "--path", "core.excludesFile"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
        .filter(|s| !s.is_empty());
    let global = match configured {
        Some(file) => Some(workdir.join(file)),
        None => config_home().map(|c| c.join("git/ignore")),
    };
    std::iter::once(common_dir.join("info/exclude"))
        .chain(global)
        .collect()
}

/// Watch a single file. Returns its real path and the path we gave the
/// watcher. The file may not exist yet, but its folder must.
fn watch_file(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    file: &Path,
) -> Option<(PathBuf, PathBuf)> {
    let real = match file.canonicalize() {
        Ok(real) => real,
        Err(_) => file.parent()?.canonicalize().ok()?.join(file.file_name()?),
    };
    // Editors often save by replacing the file. On Linux that ends a watch
    // on the file itself, so we watch its folder. On macOS, watching a
    // folder without its subfolders still makes FSEvents watch all of them,
    // which is a lot for a home folder, so we watch the file if it exists.
    let target = if cfg!(target_os = "linux") || !real.exists() {
        real.parent()?.to_path_buf()
    } else {
        real.clone()
    };
    watcher
        .lock()
        .ok()?
        .watch(&target, RecursiveMode::NonRecursive)
        .ok()?;
    Some((real, target))
}

/// Look at the working tree paths that changed in one batch, on platforms
/// where we watch the whole tree. Returns whether any of them is not
/// ignored, and a fresh list of ignored directories if the old one may be
/// out of date. It may be out of date if `rules_changed`, which includes a
/// change to any .gitignore.
fn check_changes(
    workdir: &Path,
    paths: &[PathBuf],
    rules_changed: bool,
) -> (bool, Option<Vec<PathBuf>>) {
    let rel = |p: &Path| p.strip_prefix(workdir).unwrap_or(p).to_path_buf();
    // Ask about the folders above each path too. That way we can tell an
    // ignored file in a folder we care about from a file in an ignored
    // folder that we don't know about yet.
    let mut ask = std::collections::BTreeSet::new();
    for p in paths {
        for a in rel(p).ancestors() {
            // Git rejects an empty path. The working directory itself is
            // never ignored anyway.
            if !a.as_os_str().is_empty() {
                ask.insert(a.to_path_buf());
            }
        }
    }
    let ask: Vec<PathBuf> = ask.into_iter().collect();
    let ignored: HashSet<&Path> = ask
        .iter()
        .zip(check_ignored(workdir, &ask))
        .filter(|(_, ignored)| *ignored)
        .map(|(p, _)| p.as_path())
        .collect();
    let mut any_changed = false;
    // New rules can make a folder ignored or stop it being ignored.
    let mut stale = rules_changed;
    for p in paths {
        let r = rel(p);
        if !ignored.contains(r.as_path()) {
            any_changed = true;
        } else if p.is_dir() || r.ancestors().skip(1).any(|a| ignored.contains(a)) {
            // An ignored folder appeared, or an existing folder became
            // ignored.
            stale = true;
        }
    }
    (any_changed, stale.then(|| ignored_directories(workdir)))
}

/// Ask git which of `paths` it ignores. The paths are relative to the
/// working directory. If git fails, we treat none of them as ignored.
fn check_ignored(workdir: &Path, paths: &[PathBuf]) -> Vec<bool> {
    use std::io::Write as _;
    let mut result = vec![false; paths.len()];
    if paths.is_empty() {
        return result;
    }
    let mut index = std::collections::HashMap::new();
    let mut input = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let bytes = p.as_os_str().as_encoded_bytes();
        index.insert(bytes, i);
        input.extend_from_slice(bytes);
        input.push(0);
    }
    let Ok(mut child) = std::process::Command::new("git")
        .current_dir(workdir)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return result;
    };
    // Git answers each path as soon as it reads it. If we wrote everything
    // before reading, a long list would fill both pipes and we would wait
    // on each other forever.
    let stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&input);
        }
    });
    let out = child.wait_with_output();
    let _ = writer.join();
    // Git exits with 1 when nothing is ignored. It prints the ignored paths
    // just as we gave them.
    if let Ok(out) = out
        && matches!(out.status.code(), Some(0 | 1))
    {
        for p in out.stdout.split(|&b| b == 0) {
            if let Some(&i) = index.get(p) {
                result[i] = true;
            }
        }
    }
    result
}

fn classify(roots: &Roots, path: &Path, ignored_dirs: &[PathBuf]) -> u8 {
    for dir in [&roots.git_dir, &roots.common_dir] {
        if let Ok(rel) = path.strip_prefix(dir) {
            return classify_git(rel);
        }
    }
    if path.strip_prefix(&roots.workdir).is_ok() {
        if path.components().any(|c| c.as_os_str() == ".git") {
            return 0;
        }
        if ignored_dirs.iter().any(|d| path.starts_with(d)) {
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
        // Remotes and upstreams live in the config.
        "config" => REFS,
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

    type Watched = (tempfile::TempDir, Handle, Receiver<u8>, tokio::runtime::Runtime);

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

    /// Watch an existing scratch repository.
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
        (dir, handle, rx, rt)
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
        assert_eq!(classify_git(Path::new("rebase-merge/msgnum")), STATE | REFS | INDEX);
        assert_eq!(classify_git(Path::new("logs/HEAD")), 0);
        assert_eq!(classify_git(Path::new("logs/refs/heads/main")), 0);
        assert_eq!(classify_git(Path::new("logs/refs/stash")), REFS);
        assert_eq!(classify_git(Path::new("logs/refs/stash.lock")), 0);
        assert_eq!(classify_git(Path::new("config")), REFS);
        assert_eq!(classify_git(Path::new("reftable/tables.list")), REFS | INDEX);
        assert_eq!(classify_git(Path::new("reftable/tables.list.lock")), 0);
        assert_eq!(classify_git(Path::new("reftable/0x01-0x01-1a2b3c4d.ref")), 0);
    }

    #[test]
    fn notices_changes_to_ignore_rules_outside_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let global = elsewhere.path().join("ignore");
        assert!(git(dir.path(), &["init", "-q"]));
        assert!(git(
            dir.path(),
            &["config", "core.excludesFile", global.to_str().unwrap()]
        ));
        let (dir, _handle, rx, _rt) = watch_repo(dir);
        settle(&rx);
        std::fs::write(dir.path().join(".git/info/exclude"), "*.o\n").unwrap();
        let excluded = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(excluded.map(|f| f & WORKTREE), Ok(WORKTREE));
        settle(&rx);
        // Replace the file the way many editors save.
        let tmp = elsewhere.path().join("ignore.tmp");
        std::fs::write(&tmp, "*.a\n").unwrap();
        std::fs::rename(&tmp, &global).unwrap();
        let replaced = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(replaced.map(|f| f & WORKTREE), Ok(WORKTREE));
    }

    #[test]
    fn follows_folders_that_stop_or_start_being_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &["init", "-q"]));
        std::fs::write(dir.path().join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir(dir.path().join("gen")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let (dir, _handle, rx, _rt) = watch_repo(dir);
        settle(&rx);
        std::fs::write(dir.path().join(".gitignore"), "src/\n").unwrap();
        let edited = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(edited.map(|f| f & WORKTREE), Ok(WORKTREE));
        settle(&rx);
        std::fs::write(dir.path().join("gen/a"), "x").unwrap();
        let generated = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(generated.map(|f| f & WORKTREE), Ok(WORKTREE));
        settle(&rx);
        std::fs::write(dir.path().join("src/a"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }

    /// Run `follows_the_global_config_in_this_process` in its own process,
    /// because it needs its own global git config, and tests can't safely
    /// change the environment of a process that runs other tests.
    #[test]
    fn follows_the_global_config() {
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "watch::tests::follows_the_global_config_in_this_process",
                "--ignored",
            ])
            .env("GIT_CONFIG_GLOBAL", home.path().join("gitconfig"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "follows_the_global_config runs this"]
    fn follows_the_global_config_in_this_process() {
        let Some(config) = std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from) else {
            eprintln!("skipped: GIT_CONFIG_GLOBAL isn't set");
            return;
        };
        let (_dir, _handle, rx, _rt) = watched();
        settle(&rx);
        let excludes = config.with_file_name("ignore");
        let set = format!("[core]\n\texcludesFile = {}\n", excludes.display());
        std::fs::write(&config, set).unwrap();
        // Rules from a different file are different rules.
        let configured = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(configured.map(|f| f & WORKTREE), Ok(WORKTREE));
        settle(&rx);
        std::fs::write(&excludes, "*.o\n").unwrap();
        let excluded = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(excluded.map(|f| f & WORKTREE), Ok(WORKTREE));
    }

    #[test]
    fn checks_changes_against_current_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(git(root, &["init", "-q"]));
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        let src = root.join("src/a.rs");
        let built = root.join("target/debug/a.o");
        let check = |paths: &[&PathBuf], rules_changed| {
            let paths: Vec<PathBuf> = paths.iter().map(|&p| p.clone()).collect();
            check_changes(root, &paths, rules_changed)
        };
        assert_eq!(check(&[&src], false), (true, None));
        // An ignored file in a folder we care about doesn't make the list
        // of ignored folders stale.
        std::fs::create_dir(root.join("src")).unwrap();
        assert_eq!(check(&[&root.join("src/debug.log")], false), (false, None));
        // The build folder appears after the watcher started.
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        let target = vec![root.join("target")];
        assert_eq!(check(&[&built], false), (false, Some(target.clone())));
        assert_eq!(check(&[&root.join("target")], false), (false, Some(target.clone())));
        assert_eq!(check(&[&src, &built], false), (true, Some(target.clone())));
        // Stop ignoring it.
        std::fs::write(root.join(".gitignore"), "").unwrap();
        assert_eq!(check(&[&root.join(".gitignore")], true), (true, Some(vec![])));
        assert_eq!(check(&[&built], false), (true, None));
        assert_eq!(check(&[&root.to_path_buf()], false), (true, None));
        // Ignore it again from somewhere else.
        std::fs::write(root.join(".git/info/exclude"), "target/\n").unwrap();
        assert_eq!(check(&[], true), (false, Some(target)));
    }

    #[test]
    fn checks_many_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(git(root, &["init", "-q"]));
        std::fs::write(root.join(".gitignore"), "*.o\n").unwrap();
        // Enough output to fill a pipe.
        let paths: Vec<PathBuf> = (0..20_000)
            .map(|i| PathBuf::from(format!("some/fairly/long/directory/name/file{i}.o")))
            .chain([PathBuf::from("src/a.rs")])
            .collect();
        let ignored = check_ignored(root, &paths);
        assert_eq!(ignored.iter().filter(|&&i| i).count(), 20_000);
        assert_eq!(ignored.last(), Some(&false));
    }

    #[test]
    fn worktree_paths() {
        let roots = Roots {
            workdir: PathBuf::from("/r"),
            git_dir: PathBuf::from("/r/.git"),
            common_dir: PathBuf::from("/r/.git"),
        };
        let ignored = vec![PathBuf::from("/r/target")];
        assert_eq!(classify(&roots, Path::new("/r/src/a.rs"), &ignored), WORKTREE);
        assert_eq!(classify(&roots, Path::new("/r/target/x"), &ignored), 0);
        assert_eq!(classify(&roots, Path::new("/r/.git/index"), &ignored), INDEX);
        assert_eq!(classify(&roots, Path::new("/elsewhere"), &ignored), 0);
    }
}
