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
