//! Property tests for the file watcher, run against the real git.
//!
//! Each case makes a scratch repository, starts the watcher on it, and runs
//! a few bursts of random changes. A burst writes, deletes and renames files
//! and folders a few levels deep, moves folders in from outside and out
//! again, edits `.gitignore` files and `info/exclude`, and runs git commands
//! such as commit, checkout, stash and merge. Some cases use the reftable
//! format, and some watch a linked worktree while git commands also run in
//! the main one.
//!
//! Before and after each burst, the test asks git what the app would show.
//! For the working directory, that is the status, the index and the content
//! of every file that isn't ignored. For the refs, it is every ref, `HEAD`
//! and the stash list. When the working directory looks different after a
//! burst, the watcher must have reported `WORKTREE` or `INDEX`. When the
//! refs look different, it must have reported `REFS`. Like the app, the
//! test then loads the status and sends it to the watcher.
//!
//! A report for one change in a burst can hide a change that the watcher
//! missed. So at the end, the test changes something in each folder where
//! git would show the change, one folder at a time, and checks that the
//! watcher reports each one.
//!
//! Waiting for the watcher takes time, so this runs few cases, and runs
//! them in several tests at once.

mod common;

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use common::settings::{Settings, settings};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use tergi::git::status::{self, Status};
use tergi::watch::{self, Delays, Event, INDEX, REFS, WORKTREE};

/// Folders that the changes happen in. Some of them are ignored by some of
/// the rules in `RULES`.
const DIRS: &[&str] = &["a", "a/b", "a/b/c", "d", "gen", "gen/x", "a/gen"];

/// File names. A file goes in the root or in one of `DIRS`.
const FILES: &[&str] = &["f", "g.log"];

const CONTENTS: &[&str] = &["1", "2", "1\n2\n"];

/// Lines for `.gitignore` files and `info/exclude`.
const RULES: &[&str] = &["gen/", "*.log", "b/", "/d/", "c", "!a/gen/"];

const BRANCHES: &[&str] = &["main", "topic", "other"];

/// A file or folder in the working tree. With neither part, it's the root.
#[derive(Debug, Clone)]
struct Place {
    dir: Option<Index>,
    file: Option<Index>,
}

impl Place {
    fn rel(&self) -> PathBuf {
        let mut p = PathBuf::new();
        if let Some(d) = &self.dir {
            p.push(d.get(DIRS));
        }
        if let Some(f) = &self.file {
            p.push(f.get(FILES));
        }
        p
    }
}

#[derive(Debug, Clone)]
enum IgnoreFile {
    Root,
    /// `a/.gitignore`.
    Sub,
    Exclude,
}

#[derive(Debug, Clone)]
enum GitOp {
    AddAll,
    Commit,
    CommitAll,
    Branch(Index),
    DeleteBranch(Index),
    Checkout(Index),
    Merge(Index),
    Stash,
    StashUntracked,
    StashPop,
    StashDrop(u8),
    ResetHard,
    ResetSoft,
    /// `git clean -fdx`, which deletes ignored files too.
    Clean,
    Tag,
}

impl GitOp {
    fn args(&self) -> Vec<String> {
        let words: &[&str] = match self {
            GitOp::AddAll => &["add", "-A"],
            GitOp::Commit => &["commit", "-q", "--allow-empty", "-m", "m"],
            GitOp::CommitAll => &["commit", "-q", "-a", "--allow-empty", "-m", "m"],
            GitOp::Branch(b) => return vec!["branch".into(), b.get(BRANCHES).to_string()],
            GitOp::DeleteBranch(b) => {
                return vec!["branch".into(), "-qD".into(), b.get(BRANCHES).to_string()];
            }
            GitOp::Checkout(b) => {
                return vec!["checkout".into(), "-q".into(), b.get(BRANCHES).to_string()];
            }
            GitOp::Merge(b) => {
                return vec![
                    "merge".into(),
                    "-q".into(),
                    "--no-edit".into(),
                    b.get(BRANCHES).to_string(),
                ];
            }
            GitOp::Stash => &["stash", "-q"],
            GitOp::StashUntracked => &["stash", "-q", "-u"],
            GitOp::StashPop => &["stash", "pop", "-q"],
            GitOp::StashDrop(n) => {
                return vec![
                    "stash".into(),
                    "drop".into(),
                    "-q".into(),
                    format!("stash@{{{n}}}"),
                ];
            }
            GitOp::ResetHard => &["reset", "-q", "--hard"],
            GitOp::ResetSoft => &["reset", "-q", "--soft", "HEAD~"],
            GitOp::Clean => &["clean", "-q", "-f", "-d", "-x"],
            GitOp::Tag => &["tag", "-f", "t"],
        };
        words.iter().map(|w| w.to_string()).collect()
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// Write a file, making its folder first if it's missing.
    Write {
        dir: Option<Index>,
        file: Index,
        content: Index,
    },
    Mkdir(Index),
    /// Delete a file, or a folder with everything in it.
    Remove(Place),
    /// Rename a file or folder, making the folder it goes to first.
    Rename {
        from: Place,
        to: Place,
    },
    /// Make a folder with files outside the repository, maybe with
    /// subfolders, and move it in.
    MoveIn {
        to: Index,
        deep: bool,
    },
    /// Move a folder out of the repository.
    MoveOut(Index),
    /// Rewrite an ignore file with the rules from `RULES` that are picked.
    Ignore(IgnoreFile, Vec<bool>),
    /// Run git. In a case with a linked worktree, `main` runs it in the
    /// main worktree instead of the watched one.
    Git {
        main: bool,
        op: GitOp,
    },
    /// Load the status and send it to the watcher, as the app does when it
    /// refreshes for some other reason.
    Refresh,
    /// Wait this many times 20 ms, so that some changes fall into separate
    /// batches of the watcher.
    Pause(u8),
}

#[derive(Debug, Clone)]
struct Case {
    /// Changes made before the watcher starts. They are committed.
    setup: Vec<Op>,
    bursts: Vec<Vec<Op>>,
    reftable: bool,
    worktree: bool,
    /// The watcher's quiet delay in milliseconds. Its longest delay is five
    /// times that, as in the app.
    quiet: u64,
    settings: Settings,
}

fn place() -> impl Strategy<Value = Place> {
    (
        prop::option::of(any::<Index>()),
        prop::option::of(any::<Index>()),
    )
        .prop_map(|(dir, file)| Place { dir, file })
}

fn git_op() -> impl Strategy<Value = GitOp> {
    prop_oneof![
        Just(GitOp::AddAll),
        Just(GitOp::Commit),
        Just(GitOp::CommitAll),
        any::<Index>().prop_map(GitOp::Branch),
        any::<Index>().prop_map(GitOp::DeleteBranch),
        any::<Index>().prop_map(GitOp::Checkout),
        any::<Index>().prop_map(GitOp::Merge),
        Just(GitOp::Stash),
        Just(GitOp::StashUntracked),
        Just(GitOp::StashPop),
        (0..3u8).prop_map(GitOp::StashDrop),
        Just(GitOp::ResetHard),
        Just(GitOp::ResetSoft),
        Just(GitOp::Clean),
        Just(GitOp::Tag),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let ignore_file = prop_oneof![
        Just(IgnoreFile::Root),
        Just(IgnoreFile::Sub),
        Just(IgnoreFile::Exclude),
    ];
    prop_oneof![
        6 => (prop::option::of(any::<Index>()), any::<Index>(), any::<Index>())
            .prop_map(|(dir, file, content)| Op::Write { dir, file, content }),
        1 => any::<Index>().prop_map(Op::Mkdir),
        2 => place().prop_map(Op::Remove),
        3 => (place(), place()).prop_map(|(from, to)| Op::Rename { from, to }),
        2 => (any::<Index>(), any::<bool>()).prop_map(|(to, deep)| Op::MoveIn { to, deep }),
        1 => any::<Index>().prop_map(Op::MoveOut),
        2 => (ignore_file, prop::collection::vec(prop::bool::weighted(0.3), RULES.len()))
            .prop_map(|(file, rules)| Op::Ignore(file, rules)),
        3 => (any::<bool>(), git_op()).prop_map(|(main, op)| Op::Git { main, op }),
        1 => Just(Op::Refresh),
        1 => (0..10u8).prop_map(Op::Pause),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(op(), 0..8),
        prop::collection::vec(prop::collection::vec(op(), 1..6), 1..6),
        any::<bool>(),
        prop::bool::weighted(0.3),
        // Mostly shorter than the app's 120 ms, which makes the test faster
        // and splits bursts into more batches.
        prop_oneof![3 => Just(10u64), 3 => Just(30), 1 => Just(120)],
        settings(),
    )
        .prop_map(
            |(setup, bursts, reftable, worktree, quiet, settings)| Case {
                setup,
                bursts,
                reftable,
                worktree,
                quiet,
                settings,
            },
        )
}

/// The scratch repository, and the worktree that the watcher watches.
struct Repo {
    dir: tempfile::TempDir,
    /// The main worktree.
    main: PathBuf,
    /// The watched worktree. It's `main` unless the case has a linked
    /// worktree.
    root: PathBuf,
}

/// Run git in `dir` and return its output, or `None` if it failed.
fn git(dir: &Path, args: &[impl AsRef<std::ffi::OsStr>]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_EDITOR", "true")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .expect("run git");
    out.status.success().then_some(out.stdout)
}

/// Run git in `dir` and panic if it fails.
fn must(dir: &Path, args: &[&str]) -> Vec<u8> {
    git(dir, args).unwrap_or_else(|| panic!("git {args:?} failed"))
}

impl Repo {
    fn new(case: &Case) -> Repo {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        std::fs::create_dir(&main).expect("mkdir");
        // Gits older than 2.45 can't make reftable repositories, and the
        // case falls back to the files format.
        let reftable =
            case.reftable && git(&main, &["init", "-q", "--ref-format=reftable"]).is_some();
        if !reftable {
            must(&main, &["init", "-q"]);
        }
        must(&main, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        for (key, value) in [
            ("user.name", "Test User"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
        ]
        .into_iter()
        .chain(case.settings.0.iter().copied())
        {
            must(&main, &["config", key, value]);
        }
        let mut repo = Repo {
            root: main.clone(),
            main,
            dir,
        };
        for (rel, content) in [
            ("f", "1"),
            ("a/f", "1"),
            ("a/b/c/f", "1"),
            ("d/f", "1"),
            ("gen/x/f", "1"),
            ("a/g.log", "1"),
            (".gitignore", "gen/\n*.log\n"),
        ] {
            let p = repo.main.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, content).expect("write");
        }
        for op in &case.setup {
            repo.apply(op);
        }
        must(&repo.main, &["add", "-A"]);
        must(
            &repo.main,
            &["commit", "-q", "--allow-empty", "-m", "start"],
        );
        if case.worktree {
            must(&repo.main, &["worktree", "add", "-q", "../wt"]);
            repo.root = repo.dir.path().join("wt");
        }
        repo
    }

    /// A folder next to the repositories, on the same file system, for
    /// moving folders in and out.
    fn outside(&self) -> PathBuf {
        let p = self.dir.path().join("outside");
        let _ = std::fs::create_dir(&p);
        p
    }

    fn git_dirs(&self) -> (PathBuf, PathBuf) {
        let out = must(
            &self.root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        );
        let out = String::from_utf8(out).expect("utf-8");
        let mut lines = out.lines().map(PathBuf::from);
        let git_dir = lines.next().expect("git dir");
        let common_dir = lines.next().expect("common dir");
        (git_dir, common_dir)
    }

    /// Make a change. Changes that don't make sense in the current state,
    /// like writing a file where a folder is, just fail.
    fn apply(&self, op: &Op) {
        let root = &self.root;
        match op {
            Op::Write { dir, file, content } => {
                let mut p = root.clone();
                if let Some(d) = dir {
                    p.push(d.get(DIRS));
                }
                let _ = std::fs::create_dir_all(&p);
                let _ = std::fs::write(p.join(file.get(FILES)), content.get(CONTENTS));
            }
            Op::Mkdir(d) => {
                let _ = std::fs::create_dir_all(root.join(d.get(DIRS)));
            }
            Op::Remove(place) => {
                let rel = place.rel();
                if rel.as_os_str().is_empty() {
                    return;
                }
                let p = root.join(rel);
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(p);
                } else {
                    let _ = std::fs::remove_file(p);
                }
            }
            Op::Rename { from, to } => {
                let (from, to) = (from.rel(), to.rel());
                if from.as_os_str().is_empty() || to.as_os_str().is_empty() {
                    return;
                }
                let to = root.join(to);
                if let Some(parent) = to.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(root.join(from), to);
            }
            Op::MoveIn { to, deep } => {
                let tree = self.outside().join("tree");
                let _ = std::fs::remove_dir_all(&tree);
                let mut files = vec!["f"];
                if *deep {
                    files.extend(["b/g.log", "b/c/f", "gen/f"]);
                }
                for f in files {
                    let p = tree.join(f);
                    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
                    std::fs::write(p, "moved").expect("write");
                }
                let to = root.join(to.get(DIRS));
                if let Some(parent) = to.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&tree, to);
            }
            Op::MoveOut(d) => {
                let to = self.outside().join("gone");
                let _ = std::fs::remove_dir_all(&to);
                let _ = std::fs::rename(root.join(d.get(DIRS)), to);
            }
            Op::Ignore(file, picked) => {
                let path = match file {
                    IgnoreFile::Root => root.join(".gitignore"),
                    IgnoreFile::Sub => root.join("a/.gitignore"),
                    IgnoreFile::Exclude => {
                        let (_, common_dir) = self.git_dirs();
                        common_dir.join("info/exclude")
                    }
                };
                let mut text = String::new();
                for (rule, _) in RULES.iter().zip(picked).filter(|(_, p)| **p) {
                    text.push_str(rule);
                    text.push('\n');
                }
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, text);
            }
            Op::Git { main, op } => {
                let dir = if *main { &self.main } else { root };
                git(dir, &op.args()[..]);
            }
            Op::Refresh | Op::Pause(_) => {}
        }
    }

    /// What the app would show of the working directory: the status, the
    /// index, and the content of every file that isn't ignored.
    fn worktree(&self) -> Vec<u8> {
        let mut out = must(
            &self.root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=dirty",
            ],
        );
        out.extend(must(&self.root, &["ls-files", "-z", "--stage"]));
        let files = must(
            &self.root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        );
        for name in files.split(|&b| b == 0).filter(|n| !n.is_empty()) {
            out.extend_from_slice(b"\n");
            out.extend_from_slice(name);
            out.extend_from_slice(b": ");
            let path = self.root.join(tergi::util::fs_path(name));
            match std::fs::read(path) {
                Ok(content) => out.extend(content),
                Err(e) => out.extend(format!("<{:?}>", e.kind()).bytes()),
            }
        }
        out
    }

    /// What the app would show of the refs: every ref, `HEAD` and the stash
    /// list.
    fn refs(&self) -> Vec<u8> {
        let mut out = must(
            &self.root,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        out.extend(git(&self.root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default());
        out.extend(git(&self.root, &["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default());
        out.extend(
            git(
                &self.root,
                &["log", "-g", "--format=%H", "refs/stash", "--"],
            )
            .unwrap_or_default(),
        );
        out
    }

    /// Load the status as the app does and send it to the watcher.
    fn tell(&self, handle: &watch::Handle) {
        let out = must(&self.root, Status::ARGS);
        handle.updates().send(&status::parse(&out));
    }
}

enum Got {
    Changed(u8),
    Degraded(String),
}

/// Collect what the watcher reports until it has been quiet for `quiet`.
fn settle(rx: &Receiver<Got>, flags: &mut u8, quiet: Duration) -> Result<(), TestCaseError> {
    loop {
        match rx.recv_timeout(quiet) {
            Ok(Got::Changed(f)) => *flags |= f,
            Ok(Got::Degraded(m)) => return Err(TestCaseError::fail(format!("degraded: {m}"))),
            Err(_) => return Ok(()),
        }
    }
}

/// Collect what the watcher reports until `enough` says that it's enough.
/// Reports can come a while after the change, so this waits much longer
/// than any of them should take before it gives up. Returns whether it was
/// enough.
fn wait(
    rx: &Receiver<Got>,
    flags: &mut u8,
    enough: impl Fn(u8) -> bool,
) -> Result<bool, TestCaseError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !enough(*flags) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Got::Changed(f)) => *flags |= f,
            Ok(Got::Degraded(m)) => return Err(TestCaseError::fail(format!("degraded: {m}"))),
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

/// The paths among `paths`, relative to the root, that git ignores.
fn ignored(root: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run git");
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&input)
        .expect("write to git");
    // It fails when nothing is ignored.
    let out = child.wait_with_output().expect("run git");
    out.stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(tergi::util::fs_path(p)))
        .collect()
}

/// Change something in every folder where git would show the change, one
/// folder at a time, and check that the watcher reports each one. Where
/// git would show a new file, write one. In an ignored folder, change a
/// tracked file if there is one.
///
/// A burst often changes several folders, and a report for one of them
/// hides a change that the watcher missed in another. This finds folders
/// that the watcher stopped watching, or never watched.
fn probe(repo: &Repo, rx: &Receiver<Got>, quiet: Duration) -> Result<(), TestCaseError> {
    let mut dirs = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(repo.root.join(&dir)) {
            for e in entries.flatten() {
                if e.file_name() != ".git" && e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(dir.join(e.file_name()));
                }
            }
        }
        dirs.push(dir);
    }
    let new: Vec<PathBuf> = dirs.iter().map(|d| d.join("probe")).collect();
    let ignored = ignored(&repo.root, &new);
    let mut changes: Vec<(PathBuf, bool)> = new
        .into_iter()
        .filter(|p| !ignored.contains(p))
        .map(|p| (p, false))
        .collect();
    let tracked = must(&repo.root, &["ls-files", "-z"]);
    for name in tracked.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let rel = PathBuf::from(tergi::util::fs_path(name));
        let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        if ignored.contains(&dir.join("probe"))
            && !changes.iter().any(|(p, _)| p.parent() == Some(&dir))
            && repo.root.join(&rel).is_file()
        {
            changes.push((rel, true));
        }
    }
    for (rel, append) in changes {
        let path = repo.root.join(&rel);
        if append {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .expect("open");
            f.write_all(b"probe").expect("write");
        } else {
            std::fs::write(path, "probe").expect("write");
        }
        let mut flags = 0;
        let seen = wait(rx, &mut flags, |f| f & WORKTREE != 0)?;
        prop_assert!(seen, "the watcher missed a change to {}", rel.display());
        settle(rx, &mut flags, quiet)?;
    }
    Ok(())
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let repo = Repo::new(case);
    let (git_dir, common_dir) = repo.git_dirs();
    let (tx, rx) = channel();
    let delays = Delays {
        quiet: Duration::from_millis(case.quiet),
        max: Duration::from_millis(case.quiet * 5),
    };
    let (handle, run) = watch::start_with(&repo.root, &git_dir, &common_dir, delays, move |e| {
        let _ = tx.send(match e {
            Event::Changed(f) => Got::Changed(f),
            Event::Degraded(m) => Got::Degraded(m),
        });
    })
    .expect("start the watcher");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.spawn(run);
    // Long enough after the last report that the watcher has reported
    // everything it's going to.
    let quiet = Duration::from_millis(case.quiet * 2 + 60);
    // The app loads the status right after it starts the watcher.
    repo.tell(&handle);
    settle(&rx, &mut 0, quiet)?;

    for (n, burst) in case.bursts.iter().enumerate() {
        let tree = repo.worktree();
        let refs = repo.refs();
        for op in burst {
            match op {
                Op::Refresh => repo.tell(&handle),
                Op::Pause(n) => std::thread::sleep(Duration::from_millis(20 * u64::from(*n))),
                _ => repo.apply(op),
            }
        }
        let tree_changed = repo.worktree() != tree;
        let refs_changed = repo.refs() != refs;
        let mut flags = 0;
        let seen = wait(&rx, &mut flags, |flags| {
            (!tree_changed || flags & (WORKTREE | INDEX) != 0)
                && (!refs_changed || flags & REFS != 0)
        })?;
        prop_assert!(
            seen,
            "burst {n} changed the working directory: {tree_changed}, the refs: {refs_changed}, \
             but the watcher reported {flags:#06b}"
        );
        // Like the app, load the status after a report that asks for it,
        // until the watcher stops asking.
        settle(&rx, &mut flags, quiet)?;
        for _ in 0..5 {
            if flags & (WORKTREE | INDEX) == 0 {
                break;
            }
            repo.tell(&handle);
            flags = 0;
            settle(&rx, &mut flags, quiet)?;
        }
    }
    probe(&repo, &rx, quiet)
}

/// The cases are split over this many tests, which run at the same time.
/// Most of each case is spent waiting for the watcher to go quiet, so they
/// barely compete for the processor.
const SPLIT: u32 = 4;

/// Run `cases` cases in all, unless `PROPTEST_CASES` asks for some other
/// number, split evenly over the tests. `ProptestConfig::with_cases` would
/// ignore the variable.
fn config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = cases;
    }
    config.cases = config.cases.div_ceil(SPLIT);
    // Each case takes seconds, so shrinking a failure for long would take
    // hours.
    if std::env::var_os("PROPTEST_MAX_SHRINK_ITERS").is_none() {
        config.max_shrink_iters = 200;
    }
    config
}

proptest! {
    #![proptest_config(config(16))]

    #[test]
    fn the_watcher_reports_every_change_1(case in case()) {
        check(&case)?;
    }

    #[test]
    fn the_watcher_reports_every_change_2(case in case()) {
        check(&case)?;
    }

    #[test]
    fn the_watcher_reports_every_change_3(case in case()) {
        check(&case)?;
    }

    #[test]
    fn the_watcher_reports_every_change_4(case in case()) {
        check(&case)?;
    }
}
