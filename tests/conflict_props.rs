//! Property tests for resolving conflicts with `take_side`.
//!
//! Each case gives a few paths a random version in the base commit, in
//! `main` (ours) and in `other` (theirs). A version is a text file, which
//! can be executable, a binary file, a symlink, a folder, or nothing. That
//! makes every kind of conflict a merge can leave: both changed, both
//! added, deleted by us or by them, and a file against a symlink or a
//! folder. For the last two, git moves one side to a path like `f~other`.
//!
//! The case merges `other` into `main`. The model is the index that the
//! merge leaves. Taking a side on an unmerged path should give that path
//! the entry git recorded for the side, stage 2 for ours and stage 3 for
//! theirs, in both the index and the working tree. If the side has no
//! entry, it deleted the file, and taking it should delete the file too.

use crate::common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use bstr::{BString, ByteSlice};
use common::Scratch;
use common::settings::{Settings, settings};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::ops;
use tergi::git::workdir::Workdir;
use tergi::util::fs_path;

/// The names the files can have. Some look like options or patterns, so
/// the test also checks that `take_side` passes paths literally.
const NAMES: &[&str] = &["a", "b c", "d*", "-e", "日本"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Version {
    /// A text file with three lines. Versions differ in the middle line,
    /// so two of them conflict when both sides change it.
    Text(u8, bool),
    Binary(u8),
    Link(u8),
    /// A folder with one file in it.
    Folder(u8),
}

fn version() -> impl Strategy<Value = Option<Version>> {
    let v = 0..3u8;
    prop::option::weighted(
        0.8,
        prop_oneof![
            3 => (v.clone(), prop::bool::weighted(0.2)).prop_map(|(v, x)| Version::Text(v, x)),
            1 => v.clone().prop_map(Version::Binary),
            1 => v.clone().prop_map(Version::Link),
            1 => v.prop_map(Version::Folder),
        ],
    )
}

#[derive(Debug, Clone)]
struct File {
    name: &'static str,
    base: Option<Version>,
    ours: Option<Version>,
    theirs: Option<Version>,
}

#[derive(Debug, Clone)]
struct Case {
    files: Vec<File>,
    /// Which side to take for each unmerged path, in order. Repeated as
    /// needed.
    sides: Vec<bool>,
    /// Resolve the paths in reverse order.
    reverse: bool,
    /// Let files with different names have similar contents, so that git
    /// finds renames. Otherwise each file's name is part of its contents.
    /// A rename can make git record a merged version as one side's entry,
    /// rather than the version from that side's branch. Turning renames
    /// off with `-Xno-renames` isn't enough, because git 2.47 still finds
    /// some.
    renames: bool,
    settings: Settings,
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::sample::subsequence(NAMES, 1..=NAMES.len()),
        prop::collection::vec((version(), version(), version()), NAMES.len()),
        prop::collection::vec(any::<bool>(), 1..8),
        any::<bool>(),
        any::<bool>(),
        settings(),
    )
        .prop_map(
            |(names, versions, sides, reverse, renames, settings)| Case {
                files: names
                    .into_iter()
                    .zip(versions)
                    .map(|(name, (base, ours, theirs))| File {
                        name,
                        base,
                        ours,
                        theirs,
                    })
                    .collect(),
                sides,
                reverse,
                renames,
                settings,
            },
        )
}

/// Replace whatever is at `name` with `version`. Unless `renames` is
/// set, the contents include the name, so files with different names
/// are never similar.
fn put(s: &Scratch, name: &str, version: &Option<Version>, renames: bool) {
    let path = s.file(name);
    let tag = if renames { "" } else { name };
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if meta.is_dir() {
            std::fs::remove_dir_all(&path).expect("remove folder");
        } else {
            std::fs::remove_file(&path).expect("remove file");
        }
    }
    match version {
        None => {}
        Some(Version::Text(v, exec)) => {
            std::fs::write(&path, format!("top {tag}\nline {v}\nbottom {tag}\n")).expect("write");
            if *exec {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        Some(Version::Binary(v)) => {
            std::fs::write(&path, format!("\0binary {v} {tag}")).expect("write");
        }
        Some(Version::Link(v)) => {
            std::os::unix::fs::symlink(format!("target {v} {tag}"), &path).expect("symlink");
        }
        Some(Version::Folder(v)) => {
            std::fs::create_dir(&path).expect("mkdir");
            std::fs::write(path.join("inner"), format!("inner {v} {tag}\n")).expect("write");
        }
    }
}

/// An index or tree entry: its mode and object id.
type Entry = (BString, BString);

/// Every entry in the index, by path and stage.
fn index(s: &Scratch) -> BTreeMap<(BString, u8), Entry> {
    let out = s.sh_bytes(&[b"ls-files", b"-s", b"-z"]);
    out.split(|&b| b == 0)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (meta, path) = l.split_once_str(b"\t").expect("tab");
            let mut meta = meta.split(|&b| b == b' ');
            let mode = meta.next().expect("mode");
            let id = meta.next().expect("id");
            let stage = meta.next().expect("stage")[0] - b'0';
            ((path.into(), stage), (mode.into(), id.into()))
        })
        .collect()
}

/// Every file in a commit, by path.
fn tree(s: &Scratch, rev: &str) -> BTreeMap<BString, Entry> {
    let out = s.sh_bytes(&[b"ls-tree", b"-r", b"-z", b"--full-tree", rev.as_bytes()]);
    out.split(|&b| b == 0)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (meta, path) = l.split_once_str(b"\t").expect("tab");
            let mut meta = meta.split(|&b| b == b' ');
            let mode = meta.next().expect("mode");
            let _kind = meta.next();
            let id = meta.next().expect("id");
            (path.into(), (mode.into(), id.into()))
        })
        .collect()
}

/// Check that the working tree has `entry` at `path`, or nothing.
fn check_work(s: &Scratch, path: &BString, entry: Option<&Entry>) -> Result<(), TestCaseError> {
    let fs = s.path().join(fs_path(path));
    let meta = std::fs::symlink_metadata(&fs);
    let Some((mode, id)) = entry else {
        // A folder can be there if other files live in it.
        prop_assert!(
            meta.as_ref().map_or(true, |m| m.is_dir()),
            "{} should be gone",
            path
        );
        return Ok(());
    };
    let meta = meta.map_err(|e| TestCaseError::fail(format!("{path}: {e}")))?;
    let blob = s.sh_bytes(&[b"cat-file", b"blob", id]);
    if mode == "120000" {
        prop_assert!(meta.is_symlink(), "{} should be a symlink", path);
        let target = std::fs::read_link(&fs).expect("read link");
        prop_assert_eq!(target.as_os_str().as_encoded_bytes(), &blob[..], "{}", path);
    } else {
        prop_assert!(meta.is_file(), "{} should be a file", path);
        let exec = meta.permissions().mode() & 0o100 != 0;
        prop_assert_eq!(exec, mode == "100755", "the mode of {}", path);
        prop_assert_eq!(std::fs::read(&fs).expect("read"), blob, "{}", path);
    }
    Ok(())
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = Scratch::with(&case.settings);
    // The base commit and both sides always change something else, so
    // neither side is empty and git always makes a merge.
    s.write("keep", "base\n");
    for f in &case.files {
        put(&s, f.name, &f.base, case.renames);
    }
    s.commit_all("base");
    s.sh(&["checkout", "-q", "-b", "other"]);
    s.write("theirs", "theirs\n");
    for f in &case.files {
        put(&s, f.name, &f.theirs, case.renames);
    }
    s.commit_all("theirs");
    s.sh(&["checkout", "-q", "main"]);
    s.write("ours", "ours\n");
    for f in &case.files {
        put(&s, f.name, &f.ours, case.renames);
    }
    s.commit_all("ours");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // The merge fails when it leaves conflicts, so only its effect counts.
    let _ = rt.block_on(s.git.run(&["merge", "-q", "--no-edit", "other"]));

    let before = index(&s);
    let ours = tree(&s, "main");
    let theirs = tree(&s, "other");
    let mut unmerged: Vec<BString> = Vec::new();
    for (path, stage) in before.keys() {
        if *stage > 0 && unmerged.last() != Some(path) {
            unmerged.push(path.clone());
        }
    }

    // Without renames, each side's entry should be the file from that
    // branch, wherever the branch has a file at that path.
    for ((path, stage), entry) in before.iter().filter(|_| !case.renames) {
        let branch = match stage {
            2 => &ours,
            3 => &theirs,
            _ => continue,
        };
        if let Some(want) = branch.get(path) {
            prop_assert_eq!(entry, want, "stage {} of {}", stage, path);
        }
    }

    let wd = rt
        .block_on(Workdir::load(&s.git, 3, 100_000))
        .map_err(|e| TestCaseError::fail(format!("load: {e:#}")))?;
    let mut listed: Vec<BString> = wd.conflicted.iter().map(|f| f.path().to_owned()).collect();
    listed.sort();
    prop_assert_eq!(&listed, &unmerged, "the conflicted files");

    let mut order: Vec<(BString, bool)> = unmerged
        .iter()
        .cloned()
        .zip(case.sides.iter().copied().cycle())
        .collect();
    if case.reverse {
        order.reverse();
    }
    for (path, ours) in &order {
        rt.block_on(ops::take_side(&s.git, path, *ours))
            .map_err(|e| TestCaseError::fail(format!("take {ours} for {path}: {e:#}")))?;
    }

    let after = index(&s);
    let mut want: BTreeMap<(BString, u8), Entry> = before
        .iter()
        .filter(|((_, stage), _)| *stage == 0)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (path, ours) in &order {
        let stage = if *ours { 2 } else { 3 };
        if let Some(entry) = before.get(&(path.clone(), stage)) {
            want.insert((path.clone(), 0), entry.clone());
        }
    }
    prop_assert_eq!(&after, &want, "the index after taking sides {:?}", order);

    for (path, ours) in &order {
        let stage = if *ours { 2 } else { 3 };
        check_work(&s, path, before.get(&(path.clone(), stage)))?;
    }

    let wd = rt
        .block_on(Workdir::load(&s.git, 3, 100_000))
        .map_err(|e| TestCaseError::fail(format!("load: {e:#}")))?;
    prop_assert!(
        wd.conflicted.is_empty(),
        "still conflicted: {:?}",
        wd.conflicted
    );
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

common::split_proptest! {
    config = config(128);
    parts = [part_1 part_2 part_3 part_4];

    fn taking_a_side_gives_that_side(case in case()) {
        check(&case)?;
    }
}
