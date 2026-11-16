//! Operations on scratch repositories that have gone wrong before.

mod common;

use common::Scratch;
use gix::ObjectId;
use tergi::git::commit;
use tergi::git::ops::{self, ApplyMode, apply_lines};
use tergi::git::workdir::Workdir;

#[tokio::test]
async fn message_lines_starting_with_hash() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.sh(&["add", "-A"]);
    ops::commit(&s.git, "#42 fix it\n\n# Heading\ntext\n", false)
        .await
        .expect("commit");
    assert_eq!(
        s.sh(&["log", "-1", "--format=%B"]).trim(),
        "#42 fix it\n\n# Heading\ntext"
    );
}

#[tokio::test]
async fn paths_that_look_like_patterns() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.write("[ab].txt", "x\n");
    s.commit_all("init");
    s.write("a.txt", "a2\n");
    s.write("[ab].txt", "x2\n");
    ops::stage_paths(&s.git, &["[ab].txt"])
        .await
        .expect("stage");
    assert_eq!(
        s.sh(&["diff", "--cached", "--name-only"]).trim(),
        "[ab].txt"
    );
    ops::unstage_paths(&s.git, &["[ab].txt"], true)
        .await
        .expect("unstage");
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "");
    ops::discard_paths(&s.git, &["[ab].txt"])
        .await
        .expect("discard");
    assert_eq!(s.read("a.txt"), "a2\n");
    assert_eq!(s.read("[ab].txt"), "x\n");

    s.write("b.txt", "new\n");
    s.write("[b].txt", "new\n");
    ops::delete_untracked(&s.git, &["[b].txt"])
        .await
        .expect("clean");
    assert!(s.file("b.txt").exists());
    assert!(!s.file("[b].txt").exists());

    s.write(":colon", "new\n");
    ops::stage_paths(&s.git, &[":colon"])
        .await
        .expect("stage colon");
    assert!(
        s.sh(&["diff", "--cached", "--name-only"])
            .contains(":colon")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn untracked_executable_and_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.write("base", "x\n");
    s.commit_all("init");
    s.write("run.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(s.file("run.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::os::unix::fs::symlink("base", s.file("link")).expect("symlink");
    let wd = Workdir::load(&s.git, 3, 100_000).await.expect("load");
    for f in &wd.untracked {
        apply_lines(&s.git, f, ApplyMode::Stage, &|_, _| true)
            .await
            .expect("stage");
    }
    let ls = s.sh(&["ls-files", "-s"]);
    assert!(
        ls.lines()
            .any(|l| l.starts_with("100755") && l.ends_with("run.sh")),
        "{ls}"
    );
    assert!(
        ls.lines()
            .any(|l| l.starts_with("120000") && l.ends_with("link")),
        "{ls}"
    );
    assert_eq!(
        s.sh(&["status", "--porcelain"]).trim(),
        "A  link\nA  run.sh"
    );
}

#[tokio::test]
async fn take_side_when_one_side_deleted() {
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
    assert!(s.git.run(&["merge", "other"]).await.is_err());
    // f.txt: we changed it, they deleted it. g.txt is the other way round.
    ops::take_side(&s.git, "f.txt", false)
        .await
        .expect("theirs deletes f");
    assert!(!s.file("f.txt").exists());
    ops::take_side(&s.git, "g.txt", false)
        .await
        .expect("theirs keeps g");
    assert_eq!(s.read("g.txt"), "theirs\n");
    assert_eq!(
        s.sh(&["status", "--porcelain"]).trim(),
        "D  f.txt\nA  g.txt"
    );
}

#[tokio::test]
async fn staged_files_before_the_first_commit() {
    let s = Scratch::new();
    s.write("a.txt", "one\ntwo\n");
    s.sh(&["add", "-A"]);
    let wd = Workdir::load(&s.git, 3, 100_000).await.expect("load");
    assert_eq!(wd.staged.len(), 1);
    assert_eq!(wd.staged[0].line_count(), 2);
    apply_lines(&s.git, &wd.staged[0], ApplyMode::Unstage, &|_, l| l == 1)
        .await
        .expect("unstage");
    assert_eq!(s.index("a.txt"), "one\n");
    ops::unstage_paths(&s.git, &["a.txt"], false)
        .await
        .expect("unstage");
    assert_eq!(s.sh(&["status", "--porcelain"]).trim(), "?? a.txt");
}

#[tokio::test]
async fn stash_actions_follow_the_stash_when_others_are_added() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.commit_all("init");
    s.write("f.txt", "one\n");
    s.sh(&["stash", "push", "-m", "one"]);
    s.write("f.txt", "two\n");
    s.sh(&["stash", "push", "-m", "two"]);
    let one =
        ObjectId::from_hex(s.sh(&["rev-parse", "stash@{1}"]).trim().as_bytes()).expect("stash id");

    // Another stash pushes "one" down to stash@{2}.
    s.write("f.txt", "three\n");
    s.sh(&["stash", "push", "-m", "three"]);

    ops::stash_apply(&s.git, one, false).await.expect("apply");
    assert_eq!(s.read("f.txt"), "one\n");
    s.sh(&["checkout", "--", "f.txt"]);

    ops::stash_drop(&s.git, one).await.expect("drop");
    let left = s.sh(&["stash", "list", "--format=%gs"]);
    assert_eq!(
        left.lines().collect::<Vec<_>>(),
        ["On main: three", "On main: two"]
    );

    assert!(ops::stash_drop(&s.git, one).await.is_err());
}

/// The untracked files of a stash live in its third parent, not in the
/// diff against the first one.
#[test]
fn a_stash_shows_its_untracked_files() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.commit_all("init");
    s.write("a.txt", "changed\n");
    s.write("b.txt", "staged\n");
    s.sh(&["add", "b.txt"]);
    s.write("c.txt", "untracked\n");
    s.write("dir/d.txt", "deep\n");
    s.sh(&["stash", "push", "-q", "--include-untracked"]);
    let repo = gix::open(s.path()).expect("open");
    let load = |rev: &str| {
        let id = ObjectId::from_hex(s.sh(&["rev-parse", rev]).trim().as_bytes()).expect("id");
        commit::load(&repo, id, 3, usize::MAX).expect("load")
    };
    let files = |c: &commit::CommitDetail| -> Vec<(char, String)> {
        c.files
            .iter()
            .map(|f| (f.status.letter(), f.path().to_string()))
            .collect()
    };

    let c = load("stash@{0}");
    assert_eq!(
        files(&c),
        [
            ('M', "a.txt".to_string()),
            ('A', "b.txt".to_string()),
            ('?', "c.txt".to_string()),
            ('?', "dir/d.txt".to_string()),
        ]
    );
    assert_eq!(c.texts[2].new.as_deref(), Some("untracked\n"));

    // A stash without untracked files has only two parents.
    s.sh(&["stash", "pop", "-q"]);
    s.sh(&["stash", "push", "-q"]);
    let c = load("stash@{0}");
    assert_eq!(
        files(&c),
        [('M', "a.txt".to_string()), ('A', "b.txt".to_string())]
    );
}

/// Git pairs up empty files as renames, but gix leaves them out unless
/// asked.
#[test]
fn an_empty_file_can_be_renamed() {
    let s = Scratch::new();
    s.write("a.txt", "");
    s.commit_all("init");
    s.sh(&["mv", "a.txt", "b.txt"]);
    let head = s.commit_all("move");
    let repo = gix::open(s.path()).expect("open");
    let id = ObjectId::from_hex(head.as_bytes()).expect("id");
    let c = commit::load(&repo, id, 3, usize::MAX).expect("load");
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.status.letter(), f.old_path.clone(), f.new_path.clone()))
        .collect();
    assert_eq!(
        files,
        [('R', Some("a.txt".to_string()), Some("b.txt".to_string()))]
    );

    // Turning renames off in the config still works.
    s.sh(&["config", "diff.renames", "false"]);
    let repo = gix::open(s.path()).expect("open");
    let c = commit::load(&repo, id, 3, usize::MAX).expect("load");
    let letters: Vec<char> = c.files.iter().map(|f| f.status.letter()).collect();
    assert_eq!(letters, ['D', 'A']);
}

#[tokio::test]
async fn ref_names_that_look_like_options() {
    let s = Scratch::new();
    s.write("base.txt", "a\n");
    let one = s.commit_all("one");
    s.write("other.txt", "b\n");
    s.commit_all("two");
    // A remote can hand us a tag like this. As an option, `-f` would make
    // checkout throw away local changes.
    s.sh(&["update-ref", "refs/tags/-f", &one]);
    s.write("base.txt", "dirty\n");
    ops::create_branch(&s.git, "b", "-f", true)
        .await
        .expect("branch from tag");
    assert_eq!(s.sh(&["rev-parse", "HEAD"]).trim(), one);
    assert_eq!(s.read("base.txt"), "dirty\n");
    ops::checkout(&s.git, "main").await.expect("checkout");
    ops::merge(&s.git, "-f").await.expect("merge tag");
    ops::delete_tag(&s.git, "-f").await.expect("delete tag");
    assert_eq!(s.sh(&["tag", "-l"]), "");
    assert_eq!(s.read("base.txt"), "dirty\n");
}
