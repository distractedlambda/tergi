//! Property tests for staging, unstaging and discarding selected lines.
//!
//! Each case makes a scratch repository with a random file and a random edit
//! to it, picks random changed lines, and runs them through `apply_lines`
//! with the real `git apply`. A small model works out what the file should
//! hold afterwards, straight from the diff and the selection.

mod common;

use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;

use common::Scratch;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::diff::{FileDiff, LineKind};
use tergi::git::ops::{ApplyMode, apply_lines};
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
    )
        .prop_map(|(old, ops, eol, exists, mode, context, mask)| Case {
            new: edit(&old, &ops, eol),
            old,
            exists,
            mode,
            context,
            mask,
        })
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
    let s = Scratch::new();
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
