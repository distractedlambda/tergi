//! Property tests for odd file names, run against the real git.
//!
//! Each case makes a scratch repository with a few files whose names hold
//! spaces, quotes, backslashes, tabs, newlines, ` b/`, `->`, a leading `-`,
//! trailing spaces and non-ASCII text. Each file gets random versions in
//! HEAD, the index and the working tree. A version can be text, binary or a
//! symlink, and text can be executable. So the files get added, edited,
//! deleted, change their mode and change their type.
//!
//! Then the test checks what tergi reads against what git reports in its
//! `-z` formats, which never quote names:
//!
//! - the paths in `Workdir`'s status and diff lists, with their modes,
//! - the full texts kept for syntax highlighting,
//! - staging or unstaging one line, or all lines, of each file,
//! - the paths, modes and ids that `commit::load` gives for a commit.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::Scratch;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::commit::{self, FileTexts};
use tergi::git::diff::{FileDiff, FileStatus};
use tergi::git::ops::{ApplyMode, apply_lines};
use tergi::git::patch::build_patch;
use tergi::git::workdir::Workdir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Text {
        text: String,
        exec: bool,
    },
    Binary(Vec<u8>),
    /// A symlink to the given target.
    Link(String),
}

impl Content {
    /// What git stores for this content.
    fn bytes(&self) -> &[u8] {
        match self {
            Content::Text { text, .. } => text.as_bytes(),
            Content::Binary(data) => data,
            Content::Link(target) => target.as_bytes(),
        }
    }

    fn mode(&self) -> &'static str {
        match self {
            Content::Text { exec: true, .. } => "100755",
            Content::Text { exec: false, .. } | Content::Binary(_) => "100644",
            Content::Link(_) => "120000",
        }
    }
}

/// How one version of a file comes from the version below it.
#[derive(Debug, Clone)]
enum Layer {
    Same,
    Set(Option<Content>),
}

impl Layer {
    fn apply(&self, below: &Option<Content>) -> Option<Content> {
        match self {
            Layer::Same => below.clone(),
            Layer::Set(c) => c.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Versions {
    head: Option<Content>,
    index: Option<Content>,
    work: Option<Content>,
}

#[derive(Debug, Clone)]
struct Case {
    names: Vec<String>,
    files: Vec<Versions>,
    context: u32,
    /// Stage the unstaged and untracked files, or unstage the staged ones.
    stage: bool,
    /// For each file, whether to apply all of its lines or only the first
    /// changed line. Repeated as needed.
    whole: Vec<bool>,
}

/// One component of a path, which can't contain a slash.
fn component() -> impl Strategy<Value = String> {
    let piece = prop::sample::select(vec![
        "a", "b", "x", " ", "  ", "\"", "\\", "\t", "\n", "\r", " b", "->", "-", "é", "日本", ":",
        "1:", "*", "?", "[a]", "#", "'", "~",
    ]);
    prop::collection::vec(piece, 1..4)
        .prop_map(|pieces| pieces.concat())
        .prop_filter("reserved name", |c| {
            !matches!(c.as_str(), "." | ".." | ".git")
        })
}

fn name() -> impl Strategy<Value = String> {
    prop::collection::vec(component(), 1..3).prop_map(|cs| cs.join("/"))
}

/// Keep only the files whose names can all exist at once: no repeats, and
/// no name that is a folder of another.
fn usable(files: Vec<(String, Versions)>) -> Vec<(String, Versions)> {
    let clash = |a: &str, b: &str| a == b || b.starts_with(&format!("{a}/"));
    let mut out: Vec<(String, Versions)> = Vec::new();
    for (n, v) in files {
        if n != "keep.txt" && !out.iter().any(|(o, _)| clash(o, &n) || clash(&n, o)) {
            out.push((n, v));
        }
    }
    out
}

fn content() -> impl Strategy<Value = Content> {
    let line = prop::sample::select(vec!["a", "b", "c", ""]);
    prop_oneof![
        6 => (
            prop::collection::vec(line, 0..6),
            prop::bool::weighted(0.8),
            prop::bool::weighted(0.2),
        )
            .prop_map(|(lines, eol, exec)| {
                let mut text = lines.join("\n");
                if eol && !lines.is_empty() {
                    text.push('\n');
                }
                Content::Text { text, exec }
            }),
        1 => prop::collection::vec(any::<u8>(), 0..8).prop_map(|mut data| {
            data.insert(0, 0);
            Content::Binary(data)
        }),
        // Targets that exist, one that doesn't, and other odd names.
        2 => prop_oneof![
            Just("keep.txt".to_string()),
            Just("nowhere".to_string()),
            name(),
        ]
        .prop_map(Content::Link),
    ]
}

fn layer() -> impl Strategy<Value = Layer> {
    prop_oneof![
        2 => Just(Layer::Same),
        1 => Just(Layer::Set(None)),
        4 => content().prop_map(|c| Layer::Set(Some(c))),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    let versions = (prop::option::weighted(0.7, content()), layer(), layer()).prop_map(
        |(head, index, work)| {
            let index = index.apply(&head);
            let work = work.apply(&index);
            Versions { head, index, work }
        },
    );
    (
        prop::collection::vec((name(), versions), 1..6),
        0u32..4,
        any::<bool>(),
        prop::collection::vec(any::<bool>(), 1..6),
    )
        .prop_map(|(files, context, stage, whole)| {
            let (names, files) = usable(files).into_iter().unzip();
            Case {
                names,
                files,
                context,
                stage,
                whole,
            }
        })
}

/// Put `content` at `rel` in the working tree, or remove what is there.
fn put(s: &Scratch, rel: &str, content: Option<&Content>) {
    let path = s.file(rel);
    if std::fs::symlink_metadata(&path).is_ok() {
        std::fs::remove_file(&path).expect("remove");
    }
    let Some(content) = content else { return };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    match content {
        Content::Link(target) => std::os::unix::fs::symlink(target, &path).expect("symlink"),
        _ => {
            std::fs::write(&path, content.bytes()).expect("write");
            let mode = if content.mode() == "100755" {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }
    }
}

fn git(s: &Scratch, args: &[&str]) -> Vec<u8> {
    git_in(s.path(), args, None)
}

fn git_in(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run git");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin.unwrap_or_default()).expect("write");
    drop(input);
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// The fields of a `-z` listing, as strings.
fn fields(out: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = out
        .split(|&b| b == 0)
        .map(|f| String::from_utf8(f.to_vec()).expect("utf-8"))
        .collect();
    if v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v
}

/// One side of a changed file: its path, mode and blob id. The id is
/// `None` for the working tree side, where git has no blob yet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Side {
    path: String,
    mode: String,
    id: Option<String>,
}

/// A changed file the way we expect tergi to list it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Change {
    status: char,
    old: Option<Side>,
    new: Option<Side>,
}

/// Parse the output of `git diff --raw -z --no-renames --no-abbrev`.
///
/// If `split_types` is true, a file that changed type becomes a deleted
/// file and an added file, the way `git diff -p` shows it.
fn raw(out: &[u8], split_types: bool) -> Vec<Change> {
    let f = fields(out);
    let zero = |id: &str| (!id.bytes().all(|b| b == b'0')).then(|| id.to_string());
    let mut changes = Vec::new();
    for pair in f.chunks(2) {
        let [meta, path] = pair else {
            panic!("odd raw output: {f:?}")
        };
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            panic!("odd raw line: {meta}")
        };
        let side = |mode: &str, id: &str| Side {
            path: path.clone(),
            mode: mode.to_string(),
            id: zero(id),
        };
        let old = side(old_mode, old_id);
        let new = side(new_mode, new_id);
        match status {
            "A" => changes.push(Change {
                status: 'A',
                old: None,
                new: Some(new),
            }),
            "D" => changes.push(Change {
                status: 'D',
                old: Some(old),
                new: None,
            }),
            "T" if split_types => {
                changes.push(Change {
                    status: 'D',
                    old: Some(old),
                    new: None,
                });
                changes.push(Change {
                    status: 'A',
                    old: None,
                    new: Some(new),
                });
            }
            "M" | "T" => changes.push(Change {
                status: status.chars().next().unwrap_or('?'),
                old: Some(old),
                new: Some(new),
            }),
            _ => panic!("unexpected status in {meta}"),
        }
    }
    changes
}

/// What tergi says about a file, in the same form as `Change`. Ids are
/// left out of working tree diffs, which only have abbreviated ones.
fn change_of(f: &FileDiff, with_ids: bool) -> Change {
    let id = |id: Option<gix::ObjectId>| id.filter(|_| with_ids).map(|i| i.to_string());
    let side = |path: &Option<String>, mode: &Option<String>, i| {
        path.as_ref().map(|p| Side {
            path: p.clone(),
            mode: mode.clone().unwrap_or_default(),
            id: id(i),
        })
    };
    Change {
        status: f.status.letter(),
        old: side(&f.old_path, &f.old_mode, f.old_id),
        new: side(&f.new_path, &f.new_mode, f.new_id),
    }
}

fn strip_ids(mut c: Change) -> Change {
    for side in [&mut c.old, &mut c.new].into_iter().flatten() {
        side.id = None;
    }
    c
}

fn blob(s: &Scratch, id: &str) -> Vec<u8> {
    git(s, &["cat-file", "blob", id])
}

/// The text tergi should keep for highlighting, given what is stored.
fn text_of(data: &[u8]) -> Option<String> {
    (!data.contains(&0)).then(|| String::from_utf8_lossy(data).into_owned())
}

/// What git would store for a file in the working tree.
fn disk(s: &Scratch, rel: &str) -> Vec<u8> {
    let path = s.file(rel);
    let meta = std::fs::symlink_metadata(&path).expect("stat");
    if meta.is_symlink() {
        std::fs::read_link(&path)
            .expect("readlink")
            .into_os_string()
            .into_encoded_bytes()
    } else {
        std::fs::read(&path).expect("read")
    }
}

fn texts_eq(got: &FileTexts) -> (Option<String>, Option<String>) {
    (
        got.old.as_deref().map(str::to_string),
        got.new.as_deref().map(str::to_string),
    )
}

/// Check one of `Workdir`'s lists, with its texts, against what git says.
/// `old` and `new` read what is stored on each side of a change.
fn check_list(
    what: &str,
    files: &[Arc<FileDiff>],
    texts: &[FileTexts],
    want: Vec<Change>,
    old: &dyn Fn(&Side) -> Vec<u8>,
    new: &dyn Fn(&Side) -> Vec<u8>,
) -> Result<(), TestCaseError> {
    let mut got: Vec<(Change, &FileDiff, &FileTexts)> = files
        .iter()
        .zip(texts)
        .map(|(f, t)| (change_of(f, false), &**f, t))
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want = want;
    want.sort_by_key(|c| strip_ids(c.clone()));
    let got_changes: Vec<&Change> = got.iter().map(|g| &g.0).collect();
    let want_changes: Vec<Change> = want.iter().cloned().map(strip_ids).collect();
    prop_assert_eq!(
        got_changes,
        want_changes.iter().collect::<Vec<_>>(),
        "{} files",
        what
    );
    for ((_, f, t), w) in got.iter().zip(&want) {
        let old_data = w.old.as_ref().map(old);
        let new_data = w.new.as_ref().map(new);
        let binary = [&old_data, &new_data]
            .into_iter()
            .flatten()
            .any(|d| d.contains(&0));
        prop_assert_eq!(f.binary, binary, "{} binary flag of {:?}", what, f.path());
        // A diff that only changes the mode has no lines to highlight, and
        // git doesn't give the blob ids that tergi reads the texts by.
        if !binary && f.hunks.is_empty() && !f.is_new() && !f.is_deleted() {
            continue;
        }
        let want_texts = if binary {
            (None, None)
        } else {
            (
                old_data.as_deref().and_then(text_of),
                new_data.as_deref().and_then(text_of),
            )
        };
        prop_assert_eq!(texts_eq(t), want_texts, "{} texts of {:?}", what, f.path());
    }
    Ok(())
}

/// Paths and original paths from `git status --porcelain=v1 -z`.
fn status_paths(s: &Scratch) -> Vec<(String, Option<String>)> {
    let f = fields(&git(
        s,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    ));
    let mut out = Vec::new();
    let mut it = f.into_iter();
    while let Some(rec) = it.next() {
        let renamed = matches!(rec.as_bytes()[0], b'R' | b'C');
        let orig = if renamed { it.next() } else { None };
        out.push((rec[3..].to_string(), orig));
    }
    out.sort();
    out
}

fn check_workdir(s: &Scratch, case: &Case, wd: &Workdir) -> Result<(), TestCaseError> {
    let mut got: Vec<(String, Option<String>)> = wd
        .status
        .entries
        .iter()
        .map(|e| match e {
            tergi::git::status::Entry::Changed {
                path, orig_path, ..
            } => (path.clone(), orig_path.clone()),
            e => (e.path().to_string(), None),
        })
        .collect();
    got.sort();
    prop_assert_eq!(got, status_paths(s), "status");

    let diff = |cached: bool| {
        let mut args = vec!["diff", "--raw", "-z", "--no-renames", "--no-abbrev"];
        if cached {
            args.push("--cached");
        }
        raw(&git(s, &args), true)
    };
    let stored = |side: &Side| blob(s, side.id.as_deref().expect("blob id"));
    let on_disk = |side: &Side| disk(s, &side.path);

    check_list(
        "staged",
        &wd.staged,
        &wd.staged_texts,
        diff(true),
        &stored,
        &stored,
    )?;
    check_list(
        "unstaged",
        &wd.unstaged,
        &wd.unstaged_texts,
        diff(false),
        &stored,
        &on_disk,
    )?;

    // Untracked files have no mode in git yet, so the model says which
    // mode git would record.
    let mode_of = |path: &str| {
        let i = case.names.iter().position(|n| n == path)?;
        Some(case.files[i].work.as_ref()?.mode().to_string())
    };
    let untracked = fields(&git(s, &["ls-files", "-o", "-z", "--exclude-standard"]))
        .into_iter()
        .map(|path| Change {
            status: '?',
            old: None,
            new: Some(Side {
                mode: mode_of(&path).unwrap_or_default(),
                path,
                id: None,
            }),
        })
        .collect();
    check_list(
        "untracked",
        &wd.untracked,
        &wd.untracked_texts,
        untracked,
        &stored,
        &on_disk,
    )?;
    Ok(())
}

/// The entries of the index, or of a tree, by path, as (mode, id).
fn entries(s: &Scratch, tree: Option<&str>) -> BTreeMap<String, (String, String)> {
    let out = match tree {
        None => git(s, &["ls-files", "-s", "-z"]),
        Some(t) => git(s, &["ls-tree", "-r", "-z", t]),
    };
    fields(&out)
        .into_iter()
        .map(|rec| {
            let (meta, path) = rec.split_once('\t').expect("tab");
            let parts: Vec<&str> = meta.split(' ').collect();
            let (mode, id) = match tree {
                None => (parts[0], parts[1]),
                Some(_) => (parts[0], parts[2]),
            };
            (path.to_string(), (mode.to_string(), id.to_string()))
        })
        .collect()
}

fn hash(s: &Scratch, data: &[u8]) -> String {
    let out = git_in(s.path(), &["hash-object", "--stdin"], Some(data));
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Stage the unstaged and untracked files, or unstage the staged ones. Each
/// file gets either all of its lines or only its first changed line.
fn check_staging(
    rt: &tokio::runtime::Runtime,
    s: &Scratch,
    case: &Case,
    wd: &Workdir,
) -> Result<(), TestCaseError> {
    let (mode, files): (_, Vec<&Arc<FileDiff>>) = if case.stage {
        (
            ApplyMode::Stage,
            wd.unstaged.iter().chain(&wd.untracked).collect(),
        )
    } else {
        (ApplyMode::Unstage, wd.staged.iter().collect())
    };
    // A file that changed type is listed twice. Applying one half changes
    // what the other half applies to, so leave those alone.
    let mut seen = HashSet::new();
    let twice: HashSet<&str> = files
        .iter()
        .map(|f| f.path())
        .filter(|p| !seen.insert(*p))
        .collect();

    let before = entries(s, None);
    let head = entries(s, Some("HEAD"));
    let mut touched = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        if twice.contains(f.path()) || f.hunks.is_empty() {
            continue;
        }
        let whole = case.whole[i % case.whole.len()];
        let first = f
            .hunks
            .iter()
            .enumerate()
            .find_map(|(hi, h)| Some((hi, h.changed_lines().next()?)));
        let total: usize = f.hunks.iter().map(|h| h.changed_lines().count()).sum();
        let selected = |h, l| whole || Some((h, l)) == first;
        // Some selections can't be applied, and `build_patch` refuses them.
        // `patch_props.rs` checks those, so skip them here.
        if build_patch(f, &selected, mode != ApplyMode::Stage).is_err() {
            continue;
        }
        let result = rt.block_on(apply_lines(&s.git, f, mode, &selected));
        prop_assert!(
            result.is_ok(),
            "{:?} of {:?} failed: {:?}",
            mode,
            f.path(),
            result
        );
        touched.insert(f.path().to_string(), (Arc::clone(f), whole || total == 1));
    }

    let after = entries(s, None);
    for (path, (f, whole)) in &touched {
        let got = after.get(path);
        if !whole {
            prop_assert_ne!(got, before.get(path), "{:?} of a line in {:?}", mode, path);
            continue;
        }
        let want = match mode {
            ApplyMode::Stage => f.new_path.as_ref().map(|p| {
                let mode = f.new_mode.clone().unwrap_or_default();
                (mode, hash(s, &disk(s, p)))
            }),
            _ => head.get(path).cloned(),
        };
        prop_assert_eq!(got, want.as_ref(), "{:?} of all of {:?}", mode, path);
    }
    for (path, entry) in &before {
        if !touched.contains_key(path) {
            prop_assert_eq!(after.get(path), Some(entry), "{:?} is untouched", path);
        }
    }
    Ok(())
}

/// Commit the index and check what `commit::load` gives for that commit.
fn check_commit(s: &Scratch) -> Result<(), TestCaseError> {
    s.sh(&["commit", "-q", "--allow-empty", "-m", "next"]);
    let repo = gix::open(s.path()).expect("open");
    let id = repo.head_id().expect("head").detach();
    let detail = commit::load(&repo, id, 3, usize::MAX).expect("load");

    // Renames show up as a deleted and an added file in the listing.
    let mut got = Vec::new();
    for (f, t) in detail.files.iter().zip(&detail.texts) {
        let c = change_of(f, true);
        if f.status == FileStatus::Renamed {
            got.push((
                Change {
                    status: 'D',
                    old: c.old,
                    new: None,
                },
                f,
                t,
            ));
            got.push((
                Change {
                    status: 'A',
                    old: None,
                    new: c.new,
                },
                f,
                t,
            ));
        } else {
            got.push((c, f, t));
        }
    }
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want = raw(
        &git(
            s,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--no-abbrev",
                "HEAD~",
                "HEAD",
            ],
        ),
        false,
    );
    want.sort();
    let got_changes: Vec<&Change> = got.iter().map(|g| &g.0).collect();
    prop_assert_eq!(got_changes, want.iter().collect::<Vec<_>>(), "commit files");

    for (_, f, t) in got {
        let data = |id: Option<gix::ObjectId>| id.map(|i| blob(s, &i.to_string()));
        let (old, new) = (data(f.old_id), data(f.new_id));
        let binary = [&old, &new].into_iter().flatten().any(|d| d.contains(&0));
        prop_assert_eq!(f.binary, binary, "commit binary flag of {:?}", f.path());
        let want_texts = if binary {
            (None, None)
        } else {
            (
                old.as_deref().and_then(text_of),
                new.as_deref().and_then(text_of),
            )
        };
        prop_assert_eq!(texts_eq(t), want_texts, "commit texts of {:?}", f.path());
    }
    Ok(())
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Scratch::new();
    s.write("keep.txt", "keep\n");
    let layers: [fn(&Versions) -> Option<&Content>; 3] = [
        |v| v.head.as_ref(),
        |v| v.index.as_ref(),
        |v| v.work.as_ref(),
    ];
    for (k, layer) in layers.iter().enumerate() {
        for (name, v) in case.names.iter().zip(&case.files) {
            put(&s, name, layer(v));
        }
        match k {
            0 => {
                s.commit_all("init");
            }
            1 => {
                s.sh(&["add", "-A"]);
            }
            _ => {}
        }
    }

    let wd = rt
        .block_on(Workdir::load(&s.git, case.context, 100_000))
        .expect("load");
    check_workdir(&s, case, &wd)?;
    check_staging(&rt, &s, case, &wd)?;
    check_commit(&s)
}

/// Run `cases` cases, unless `PROPTEST_CASES` asks for some other number.
/// `ProptestConfig::with_cases` would ignore the variable.
fn config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = cases;
    }
    config
}

proptest! {
    #![proptest_config(config(100))]

    #[test]
    fn odd_file_names(case in case()) {
        check(&case)?;
    }
}
