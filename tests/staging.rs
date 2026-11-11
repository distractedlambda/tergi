mod common;

use common::Scratch;
use tergi::git::diff::{FileDiff, LineKind};
use tergi::git::ops::{ApplyMode, apply_lines};
use tergi::git::workdir::Workdir;

fn unstaged(s: &Scratch, path: &str) -> FileDiff {
    let wd = Workdir::load(&s.git, 3, 100_000).expect("load");
    wd.unstaged
        .into_iter()
        .chain(wd.untracked)
        .find(|f| f.path() == path)
        .expect("file in diff")
}

fn staged(s: &Scratch, path: &str) -> FileDiff {
    let wd = Workdir::load(&s.git, 3, 100_000).expect("load");
    wd.staged.into_iter().find(|f| f.path() == path).expect("file in diff")
}

/// Select changed lines whose text is in `texts`.
fn select<'a>(f: &'a FileDiff, texts: &'a [&str]) -> impl Fn(usize, usize) -> bool + 'a {
    move |h, l| {
        let line = &f.hunks[h].lines[l];
        line.kind != LineKind::Context && texts.contains(&line.text.as_str())
    }
}

#[test]
fn stage_single_added_line() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nx\nb\ny\nc\n");
    let f = unstaged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["y"])).expect("stage");
    assert_eq!(s.index("f.txt"), "a\nb\ny\nc\n");
    assert_eq!(s.read("f.txt"), "a\nx\nb\ny\nc\n");
}

#[test]
fn stage_part_of_a_replacement() {
    let s = Scratch::new();
    s.write("f.txt", "1\n2\n3\n4\n5\n");
    s.commit_all("init");
    s.write("f.txt", "1\nTWO\nTHREE\n4\n5\n");
    let f = unstaged(&s, "f.txt");
    // Replace only "3" with "THREE".
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["3", "THREE"])).expect("stage");
    assert_eq!(s.index("f.txt"), "1\n2\nTHREE\n4\n5\n");
}

#[test]
fn stage_lines_across_hunks() {
    let s = Scratch::new();
    let base: String = (1..=30).map(|i| format!("{i}\n")).collect();
    s.write("f.txt", &base);
    s.commit_all("init");
    let changed = base.replacen("2\n", "two\n", 1).replace("\n25\n", "\n25\nnew\n");
    s.write("f.txt", &changed);
    let f = unstaged(&s, "f.txt");
    assert_eq!(f.hunks.len(), 2);
    // Only the second hunk's addition. The first hunk's offset must not matter.
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["new"])).expect("stage");
    assert_eq!(s.index("f.txt"), base.replace("\n25\n", "\n25\nnew\n"));
}

#[test]
fn unstage_some_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nB\nc\nd\n");
    s.sh(&["add", "f.txt"]);
    let f = staged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Unstage, &select(&f, &["d"])).expect("unstage");
    assert_eq!(s.index("f.txt"), "a\nB\nc\n");
    // The working tree is untouched.
    assert_eq!(s.read("f.txt"), "a\nB\nc\nd\n");
}

#[test]
fn discard_some_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nB\nc\nd\n");
    let f = unstaged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Discard, &select(&f, &["b", "B"])).expect("discard");
    assert_eq!(s.read("f.txt"), "a\nb\nc\nd\n");
}

#[test]
fn stage_part_of_untracked_file() {
    let s = Scratch::new();
    s.write("keep.txt", "x\n");
    s.commit_all("init");
    s.write("new.txt", "one\ntwo\nthree\n");
    let f = unstaged(&s, "new.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["one", "three"])).expect("stage");
    assert_eq!(s.index("new.txt"), "one\nthree\n");
}

#[test]
fn unstage_part_of_new_file() {
    let s = Scratch::new();
    s.write("keep.txt", "x\n");
    s.commit_all("init");
    s.write("new.txt", "one\ntwo\nthree\n");
    s.sh(&["add", "new.txt"]);
    let f = staged(&s, "new.txt");
    apply_lines(&s.git, &f, ApplyMode::Unstage, &select(&f, &["two"])).expect("unstage");
    assert_eq!(s.index("new.txt"), "one\nthree\n");
}

#[test]
fn stage_part_of_deletion() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    std::fs::remove_file(s.file("f.txt")).expect("rm");
    let f = unstaged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["b"])).expect("stage");
    assert_eq!(s.index("f.txt"), "a\nc\n");
}

#[test]
fn stage_whole_deletion_through_patch() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\n");
    s.write("g.txt", "keep\n");
    s.commit_all("init");
    std::fs::remove_file(s.file("f.txt")).expect("rm");
    let f = unstaged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &|_, _| true).expect("stage");
    let out = s.sh(&["diff", "--cached", "--name-status"]);
    assert_eq!(out.trim(), "D\tf.txt");
}

#[test]
fn no_newline_at_end() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb");
    s.commit_all("init");
    s.write("f.txt", "a\nb\nc");
    let f = unstaged(&s, "f.txt");
    // Taking the whole change at the end of the file works.
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["b", "c"])).expect("stage");
    assert_eq!(s.index("f.txt"), "a\nb\nc");
}

#[test]
fn splitting_last_line_change_is_refused() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb");
    s.commit_all("init");
    s.write("f.txt", "a\nb\nc");
    let f = unstaged(&s, "f.txt");
    // Selecting only "+b" (with newline) but not "-b" (without) can't work.
    let only_add_b = |h: usize, l: usize| {
        let line = &f.hunks[h].lines[l];
        line.kind == LineKind::Add && line.text == "b"
    };
    assert!(apply_lines(&s.git, &f, ApplyMode::Stage, &only_add_b).is_err());
}

#[test]
fn crlf_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\r\nb\r\nc\r\n");
    s.commit_all("init");
    s.write("f.txt", "a\r\nB\r\nc\r\nd\r\n");
    let f = unstaged(&s, "f.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["d\r"])).expect("stage");
    assert_eq!(s.index("f.txt"), "a\r\nb\r\nc\r\nd\r\n");
}

#[test]
fn paths_with_spaces_and_unicode() {
    let s = Scratch::new();
    s.write("dir with space/fïle.txt", "a\n");
    s.commit_all("init");
    s.write("dir with space/fïle.txt", "a\nb\nc\n");
    let f = unstaged(&s, "dir with space/fïle.txt");
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["c"])).expect("stage");
    assert_eq!(s.index("dir with space/fïle.txt"), "a\nc\n");
}

#[test]
fn every_single_line_of_a_mixed_hunk() {
    // Stage each changed line on its own, from a fresh state, and check the
    // index matches what we expect from applying just that line.
    let s = Scratch::new();
    let old = "1\n2\n3\n4\n5\n6\n";
    let new = "1\nX\n3\nY\nZ\n6\n7\n";
    s.write("f.txt", old);
    s.commit_all("init");
    s.write("f.txt", new);
    let f = unstaged(&s, "f.txt");
    let changed: Vec<(usize, usize)> = f
        .hunks
        .iter()
        .enumerate()
        .flat_map(|(h, hunk)| hunk.changed_lines().map(move |l| (h, l)))
        .collect();
    for &(h, l) in &changed {
        s.sh(&["reset", "-q"]);
        let line = f.hunks[h].lines[l].clone();
        apply_lines(&s.git, &f, ApplyMode::Stage, &|a, b| (a, b) == (h, l)).expect("stage");
        let idx = s.index("f.txt");
        match line.kind {
            LineKind::Del => assert!(!idx.lines().any(|x| x == line.text), "{idx:?}"),
            LineKind::Add => assert!(idx.lines().any(|x| x == line.text), "{idx:?}"),
            LineKind::Context => unreachable!(),
        }
        // Everything not touched stays as it was.
        assert_eq!(idx.lines().count(), old.lines().count() + usize::from(line.kind == LineKind::Add) - usize::from(line.kind == LineKind::Del));
    }
}
