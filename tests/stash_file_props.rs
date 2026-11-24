//! Property tests for stashing one file, run against the real git.
//!
//! Each case makes the files of `path_props`, with odd names and random
//! versions in HEAD, the index and the working tree. Then it stashes one
//! of them with `ops::stash_file` and checks that:
//!
//! - every other file is left as it was, in the index and on disk,
//! - the stashed file is back to its version in HEAD,
//! - the stash commits hold that one file as it was staged, as it was on
//!   disk, and as an untracked file, and nothing else that differs from
//!   HEAD,
//! - applying the stash puts the file back on disk.
//!
//! A file without changes can't be stashed, and then nothing changes.

use crate::common;
use crate::path_props::{self, Versions};

use std::collections::BTreeMap;

use bstr::{BString, ByteSlice as _};
use common::Scratch;
use common::settings::{Settings, settings};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::ObjectId;
use tergi::git::ops;

#[derive(Debug, Clone)]
struct Case {
    names: Vec<BString>,
    files: Vec<Versions>,
    /// The file to stash, modulo the number of files.
    pick: usize,
    message: Option<&'static str>,
    settings: Settings,
}

fn case() -> impl Strategy<Value = Case> {
    (
        path_props::files(),
        0..8usize,
        prop::option::of(prop::sample::select(vec!["wip", "  -m tricky  "])),
        settings(),
    )
        .prop_map(|((names, files), pick, message, settings)| Case {
            names,
            files,
            pick,
            message,
            settings,
        })
}

/// Mode and blob id of each file on disk, as git would store it. A file
/// that isn't there is left out.
fn work_entries(s: &Scratch, names: &[BString]) -> BTreeMap<BString, (String, String)> {
    names
        .iter()
        .filter_map(|n| {
            let path = path_props::file(s, n);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            let mode = if meta.is_symlink() {
                "120000"
            } else if std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o100 != 0 {
                "100755"
            } else {
                "100644"
            };
            Some((
                n.clone(),
                (
                    mode.to_string(),
                    path_props::hash(s, &path_props::disk(s, n)),
                ),
            ))
        })
        .collect()
}

/// `base` with the entry for `path` replaced, or removed if `entry` is
/// `None`.
fn with(
    base: &BTreeMap<BString, (String, String)>,
    path: &BString,
    entry: Option<&(String, String)>,
) -> BTreeMap<BString, (String, String)> {
    let mut m = base.clone();
    match entry {
        Some(e) => m.insert(path.clone(), e.clone()),
        None => m.remove(path),
    };
    m
}

fn rev(s: &Scratch, spec: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(s.path())
        .args(["rev-parse", "-q", "--verify", spec])
        .output()
        .expect("run git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = path_props::make_repo(&case.settings, &case.names, &case.files);
    let names = &case.names;
    let pick = &names[case.pick % names.len()];

    let head = path_props::entries(&s, Some("HEAD"));
    let index = path_props::entries(&s, None);
    let work = work_entries(&s, names);
    let untracked = !index.contains_key(pick) && work.contains_key(pick);
    let changed = head.get(pick) != index.get(pick) || untracked || {
        index.contains_key(pick) && index.get(pick) != work.get(pick)
    };

    let message = case.message.unwrap_or("");
    let result = common::block_on(ops::stash_file(&s.git, pick, message));
    if !changed {
        prop_assert!(result.is_err(), "stashing unchanged {:?} should fail", pick);
        prop_assert_eq!(rev(&s, "refs/stash"), None, "no stash for {:?}", pick);
        prop_assert_eq!(path_props::entries(&s, None), index);
        prop_assert_eq!(work_entries(&s, names), work);
        return Ok(());
    }
    prop_assert!(result.is_ok(), "stashing {:?} failed: {:?}", pick, result);

    // The other files are as they were, and the stashed one is as in HEAD.
    prop_assert_eq!(
        path_props::entries(&s, None),
        with(&index, pick, head.get(pick)),
        "index after stashing {:?}",
        pick
    );
    prop_assert_eq!(
        work_entries(&s, names),
        with(&work, pick, head.get(pick)),
        "working tree after stashing {:?}",
        pick
    );

    // The stash commits differ from HEAD only in the stashed file.
    let head_id = rev(&s, "HEAD");
    prop_assert_eq!(rev(&s, "stash@{0}^1"), head_id);
    prop_assert_eq!(
        path_props::entries(&s, Some("stash@{0}^2")),
        with(&head, pick, index.get(pick)),
        "index commit of the stash of {:?}",
        pick
    );
    // An untracked file isn't in the working tree commit, like the index.
    let on_disk = work.get(pick).filter(|_| index.contains_key(pick));
    prop_assert_eq!(
        path_props::entries(&s, Some("stash@{0}")),
        with(&head, pick, on_disk),
        "working tree commit of the stash of {:?}",
        pick
    );
    if untracked {
        let mut want = BTreeMap::new();
        want.insert(pick.clone(), work[pick].clone());
        prop_assert_eq!(
            path_props::entries(&s, Some("stash@{0}^3")),
            want,
            "untracked commit of the stash of {:?}",
            pick
        );
    } else {
        prop_assert_eq!(rev(&s, "stash@{0}^3"), None, "{:?} isn't untracked", pick);
    }

    // The stash is named after the message, or the file, with each run of
    // whitespace made one space.
    let commit = path_props::git(&s, &["cat-file", "commit", "stash@{0}"]);
    let (_, subject) = commit.split_once_str("\n\n").expect("message");
    let want = match message.trim() {
        "" => pick.to_str_lossy(),
        m => m.into(),
    };
    let want = want.split_whitespace().collect::<Vec<_>>().join(" ");
    let want = format!("On main: {want}\n");
    prop_assert_eq!(subject.as_bstr(), want.as_bytes().as_bstr());

    // Git can't bring back an untracked file over the version that HEAD
    // has, and that's what is on disk now. `git stash -u` leaves the
    // same kind of stash when a staged deletion is still on disk.
    if untracked && head.contains_key(pick) {
        return Ok(());
    }
    let id = ObjectId::from_hex(rev(&s, "refs/stash").expect("stash").as_bytes()).expect("id");
    let applied = common::block_on(ops::stash_apply(&s.git, id, true));
    prop_assert!(applied.is_ok(), "popping {:?} failed: {:?}", pick, applied);
    prop_assert_eq!(
        work_entries(&s, names),
        work,
        "working tree after popping {:?}",
        pick
    );
    Ok(())
}

common::split_proptest! {
    config = path_props::config(100);
    parts = [part_1 part_2 part_3 part_4];

    fn stash_one_file(case in case()) {
        check(&case)?;
    }
}
