//! Watching the working tree and the git directory for changes.
//!
//! On Linux, inotify watches single directories, so we walk the working tree
//! and watch each directory that git doesn't ignore. Build output folders
//! like `target/` or `node_modules/` are never watched, which keeps us well
//! under the inotify limit and avoids refresh storms during builds. New
//! directories get watches as they appear.
//!
//! On macOS, FSEvents watches a whole tree cheaply, so we watch the root and
//! drop events from ignored directories.
//!
//! Events are collected for a short while and then reported as a set of
//! flags saying what needs to be reloaded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{RecvTimeoutError, unbounded};
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

pub fn start(
    workdir: &Path,
    git_dir: &Path,
    common_dir: &Path,
    report: impl Fn(Event) + Send + 'static,
) -> Result<Handle> {
    let roots = Roots {
        workdir: workdir.canonicalize().context("bad workdir")?,
        git_dir: git_dir.canonicalize().context("bad git dir")?,
        common_dir: common_dir.canonicalize().context("bad common dir")?,
    };
    let (tx, rx) = unbounded::<notify::Result<notify::Event>>();
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
        }
    }

    let ignored_dirs = if cfg!(target_os = "linux") {
        Vec::new()
    } else {
        ignored_directories(&roots.workdir)
    };

    let degraded = if cfg!(target_os = "linux") {
        watch_tree(&watcher, &roots.workdir, &roots.workdir).err()
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

    let w2 = watcher.clone();
    std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || {
            let mut pending: u8 = 0;
            let mut first: Option<Instant> = None;
            let mut new_dirs: HashSet<PathBuf> = HashSet::new();
            loop {
                let timeout = match first {
                    Some(t) => QUIET.min(MAX_DELAY.saturating_sub(t.elapsed())),
                    None => Duration::from_secs(3600),
                };
                match rx.recv_timeout(timeout) {
                    Ok(Ok(ev)) => {
                        if matches!(ev.kind, EventKind::Access(_)) {
                            continue;
                        }
                        // A directory can also appear by being moved in.
                        let appeared = matches!(
                            ev.kind,
                            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
                        );
                        for p in &ev.paths {
                            let flags = classify(&roots, p, &ignored_dirs);
                            if flags == 0 {
                                continue;
                            }
                            if cfg!(target_os = "linux") && flags == WORKTREE && appeared && p.is_dir() {
                                new_dirs.insert(p.clone());
                            }
                            pending |= flags;
                            first.get_or_insert_with(Instant::now);
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        for d in new_dirs.drain() {
                            if !is_ignored(&roots.workdir, &d) {
                                let _ = watch_tree(&w2, &roots.workdir, &d);
                            }
                        }
                        if pending != 0 {
                            report(Event::Changed(pending));
                        }
                        pending = 0;
                        first = None;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .context("could not start watcher thread")?;

    Ok(Handle { _watcher: watcher })
}

/// Watch `start` and every directory under it that git doesn't ignore.
fn watch_tree(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    workdir: &Path,
    start: &Path,
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
        "rebase-merge" | "rebase-apply" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        | "sequencer" => STATE | REFS | INDEX,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Watch a new scratch repository and collect the flags it reports.
    fn watched() -> (tempfile::TempDir, Handle, crossbeam_channel::Receiver<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(init.success());
        let (tx, rx) = unbounded();
        let git_dir = dir.path().join(".git");
        let handle = start(dir.path(), &git_dir, &git_dir, move |e| {
            if let Event::Changed(flags) = e {
                let _ = tx.send(flags);
            }
        })
        .unwrap();
        (dir, handle, rx)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watches_a_directory_that_is_moved_in() {
        let (dir, _handle, rx) = watched();
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
    fn git_paths() {
        assert_eq!(classify_git(Path::new("index")), INDEX);
        assert_eq!(classify_git(Path::new("index.lock")), 0);
        assert_eq!(classify_git(Path::new("refs/heads/main")), REFS | INDEX);
        assert_eq!(classify_git(Path::new("refs/heads/main.lock")), 0);
        assert_eq!(classify_git(Path::new("objects/ab/cdef")), 0);
        assert_eq!(classify_git(Path::new("rebase-merge/msgnum")), STATE | REFS | INDEX);
        assert_eq!(classify_git(Path::new("logs/HEAD")), 0);
        assert_eq!(classify_git(Path::new("config")), REFS);
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
