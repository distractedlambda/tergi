//! Property tests for staging, unstaging and discarding selected lines.
//!
//! Each case makes a scratch repository with a random file and a random edit
//! to it, picks random changed lines, and runs them through `apply_lines`
//! with the real `git apply`. A small model works out what the file should
//! hold afterwards, straight from the diff and the selection.
//!
//! `staging_in_steps` does the same over several files and several steps.
//! Each file starts with random versions in HEAD, the index and the working
//! tree, so it can have staged and unstaged changes at once. Each step
//! stages, unstages or discards a section, a file, a hunk or some lines, or
//! edits a file. The model keeps all three versions of each file. After
//! every step the test checks the repository against the model, and checks
//! that `Workdir` lists the right files with diffs that turn one version
//! into the other.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::Scratch;
use common::settings::{Settings, settings};
use gix::bstr::ByteSlice as _;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::commit::FileTexts;
use tergi::git::diff::{FileDiff, LineKind};
use tergi::git::ops::{self, ApplyMode, apply_lines};
use tergi::git::patch::build_patch;
use tergi::git::workdir::Workdir;

const PATH: &str = "f.txt";

/// One line of a file, and whether it ends in a newline.
type Line = (String, bool);

#[derive(Debug, Clone)]
struct File {
    lines: Vec<String>,
    /// Whether the last line ends in a newline.
    eol: bool,
}

impl File {
    fn text(&self) -> String {
        let mut s = self.lines.join("\n");
        if self.eol && !self.lines.is_empty() {
            s.push('\n');
        }
        s
    }
}

fn split(text: &str) -> Vec<Line> {
    text.split_inclusive('\n')
        .map(|l| match l.strip_suffix('\n') {
            Some(l) => (l.to_string(), true),
            None => (l.to_string(), false),
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Exists {
    Both,
    /// The file is new: untracked, or added to the index.
    OnlyNew,
    /// The file is deleted.
    OnlyOld,
}

#[derive(Debug, Clone)]
struct Case {
    old: File,
    new: File,
    exists: Exists,
    mode: ApplyMode,
    context: u32,
    /// Whether each changed line is selected, repeated as needed.
    mask: Vec<bool>,
    settings: Settings,
}

fn line() -> impl Strategy<Value = String> {
    // Few distinct lines, so the diffs have to line up repeated text.
    prop::sample::select(vec!["a", "b", "c", "d", "", "x y"]).prop_map(str::to_string)
}

fn file() -> impl Strategy<Value = File> {
    (prop::collection::vec(line(), 0..24), any::<bool>())
        .prop_map(|(lines, eol)| File { lines, eol })
}

/// A copy of `old` with some lines kept, dropped, replaced or inserted.
/// Each op applies to the old line at the same position, and the last one
/// can append a line. Op 0 keeps the line, which is what shrinking tends to.
fn edit(old: &File, ops: &[(u8, String)], eol: bool) -> File {
    let mut out = Vec::new();
    for i in 0..=old.lines.len() {
        let line = old.lines.get(i);
        match ops.get(i) {
            Some((7, text)) => {
                out.push(text.clone());
                out.extend(line.cloned());
            }
            Some((8, _)) => {}
            Some((9, text)) if line.is_some() => out.push(text.clone()),
            _ => out.extend(line.cloned()),
        }
    }
    File { lines: out, eol }
}

fn case() -> impl Strategy<Value = Case> {
    let exists = prop_oneof![
        6 => Just(Exists::Both),
        1 => Just(Exists::OnlyNew),
        1 => Just(Exists::OnlyOld),
    ];
    let mode = prop_oneof![
        Just(ApplyMode::Stage),
        Just(ApplyMode::Unstage),
        Just(ApplyMode::Discard),
    ];
    (
        file(),
        prop::collection::vec((0u8..10, line()), 0..25),
        prop::bool::weighted(0.8),
        exists,
        mode,
        0u32..4,
        prop::collection::vec(any::<bool>(), 1..40),
        settings(),
    )
        .prop_map(
            |(old, ops, eol, exists, mode, context, mask, settings)| Case {
                new: edit(&old, &ops, eol),
                old,
                exists,
                mode,
                context,
                mask,
                settings,
            },
        )
}

/// What the target file should hold after applying the selected lines of
/// `diff` to `base`. Applying forward, `base` is the old side and selected
/// lines move it towards the new side. Applying in reverse, it's the other
/// way around.
///
/// Returns `None` if the result would need a line without a trailing newline
/// somewhere other than at the end. That happens when the selection splits a
/// change to the last line, and `build_patch` must refuse it.
fn expected(
    diff: &FileDiff,
    base: &[Line],
    selected: &HashSet<(usize, usize)>,
    reverse: bool,
) -> Option<Vec<Line>> {
    let mut out: Vec<Line> = Vec::new();
    let mut pos = 0;
    for (hi, h) in diff.hunks.iter().enumerate() {
        let (start, len) = if reverse {
            (h.new_start, h.new_len)
        } else {
            (h.old_start, h.old_len)
        };
        // For an empty range, the header names the line before it.
        let first = if len == 0 { start } else { start - 1 } as usize;
        out.extend_from_slice(&base[pos..first]);
        pos = first;
        for (li, l) in h.lines.iter().enumerate() {
            let sel = l.kind != LineKind::Context && selected.contains(&(hi, li));
            let on_base = match l.kind {
                LineKind::Context => true,
                LineKind::Del => !reverse,
                LineKind::Add => reverse,
            };
            if on_base {
                assert_eq!(base[pos].0, l.text, "diff doesn't match the file");
                if !sel {
                    out.push(base[pos].clone());
                }
                pos += 1;
            } else if sel {
                out.push((l.text.clone(), !l.no_eol));
            }
        }
    }
    out.extend_from_slice(&base[pos..]);
    if out.iter().rev().skip(1).any(|(_, eol)| !eol) {
        return None;
    }
    Some(out)
}

fn git_bytes(s: &Scratch, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(s.path())
        .args(args)
        .output()
        .expect("run git");
    out.status.success().then_some(out.stdout)
}

/// The staged content of the file, or `None` if it isn't in the index.
fn index(s: &Scratch) -> Option<String> {
    let listed = git_bytes(s, &["ls-files", "--", PATH]).expect("ls-files");
    if listed.is_empty() {
        return None;
    }
    let blob = git_bytes(s, &["cat-file", "blob", &format!(":{PATH}")]).expect("cat-file");
    Some(String::from_utf8(blob).expect("utf-8"))
}

fn worktree(s: &Scratch) -> Option<String> {
    std::fs::read_to_string(s.file(PATH)).ok()
}

fn join(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|(t, eol)| if *eol { format!("{t}\n") } else { t.clone() })
        .collect()
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Scratch::with(&case.settings);
    // A HEAD commit, so every case has something to compare with.
    s.write("keep.txt", "keep\n");
    let old = matches!(case.exists, Exists::Both | Exists::OnlyOld).then(|| case.old.text());
    let new = matches!(case.exists, Exists::Both | Exists::OnlyNew).then(|| case.new.text());
    if let Some(old) = &old {
        s.write(PATH, old);
    }
    s.commit_all("init");
    match &new {
        Some(new) => s.write(PATH, new),
        None => std::fs::remove_file(s.file(PATH)).expect("remove"),
    }
    if case.mode == ApplyMode::Unstage {
        s.sh(&["add", "-A"]);
    }

    let wd = rt
        .block_on(Workdir::load(&s.git, case.context, 100_000))
        .expect("load");
    let files = match case.mode {
        ApplyMode::Unstage => wd.staged,
        _ => wd.unstaged.into_iter().chain(wd.untracked).collect(),
    };
    let Some(diff) = files.into_iter().find(|f| f.path() == PATH) else {
        // Nothing changed.
        prop_assert_eq!(&old, &new);
        return Ok(());
    };
    let diff = Arc::unwrap_or_clone(diff);

    let mut selected = HashSet::new();
    let mut k = 0;
    for (hi, h) in diff.hunks.iter().enumerate() {
        for li in h.changed_lines() {
            if case.mask[k % case.mask.len()] {
                selected.insert((hi, li));
            }
            k += 1;
        }
    }
    let total = k;
    let is_selected = |h: usize, l: usize| selected.contains(&(h, l));
    let reverse = case.mode != ApplyMode::Stage;

    let patch = build_patch(&diff, &is_selected, reverse);
    if selected.is_empty() {
        prop_assert!(
            matches!(patch, Ok(None)),
            "nothing selected, but got {:?}",
            patch
        );
        return Ok(());
    }
    let base = split(
        if reverse {
            new.as_deref()
        } else {
            old.as_deref()
        }
        .unwrap_or(""),
    );
    let Some(lines) = expected(&diff, &base, &selected, reverse) else {
        prop_assert!(patch.is_err(), "expected a refusal, but got {:?}", patch);
        return Ok(());
    };
    let patch = match patch {
        Ok(Some(p)) => String::from_utf8_lossy(&p).into_owned(),
        other => {
            return Err(TestCaseError::fail(format!(
                "expected a patch, but got {other:?}"
            )));
        }
    };

    // Applying all changes to a file that one side lacks creates or
    // deletes it.
    let full = selected.len() == total;
    let gone = full
        && if reverse {
            diff.is_new()
        } else {
            diff.is_deleted()
        };
    let want = (!gone).then(|| join(&lines));

    let index_before = index(&s);
    let worktree_before = worktree(&s);
    let result = rt.block_on(apply_lines(&s.git, &diff, case.mode, &is_selected));
    prop_assert!(
        result.is_ok(),
        "apply failed: {:?}\npatch:\n{}",
        result,
        patch
    );
    match case.mode {
        ApplyMode::Stage | ApplyMode::Unstage => {
            prop_assert_eq!(index(&s), want, "patch:\n{}", patch);
            prop_assert_eq!(worktree(&s), worktree_before);
        }
        ApplyMode::Discard => {
            prop_assert_eq!(worktree(&s), want, "patch:\n{}", patch);
            prop_assert_eq!(index(&s), index_before);
        }
    }
    Ok(())
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
    #![proptest_config(config(200))]

    #[test]
    fn applying_selected_lines(case in case()) {
        check(&case)?;
    }
}

// Staging in several steps.

const PATHS: [&str; 3] = ["a.txt", "b.txt", "c.txt"];

/// The path of a file, which is always one of `PATHS`.
fn path_of(f: &FileDiff) -> &str {
    f.path().to_str().expect("utf-8")
}

/// How one version of a file comes from the version below it.
#[derive(Debug, Clone)]
enum Layer {
    Absent,
    Same,
    /// An edit of the version below, or the given file if there is none.
    Edit(Vec<(u8, String)>, bool, File),
}

impl Layer {
    fn apply(&self, below: Option<&str>) -> Option<String> {
        match (self, below) {
            (Layer::Absent, _) => None,
            (Layer::Same, below) => below.map(str::to_string),
            (Layer::Edit(ops, eol, _), Some(below)) => Some(edit(&parse(below), ops, *eol).text()),
            (Layer::Edit(_, _, fresh), None) => Some(fresh.text()),
        }
    }
}

fn parse(text: &str) -> File {
    let lines = split(text);
    File {
        eol: lines.last().is_none_or(|(_, eol)| *eol),
        lines: lines.into_iter().map(|(t, _)| t).collect(),
    }
}

/// The three versions of a file. `None` means that it isn't there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Versions {
    head: Option<String>,
    index: Option<String>,
    work: Option<String>,
}

#[derive(Debug, Clone)]
enum Pick {
    /// The whole section that the file is in.
    Section,
    File,
    /// One hunk, counting round if the file has fewer.
    Hunk(usize),
    /// Whether each changed line is selected, repeated as needed.
    Lines(Vec<bool>),
}

#[derive(Debug, Clone)]
enum Step {
    /// Pick a file among those the mode can apply to, counting round, and
    /// apply part or all of it.
    Apply {
        mode: ApplyMode,
        file: usize,
        pick: Pick,
    },
    /// Change a file in the working tree, the way the user might between
    /// two operations.
    Edit { file: usize, layer: Layer },
}

#[derive(Debug, Clone)]
struct Steps {
    /// Whether the repository has a commit.
    born: bool,
    files: Vec<Versions>,
    context: u32,
    steps: Vec<Step>,
    settings: Settings,
}

fn layer() -> impl Strategy<Value = Layer> {
    prop_oneof![
        1 => Just(Layer::Absent),
        2 => Just(Layer::Same),
        5 => (
            prop::collection::vec((0u8..10, line()), 0..25),
            prop::bool::weighted(0.8),
            file(),
        )
            .prop_map(|(ops, eol, fresh)| Layer::Edit(ops, eol, fresh)),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    let mode = prop_oneof![
        Just(ApplyMode::Stage),
        Just(ApplyMode::Unstage),
        Just(ApplyMode::Discard),
    ];
    let pick = prop_oneof![
        1 => Just(Pick::Section),
        2 => Just(Pick::File),
        3 => (0usize..8).prop_map(Pick::Hunk),
        6 => prop::collection::vec(any::<bool>(), 1..40).prop_map(Pick::Lines),
    ];
    prop_oneof![
        8 => (mode, 0usize..PATHS.len(), pick)
            .prop_map(|(mode, file, pick)| Step::Apply { mode, file, pick }),
        2 => (0usize..PATHS.len(), layer()).prop_map(|(file, layer)| Step::Edit { file, layer }),
    ]
}

fn steps() -> impl Strategy<Value = Steps> {
    let versions = (file(), prop::bool::weighted(0.8), layer(), layer());
    (
        prop::bool::weighted(0.85),
        prop::collection::vec(versions, 1..=PATHS.len()),
        0u32..4,
        prop::collection::vec(step(), 1..10),
        settings(),
    )
        .prop_map(|(born, files, context, steps, settings)| Steps {
            files: files
                .into_iter()
                .map(|(head, in_head, index, work)| {
                    let head = (born && in_head).then(|| head.text());
                    let index = index.apply(head.as_deref());
                    let work = work.apply(index.as_deref());
                    Versions { head, index, work }
                })
                .collect(),
            born,
            context,
            steps,
            settings,
        })
}

fn put(s: &Scratch, path: &str, text: Option<&str>) {
    match text {
        Some(text) => s.write(path, text),
        None => {
            let _ = std::fs::remove_file(s.file(path));
        }
    }
}

/// Read blobs such as `HEAD:a.txt` or `:a.txt` with one `git cat-file`.
/// Names that don't resolve give `None`.
fn blobs(s: &Scratch, names: &[String]) -> Vec<Option<String>> {
    let mut child = Command::new("git")
        .current_dir(s.path())
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run git");
    let input: String = names.iter().map(|n| format!("{n}\n")).collect();
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("cat-file");
    // Each blob comes as "<id> blob <size>\n", its contents and a newline,
    // and a missing one as "<name> missing\n".
    let mut rest = out.stdout.as_slice();
    let mut texts = Vec::new();
    for _ in names {
        let end = rest.iter().position(|&b| b == b'\n').expect("header");
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        if header.ends_with(" missing") {
            texts.push(None);
            continue;
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("size");
        texts.push(Some(
            String::from_utf8(rest[..size].to_vec()).expect("utf-8"),
        ));
        rest = &rest[size + 1..];
    }
    texts
}

/// The versions of each file as git and the file system have them.
fn actual(s: &Scratch, n: usize) -> Vec<Versions> {
    let names: Vec<String> = PATHS[..n]
        .iter()
        .flat_map(|p| [format!("HEAD:{p}"), format!(":{p}")])
        .collect();
    let mut texts = blobs(s, &names).into_iter();
    PATHS[..n]
        .iter()
        .map(|p| Versions {
            head: texts.next().flatten(),
            index: texts.next().flatten(),
            work: std::fs::read_to_string(s.file(p)).ok(),
        })
        .collect()
}

fn all_lines(diff: &FileDiff) -> HashSet<(usize, usize)> {
    diff.hunks
        .iter()
        .enumerate()
        .flat_map(|(hi, h)| h.changed_lines().map(move |li| (hi, li)))
        .collect()
}

/// Check one list of `Workdir` against the model. `diffs` should hold the
/// files for which `listed` is true, applying all of a file's diff to `old`
/// should give `new`, and `texts` should hold both versions.
fn check_list(
    name: &str,
    diffs: &[Arc<FileDiff>],
    texts: &[FileTexts],
    model: &[Versions],
    listed: impl Fn(&Versions) -> bool,
    old: impl Fn(&Versions) -> Option<&str>,
    new: impl Fn(&Versions) -> Option<&str>,
) -> Result<(), TestCaseError> {
    let want: Vec<&str> = PATHS
        .iter()
        .zip(model)
        .filter(|(_, v)| listed(v))
        .map(|(p, _)| *p)
        .collect();
    let got: Vec<&str> = diffs.iter().map(|f| path_of(f)).collect();
    prop_assert_eq!(got, want, "the {} files", name);
    for (i, f) in diffs.iter().enumerate() {
        let Some(v) = PATHS.iter().position(|p| *p == f.path()).map(|i| &model[i]) else {
            continue;
        };
        let (old, new) = (old(v), new(v));
        prop_assert_eq!(f.is_new(), old.is_none(), "{} {} is new", name, f.path());
        prop_assert_eq!(
            f.is_deleted(),
            new.is_none(),
            "{} {} is deleted",
            name,
            f.path()
        );
        let applied = expected(f, &split(old.unwrap_or("")), &all_lines(f), false)
            .expect("the new side is a file");
        prop_assert_eq!(
            join(&applied),
            new.unwrap_or(""),
            "{} diff of {}",
            name,
            f.path()
        );
        prop_assert_eq!(texts[i].old.as_deref(), old, "old text of {}", f.path());
        prop_assert_eq!(texts[i].new.as_deref(), new, "new text of {}", f.path());
    }
    Ok(())
}

/// Check that `Workdir` lists the files the model says it should, with
/// diffs and texts that match the model.
fn check_workdir(wd: &Workdir, model: &[Versions]) -> Result<(), TestCaseError> {
    check_list(
        "staged",
        &wd.staged,
        &wd.staged_texts,
        model,
        |v| v.index != v.head,
        |v| v.head.as_deref(),
        |v| v.index.as_deref(),
    )?;
    check_list(
        "unstaged",
        &wd.unstaged,
        &wd.unstaged_texts,
        model,
        |v| v.index.is_some() && v.work != v.index,
        |v| v.index.as_deref(),
        |v| v.work.as_deref(),
    )?;
    check_list(
        "untracked",
        &wd.untracked,
        &wd.untracked_texts,
        model,
        |v| v.index.is_none() && v.work.is_some(),
        |_| None,
        |v| v.work.as_deref(),
    )?;
    prop_assert!(wd.conflicted.is_empty());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sec {
    Staged,
    Unstaged,
    Untracked,
}

/// Run one step against the real repository and the model.
fn run_step(
    rt: &tokio::runtime::Runtime,
    s: &Scratch,
    born: bool,
    wd: &Workdir,
    model: &mut [Versions],
    step: &Step,
) -> Result<(), TestCaseError> {
    let (mode, file, pick) = match step {
        Step::Apply { mode, file, pick } => (*mode, *file, pick),
        Step::Edit { file, layer } => {
            let i = file % model.len();
            let v = &mut model[i];
            v.work = layer.apply(v.work.as_deref());
            put(s, PATHS[i], v.work.as_deref());
            return Ok(());
        }
    };
    let candidates = |mode| -> Vec<(Sec, &Arc<FileDiff>)> {
        match mode {
            ApplyMode::Unstage => wd.staged.iter().map(|f| (Sec::Staged, f)).collect(),
            _ => wd
                .unstaged
                .iter()
                .map(|f| (Sec::Unstaged, f))
                .chain(wd.untracked.iter().map(|f| (Sec::Untracked, f)))
                .collect(),
        }
    };
    // Earlier steps often leave nothing for this mode to apply to. Use the
    // other one then, so that fewer steps do nothing.
    let other = match mode {
        ApplyMode::Unstage => ApplyMode::Stage,
        _ => ApplyMode::Unstage,
    };
    let (mode, candidates) = match candidates(mode) {
        c if !c.is_empty() => (mode, c),
        _ => (other, candidates(other)),
    };
    if candidates.is_empty() {
        return Ok(());
    }
    let (sec, diff) = candidates[file % candidates.len()];
    let index_of = |path: &str| PATHS.iter().position(|p| *p == path).expect("known path");
    let git = &s.git;

    // Whole files and sections go through plain git commands.
    let paths: Vec<&str> = match pick {
        Pick::Section => candidates
            .iter()
            .filter(|(s, _)| *s == sec)
            .map(|(_, f)| path_of(f))
            .collect(),
        Pick::File => vec![path_of(diff)],
        Pick::Hunk(_) | Pick::Lines(_) => Vec::new(),
    };
    if !paths.is_empty() {
        let result = rt.block_on(async {
            match (mode, pick) {
                (ApplyMode::Stage, _) => ops::stage_paths(git, &paths).await,
                (ApplyMode::Unstage, Pick::Section) => ops::unstage_all(git, born).await,
                (ApplyMode::Unstage, _) => ops::unstage_paths(git, &paths, born).await,
                (ApplyMode::Discard, _) if sec == Sec::Untracked => {
                    ops::delete_untracked(git, &paths).await
                }
                (ApplyMode::Discard, _) => ops::discard_paths(git, &paths).await,
            }
        });
        prop_assert!(result.is_ok(), "{:?} failed: {:?}", step, result);
        for p in paths {
            let v = &mut model[index_of(p)];
            match mode {
                ApplyMode::Stage => v.index = v.work.clone(),
                ApplyMode::Unstage => v.index = v.head.clone(),
                ApplyMode::Discard => v.work = v.index.clone(),
            }
        }
        return Ok(());
    }

    // Hunks and lines go through `apply_lines`.
    let mut selected = HashSet::new();
    let mut total = 0;
    match pick {
        Pick::Hunk(k) => {
            if diff.hunks.is_empty() {
                return Ok(());
            }
            let hi = k % diff.hunks.len();
            selected.extend(diff.hunks[hi].changed_lines().map(|li| (hi, li)));
            total = all_lines(diff).len();
        }
        Pick::Lines(mask) => {
            for (hi, h) in diff.hunks.iter().enumerate() {
                for li in h.changed_lines() {
                    if mask[total % mask.len()] {
                        selected.insert((hi, li));
                    }
                    total += 1;
                }
            }
        }
        Pick::Section | Pick::File => unreachable!(),
    }
    let is_selected = |h: usize, l: usize| selected.contains(&(h, l));
    let result = rt.block_on(ops::apply_lines(git, diff, mode, &is_selected));
    if selected.is_empty() {
        prop_assert!(result.is_err(), "applied an empty selection");
        return Ok(());
    }

    // Staging and unstaging change the index, and discarding changes the
    // working tree. Either way, the patch applies to the side of the diff
    // that the target has now.
    let v = &mut model[index_of(path_of(diff))];
    let target = match mode {
        ApplyMode::Stage | ApplyMode::Unstage => &mut v.index,
        ApplyMode::Discard => &mut v.work,
    };
    let reverse = mode != ApplyMode::Stage;
    let base = split(target.as_deref().unwrap_or(""));
    let Some(lines) = expected(diff, &base, &selected, reverse) else {
        prop_assert!(result.is_err(), "expected a refusal, but got {:?}", result);
        return Ok(());
    };
    prop_assert!(result.is_ok(), "{:?} failed: {:?}", step, result);
    let gone = selected.len() == total
        && if reverse {
            diff.is_new()
        } else {
            diff.is_deleted()
        };
    *target = (!gone).then(|| join(&lines));
    Ok(())
}

fn check_steps(case: &Steps) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Scratch::with(&case.settings);
    let n = case.files.len();
    for (p, v) in PATHS.iter().zip(&case.files) {
        put(&s, p, v.head.as_deref());
    }
    if case.born {
        s.commit_all("init");
    }
    for (p, v) in PATHS.iter().zip(&case.files) {
        put(&s, p, v.index.as_deref());
    }
    s.sh(&["add", "-A"]);
    for (p, v) in PATHS.iter().zip(&case.files) {
        put(&s, p, v.work.as_deref());
    }

    let mut model = case.files.clone();
    prop_assert_eq!(&actual(&s, n), &model, "after setting up");
    for (i, step) in case.steps.iter().enumerate() {
        let wd = rt
            .block_on(Workdir::load(&s.git, case.context, 100_000))
            .expect("load");
        check_workdir(&wd, &model)?;
        run_step(&rt, &s, case.born, &wd, &mut model, step)?;
        prop_assert_eq!(&actual(&s, n), &model, "after step {}: {:?}", i, step);
    }
    Ok(())
}

proptest! {
    #![proptest_config(config(100))]

    #[test]
    fn staging_in_steps(case in steps()) {
        check_steps(&case)?;
    }
}
