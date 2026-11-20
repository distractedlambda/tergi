//! Property tests for commit diffs, run against the real git.
//!
//! `commit::load` diffs trees with gitoxide and texts with imara-diff, not
//! with git. Each case builds two random trees and checks what it gives for
//! the commit between them against `git diff-tree`. The trees hold text,
//! empty, binary and executable files, symlinks and submodules, and a path
//! can be a file in one tree and a folder in the other. Some files move to
//! another path, so that renames show up too.
//!
//! The same trees are also committed as two unrelated root commits, and
//! `commit::compare` between those is checked against `git diff-tree` in
//! both directions.
//!
//! Some cases make a stash with untracked files instead, from a working
//! tree that holds the second tree.
//!
//! For each file, the test checks the status, paths, modes and ids against
//! git, and whether git thinks it is binary. It checks the texts kept for
//! highlighting, and that applying the hunks to the old text gives the new
//! text. It doesn't compare the hunks with git's, because imara-diff and
//! git don't always pick the same lines as changed.
//!
//! Copies are left out. Gix finds them in a different way from git, which
//! `todo/bugs.md` describes.

use crate::common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use common::Scratch;
use common::settings::{Settings, settings};
use gix::bstr::{BStr, BString, ByteSlice as _};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::commit::{self, CommitDetail};
use tergi::git::diff::{FileDiff, Hunk, LineKind};

#[derive(Clone, PartialEq, Eq)]
enum Content {
    Text {
        data: Vec<u8>,
        exec: bool,
    },
    Binary(Vec<u8>),
    /// A symlink to the given target.
    Link(String),
    /// A submodule at the given commit.
    Submodule(&'static str),
}

impl Content {
    fn mode(&self) -> &'static str {
        match self {
            Content::Text { exec: true, .. } => "100755",
            Content::Text { exec: false, .. } | Content::Binary(_) => "100644",
            Content::Link(_) => "120000",
            Content::Submodule(_) => "160000",
        }
    }

    /// What git stores for this content. Submodules have no blob.
    fn bytes(&self) -> &[u8] {
        match self {
            Content::Text { data, .. } | Content::Binary(data) => data,
            Content::Link(target) => target.as_bytes(),
            Content::Submodule(_) => b"",
        }
    }
}

/// Shows the data as escaped text, which is easier to read when a case
/// fails.
impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = |data: &[u8]| data.escape_ascii().to_string();
        match self {
            Content::Text { data, exec } => write!(f, "Text({:?}, exec: {exec})", text(data)),
            Content::Binary(data) => write!(f, "Binary({:?})", text(data)),
            Content::Link(target) => write!(f, "Link({target:?})"),
            Content::Submodule(id) => write!(f, "Submodule({id})"),
        }
    }
}

/// How the second version of a file comes from the first one.
#[derive(Debug, Clone)]
enum Layer {
    Same,
    Set(Option<Content>),
    /// The file moves to another path, maybe a little changed, so that it
    /// can be found as a rename.
    Move(&'static str, Edit),
}

#[derive(Debug, Clone, Copy)]
enum Edit {
    None,
    Append,
    DropFirst,
}

#[derive(Debug, Clone)]
struct Entry {
    path: &'static str,
    old: Option<Content>,
    new: Layer,
    /// In a stash, whether to stage the new version.
    add: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Renames {
    Off,
    On,
}

impl Renames {
    fn config(self) -> &'static str {
        match self {
            Renames::Off => "false",
            Renames::On => "true",
        }
    }

    fn flag(self) -> Option<&'static str> {
        match self {
            Renames::Off => None,
            Renames::On => Some("-M"),
        }
    }
}

#[derive(Debug, Clone)]
struct Case {
    entries: Vec<Entry>,
    context: u32,
    renames: Renames,
    stash: bool,
    settings: Settings,
}

/// A few paths, so that the same path often shows up on both sides, and
/// some are folders of others.
const PATHS: &[&str] = &[
    "a", "b", "c", "a/a", "a/b", "b/c", "d/e", "d/e/f", "x y", "é",
];

fn text() -> impl Strategy<Value = Vec<u8>> {
    let line = prop::sample::select(vec![
        &b"a"[..],
        b"b",
        b"c",
        b"d",
        b"e",
        b"",
        b"x\r",
        "é".as_bytes(),
        b"\xe9",
    ]);
    (
        prop::collection::vec(line, 0..10),
        prop::bool::weighted(0.8),
    )
        .prop_map(|(lines, eol)| {
            let mut data = lines.join(&b'\n');
            if eol && !lines.is_empty() {
                data.push(b'\n');
            }
            data
        })
}

fn content() -> impl Strategy<Value = Content> {
    prop_oneof![
        8 => (text(), prop::bool::weighted(0.2))
            .prop_map(|(data, exec)| Content::Text { data, exec }),
        2 => (text(), any::<u8>()).prop_map(|(mut data, at)| {
            data.insert(usize::from(at) % (data.len() + 1), 0);
            Content::Binary(data)
        }),
        // Git only looks for a NUL byte in the first 8000 bytes.
        1 => (7990usize..8010, text()).prop_map(|(at, tail)| {
            let mut data = b"a\n".repeat(at / 2);
            data.resize(at, b'b');
            data.push(0);
            data.extend(tail);
            if at >= 8000 {
                Content::Text { data, exec: false }
            } else {
                Content::Binary(data)
            }
        }),
        2 => prop::sample::select(vec!["a", "b", "nowhere", "d/e"])
            .prop_map(|t| Content::Link(t.to_string())),
        1 => prop::sample::select(vec![
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ])
        .prop_map(Content::Submodule),
    ]
}

fn layer() -> impl Strategy<Value = Layer> {
    let edit = prop::sample::select(vec![Edit::None, Edit::Append, Edit::DropFirst]);
    prop_oneof![
        3 => Just(Layer::Same),
        1 => Just(Layer::Set(None)),
        3 => content().prop_map(|c| Layer::Set(Some(c))),
        3 => (prop::sample::select(PATHS), edit).prop_map(|(p, e)| Layer::Move(p, e)),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    let entry = (
        prop::sample::select(PATHS),
        prop::option::weighted(0.7, content()),
        layer(),
        prop::bool::weighted(0.3),
    )
        .prop_map(|(path, old, new, add)| Entry {
            path,
            old,
            new,
            add,
        });
    (
        prop::collection::vec(entry, 1..8),
        0u32..4,
        prop::sample::select(vec![Renames::Off, Renames::On]),
        prop::bool::weighted(0.3),
        settings(),
    )
        .prop_map(|(entries, context, renames, stash, settings)| Case {
            entries,
            context,
            renames,
            stash,
            settings,
        })
}

fn edit(content: &Content, edit: Edit) -> Content {
    let Content::Text { data, exec } = content else {
        return content.clone();
    };
    let mut data = data.clone();
    match edit {
        Edit::None => {}
        Edit::Append => data.extend(b"new\n"),
        Edit::DropFirst => {
            let end = data
                .iter()
                .position(|&b| b == b'\n')
                .map_or(data.len(), |i| i + 1);
            data.drain(..end);
        }
    }
    Content::Text { data, exec: *exec }
}

/// The files of one side, keeping the first of any paths that clash.
///
/// Stashes leave out submodules, which would need a real repository in the
/// working tree. They also leave out symlinks that replace a folder, because
/// git can't stash the files that were in it.
fn tree(case: &Case, new: bool) -> Vec<(&'static str, Content)> {
    let version = |e: &Entry| -> Option<(&'static str, Content)> {
        let old = || Some((e.path, e.old.clone()?));
        if !new {
            return old();
        }
        match &e.new {
            Layer::Same => old(),
            Layer::Set(c) => Some((e.path, c.clone()?)),
            Layer::Move(to, ed) => Some((*to, edit(e.old.as_ref()?, *ed))),
        }
    };
    let old = if new { tree(case, false) } else { Vec::new() };
    let clash = |a: &str, b: &str| a == b || b.starts_with(&format!("{a}/"));
    let folder = |p: &str| old.iter().any(|(o, _)| clash(p, o) && p != *o);
    let mut out: Vec<(&'static str, Content)> = Vec::new();
    for e in &case.entries {
        let Some((path, c)) = version(e) else {
            continue;
        };
        let skip = match c {
            Content::Submodule(_) => true,
            Content::Link(_) => folder(path),
            _ => false,
        };
        if case.stash && skip {
            continue;
        }
        if !out.iter().any(|(p, _)| clash(p, path) || clash(path, p)) {
            out.push((path, c));
        }
    }
    out
}

fn git_out(s: &Scratch, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(s.path())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run git");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin).expect("write");
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

fn git(s: &Scratch, args: &[&str]) -> Vec<u8> {
    git_out(s, args, b"")
}

/// Commands for `git fast-import` that commit the given files to `branch`.
fn import(out: &mut Vec<u8>, branch: &str, parent: Option<&str>, files: &[(&str, Content)]) {
    out.extend(format!("commit refs/heads/{branch}\n").as_bytes());
    out.extend(b"committer Test User <test@example.com> 0 +0000\n");
    out.extend(format!("data {}\n{branch}\n", branch.len() + 1).as_bytes());
    if let Some(p) = parent {
        out.extend(format!("from refs/heads/{p}\ndeleteall\n").as_bytes());
    }
    for (path, c) in files {
        if let Content::Submodule(id) = c {
            out.extend(format!("M 160000 {id} {path}\n").as_bytes());
            continue;
        }
        let data = c.bytes();
        out.extend(format!("M {} inline {path}\ndata {}\n", c.mode(), data.len()).as_bytes());
        out.extend(data);
        out.push(b'\n');
    }
}

/// Put the files of a side in the working tree, replacing whatever the
/// other side left there.
fn write_tree(s: &Scratch, old: &[(&str, Content)], new: &[(&str, Content)]) {
    for (path, _) in old {
        std::fs::remove_file(s.file(path)).expect("remove");
    }
    for (path, c) in new {
        let path = s.file(path);
        if path.is_dir() {
            std::fs::remove_dir_all(&path).expect("remove folder");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        match c {
            Content::Link(target) => std::os::unix::fs::symlink(target, &path).expect("symlink"),
            _ => {
                std::fs::write(&path, c.bytes()).expect("write");
                let mode = if c.mode() == "100755" { 0o755 } else { 0o644 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("chmod");
            }
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Side {
    path: String,
    mode: String,
    id: String,
}

/// A changed file, as git lists it and as we expect tergi to list it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Change {
    status: char,
    old: Option<Side>,
    new: Option<Side>,
}

/// Parse the output of `git diff-tree -r -z --no-abbrev`.
fn raw(out: &[u8]) -> Vec<Change> {
    let f = fields(out);
    let mut it = f.into_iter();
    let mut changes = Vec::new();
    while let Some(meta) = it.next() {
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            panic!("odd raw line: {meta}")
        };
        let status = status.chars().next().expect("status");
        let path = it.next().expect("path");
        let new_path = if matches!(status, 'R' | 'C') {
            it.next().expect("second path")
        } else {
            path.clone()
        };
        let side = |path: String, mode: &str, id: &str| Side {
            path,
            mode: mode.to_string(),
            id: id.to_string(),
        };
        changes.push(Change {
            status,
            old: (status != 'A').then(|| side(path, old_mode, old_id)),
            new: (status != 'D').then(|| side(new_path, new_mode, new_id)),
        });
    }
    changes
}

/// The files of a tree, as untracked files.
fn untracked(s: &Scratch, tree: &str) -> Vec<Change> {
    fields(&git(s, &["ls-tree", "-r", "-z", tree]))
        .into_iter()
        .map(|rec| {
            let (meta, path) = rec.split_once('\t').expect("tab");
            let parts: Vec<&str> = meta.split(' ').collect();
            Change {
                status: '?',
                old: None,
                new: Some(Side {
                    path: path.to_string(),
                    mode: parts[0].to_string(),
                    id: parts[2].to_string(),
                }),
            }
        })
        .collect()
}

/// Split renames of files whose content changed into a deleted file and an
/// added file. Git and gix measure how similar two files are in different
/// ways, so they don't always agree on these. Renames of files that stayed
/// the same have to match.
fn exact_renames(changes: &[Change]) -> Vec<Change> {
    let mut out = Vec::new();
    for c in changes {
        match (&c.old, &c.new) {
            (Some(old), Some(new)) if c.status == 'R' && old.id != new.id => {
                out.push(Change {
                    status: 'D',
                    old: Some(old.clone()),
                    new: None,
                });
                out.push(Change {
                    status: 'A',
                    old: None,
                    new: Some(new.clone()),
                });
            }
            _ => out.push(c.clone()),
        }
    }
    out.sort();
    out
}

fn change_of(f: &FileDiff) -> Change {
    let side = |path: &Option<BString>, mode: &Option<String>, id: Option<gix::ObjectId>| {
        path.as_ref().map(|p| Side {
            path: p.to_str().expect("utf-8").to_string(),
            mode: mode.clone().unwrap_or_default(),
            id: id.map(|i| i.to_string()).unwrap_or_default(),
        })
    };
    Change {
        status: f.status.letter(),
        old: side(&f.old_path, &f.old_mode, f.old_id),
        new: side(&f.new_path, &f.new_mode, f.new_id),
    }
}

/// Apply hunks to the old text.
fn apply(old: &str, hunks: &[Hunk]) -> Result<String, TestCaseError> {
    let old: Vec<&str> = old.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut pos = 0;
    for h in hunks {
        let first = if h.old_len == 0 {
            h.old_start
        } else {
            h.old_start - 1
        } as usize;
        prop_assert!(first >= pos, "hunks overlap or are out of order");
        out.extend(old[pos..first].iter().copied());
        pos = first;
        for l in &h.lines {
            if l.kind != LineKind::Add {
                let line = old.get(pos).copied().unwrap_or_default();
                prop_assert_eq!(line.strip_suffix('\n').unwrap_or(line), &l.text);
                pos += 1;
            }
            if l.kind != LineKind::Del {
                out.push_str(&l.text);
                if !l.no_eol {
                    out.push('\n');
                }
            }
        }
    }
    out.extend(old[pos..].iter().copied());
    Ok(out)
}

/// Whether git thinks a blob is binary. Its diff against the empty blob
/// then has "-" for its line counts.
fn git_binary(s: &Scratch, id: &str) -> bool {
    let empty = git_out(s, &["hash-object", "-w", "--stdin"], b"");
    let empty = String::from_utf8(empty).expect("utf-8");
    git(s, &["diff", "--numstat", empty.trim(), id]).starts_with(b"-\t-\t")
}

/// The text tergi should keep for one side of a change, or `None` if git
/// thinks it is binary.
fn text_of(s: &Scratch, side: &Side) -> Option<String> {
    if side.mode == "160000" {
        return Some(format!("Subproject commit {}\n", side.id));
    }
    if git_binary(s, &side.id) {
        return None;
    }
    let data = git(s, &["cat-file", "blob", &side.id]);
    Some(String::from_utf8_lossy(&data).into_owned())
}

/// Check what `commit::load` gives against git's listing.
fn check_detail(s: &Scratch, detail: &CommitDetail, want: &[Change]) -> Result<(), TestCaseError> {
    let got: Vec<Change> = detail.files.iter().map(|f| change_of(f)).collect();
    prop_assert_eq!(exact_renames(&got), exact_renames(want), "files");

    let paths: Vec<&BStr> = detail.files.iter().map(|f| f.path()).collect();
    let mut by_path = paths.clone();
    by_path.sort();
    prop_assert_eq!(paths, by_path, "sorted by path");

    for ((f, t), c) in detail.files.iter().zip(&detail.texts).zip(&got) {
        let old = c.old.as_ref().map(|side| text_of(s, side));
        let new = c.new.as_ref().map(|side| text_of(s, side));
        let binary = [&old, &new].into_iter().flatten().any(Option::is_none);
        prop_assert_eq!(f.binary, binary, "binary flag of {:?}", f.path());
        prop_assert!(!f.too_large);

        let want_texts = if binary {
            (None, None)
        } else {
            (old.flatten(), new.flatten())
        };
        let got_texts = (
            t.old.as_deref().map(str::to_string),
            t.new.as_deref().map(str::to_string),
        );
        prop_assert_eq!(&got_texts, &want_texts, "texts of {:?}", f.path());
        if binary {
            prop_assert!(f.hunks.is_empty());
            continue;
        }

        let (old, new) = want_texts;
        let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
        prop_assert_eq!(f.hunks.is_empty(), old == new, "hunks of {:?}", f.path());
        prop_assert_eq!(apply(&old, &f.hunks)?, new, "applying {:?}", f.path());
    }
    Ok(())
}

/// The files that git lists for the diff from `old` to `new`, or for the
/// root commit `new` if `old` is `None`.
fn git_diff(s: &Scratch, case: &Case, old: Option<&str>, new: &str) -> Vec<Change> {
    let mut args = vec!["diff-tree", "-r", "-z", "--no-abbrev", "--no-commit-id"];
    args.extend(case.renames.flag());
    match old {
        Some(old) => args.push(old),
        None => args.push("--root"),
    }
    args.push(new);
    raw(&git(s, &args))
}

fn rev_parse(s: &Scratch, rev: &str) -> gix::ObjectId {
    let id = String::from_utf8(git(s, &["rev-parse", rev])).expect("utf-8");
    gix::ObjectId::from_hex(id.trim().as_bytes()).expect("id")
}

fn load(s: &Scratch, case: &Case, rev: &str) -> CommitDetail {
    let repo = gix::open(s.path()).expect("open");
    commit::load(&repo, rev_parse(s, rev), case.context, usize::MAX).expect("load")
}

fn compare(s: &Scratch, case: &Case, from: &str, to: &str) -> CommitDetail {
    let repo = gix::open(s.path()).expect("open");
    let (from, to) = (rev_parse(s, from), rev_parse(s, to));
    let c = commit::compare(&repo, from, to, case.context, usize::MAX).expect("compare");
    assert_eq!((c.id, c.base.as_ref().map(|b| b.id)), (to, Some(from)));
    c
}

fn check_commits(s: &Scratch, case: &Case) -> Result<(), TestCaseError> {
    let (old, new) = (tree(case, false), tree(case, true));
    let mut stream = Vec::new();
    import(&mut stream, "old", None, &old);
    import(&mut stream, "new", Some("old"), &new);
    import(&mut stream, "other", None, &new);
    git_out(s, &["fast-import", "--quiet"], &stream);

    check_detail(s, &load(s, case, "old"), &git_diff(s, case, None, "old"))?;
    check_detail(
        s,
        &load(s, case, "new"),
        &git_diff(s, case, Some("old"), "new"),
    )?;
    check_detail(
        s,
        &compare(s, case, "old", "other"),
        &git_diff(s, case, Some("old"), "other"),
    )?;
    check_detail(
        s,
        &compare(s, case, "other", "old"),
        &git_diff(s, case, Some("other"), "old"),
    )
}

fn check_stash(s: &Scratch, case: &Case) -> Result<(), TestCaseError> {
    let (old, new) = (tree(case, false), tree(case, true));
    let mut stream = Vec::new();
    import(&mut stream, "main", None, &old);
    git_out(s, &["fast-import", "--quiet"], &stream);
    s.sh(&["reset", "-q", "--hard"]);
    write_tree(s, &old, &new);

    let added: Vec<String> = case
        .entries
        .iter()
        .filter(|e| e.add)
        .map(|e| match e.new {
            Layer::Move(to, _) => to,
            _ => e.path,
        })
        .filter(|path| new.iter().any(|(p, _)| p == path))
        .map(|path| format!(":(literal){path}"))
        .collect();
    if !added.is_empty() {
        let mut args = vec!["add", "-A", "--"];
        args.extend(added.iter().map(String::as_str));
        s.sh(&args);
    }
    s.sh(&["stash", "push", "-q", "--include-untracked"]);
    if s.sh(&["stash", "list"]).is_empty() {
        return Ok(());
    }

    let mut files = git_diff(s, case, Some("stash@{0}^1"), "stash@{0}");
    // Without untracked files, the stash has only two parents.
    if !s
        .sh(&["rev-parse", "-q", "--verify", "stash@{0}^3"])
        .is_empty()
    {
        files.extend(untracked(s, "stash@{0}^3"));
    }
    check_detail(s, &load(s, case, "stash@{0}"), &files)
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = Scratch::with(&case.settings);
    s.sh(&["config", "diff.renames", case.renames.config()]);
    if case.stash {
        check_stash(&s, case)
    } else {
        check_commits(&s, case)
    }
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

common::split_proptest! {
    config = config(100);
    parts = [part_1 part_2 part_3 part_4];

    fn commit_diffs_match_git(case in case()) {
        check(&case)?;
    }
}
