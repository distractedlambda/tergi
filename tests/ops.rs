//! Operations on scratch repositories that have gone wrong before.

mod common;

use common::Scratch;
use tergi::git::ops::{self, ApplyMode, apply_lines};
use tergi::git::workdir::Workdir;

#[test]
fn message_lines_starting_with_hash() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.sh(&["add", "-A"]);
    ops::commit(&s.git, "#42 fix it\n\n# Heading\ntext\n", false).expect("commit");
    assert_eq!(s.sh(&["log", "-1", "--format=%B"]).trim(), "#42 fix it\n\n# Heading\ntext");
}

#[test]
fn paths_that_look_like_patterns() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.write("[ab].txt", "x\n");
    s.commit_all("init");
    s.write("a.txt", "a2\n");
    s.write("[ab].txt", "x2\n");
    ops::stage_paths(&s.git, &["[ab].txt"]).expect("stage");
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "[ab].txt");
    ops::unstage_paths(&s.git, &["[ab].txt"], true).expect("unstage");
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "");
    ops::discard_paths(&s.git, &["[ab].txt"]).expect("discard");
    assert_eq!(s.read("a.txt"), "a2\n");
    assert_eq!(s.read("[ab].txt"), "x\n");

    s.write("b.txt", "new\n");
    s.write("[b].txt", "new\n");
    ops::delete_untracked(&s.git, &["[b].txt"]).expect("clean");
    assert!(s.file("b.txt").exists());
    assert!(!s.file("[b].txt").exists());

    s.write(":colon", "new\n");
    ops::stage_paths(&s.git, &[":colon"]).expect("stage colon");
    assert!(s.sh(&["diff", "--cached", "--name-only"]).contains(":colon"));
}

#[cfg(unix)]
#[test]
fn untracked_executable_and_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.write("base", "x\n");
    s.commit_all("init");
    s.write("run.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(s.file("run.sh"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("base", s.file("link")).expect("symlink");
    let wd = Workdir::load(&s.git, 3, 100_000).expect("load");
    for f in &wd.untracked {
        apply_lines(&s.git, f, ApplyMode::Stage, &|_, _| true).expect("stage");
    }
    let ls = s.sh(&["ls-files", "-s"]);
    assert!(ls.lines().any(|l| l.starts_with("100755") && l.ends_with("run.sh")), "{ls}");
    assert!(ls.lines().any(|l| l.starts_with("120000") && l.ends_with("link")), "{ls}");
    assert_eq!(s.sh(&["status", "--porcelain"]).trim(), "A  link\nA  run.sh");
}

#[test]
fn take_side_when_one_side_deleted() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.write("g.txt", "a\n");
    s.commit_all("init");
    s.sh(&["checkout", "-q", "-b", "other"]);
    s.sh(&["rm", "-q", "f.txt"]);
    s.write("g.txt", "theirs\n");
    s.commit_all("delete f");
    s.sh(&["checkout", "-q", "main"]);
    s.write("f.txt", "ours\n");
    s.sh(&["rm", "-q", "g.txt"]);
    s.commit_all("change f");
    assert!(s.git.run(&["merge", "other"]).is_err());
    // f.txt: we changed it, they deleted it. g.txt is the other way round.
    ops::take_side(&s.git, "f.txt", false).expect("theirs deletes f");
    assert!(!s.file("f.txt").exists());
    ops::take_side(&s.git, "g.txt", false).expect("theirs keeps g");
    assert_eq!(s.read("g.txt"), "theirs\n");
    assert_eq!(s.sh(&["status", "--porcelain"]).trim(), "D  f.txt\nA  g.txt");
}

#[test]
fn staged_files_before_the_first_commit() {
    let s = Scratch::new();
    s.write("a.txt", "one\ntwo\n");
    s.sh(&["add", "-A"]);
    let wd = Workdir::load(&s.git, 3, 100_000).expect("load");
    assert_eq!(wd.staged.len(), 1);
    assert_eq!(wd.staged[0].line_count(), 2);
    apply_lines(&s.git, &wd.staged[0], ApplyMode::Unstage, &|_, l| l == 1).expect("unstage");
    assert_eq!(s.index("a.txt"), "one\n");
    ops::unstage_paths(&s.git, &["a.txt"], false).expect("unstage");
    assert_eq!(s.sh(&["status", "--porcelain"]).trim(), "?? a.txt");
}
