//! Operations on scratch repositories that have gone wrong before.

use crate::common;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use bstr::BString;
use common::Scratch;
use tergi::git::ObjectId;
use tergi::git::commit;
use tergi::git::log;
use tergi::git::ops::{self, ApplyMode, apply_lines};
use tergi::git::rebase::{self, RepoState};
use tergi::git::refs::Refs;
use tergi::git::status::Status;
use tergi::git::workdir::Workdir;
use tergi::util::Generation;

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

/// tergi reads messages as UTF-8, so they have to come out that way
/// whatever the user set. Otherwise rewording would put replacement
/// characters into the message.
#[tokio::test]
async fn messages_with_another_log_encoding() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.commit_all("café");
    s.sh(&["config", "i18n.logOutputEncoding", "ISO-8859-1"]);
    let msg = ops::commit_message(&s.git, "HEAD").await.expect("message");
    assert_eq!(msg, "café");
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
    ops::take_side(&s.git, b"f.txt", false)
        .await
        .expect("theirs deletes f");
    assert!(!s.file("f.txt").exists());
    ops::take_side(&s.git, b"g.txt", false)
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
    let load = |rev: &str| {
        let id = ObjectId::from_hex(s.sh(&["rev-parse", rev]).trim().as_bytes()).expect("id");
        common::block_on(commit::load(&s.git, id, 3, usize::MAX)).expect("load")
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

/// Git pairs up empty files as renames.
#[test]
fn an_empty_file_can_be_renamed() {
    let s = Scratch::new();
    s.write("a.txt", "");
    s.commit_all("init");
    s.sh(&["mv", "a.txt", "b.txt"]);
    let head = s.commit_all("move");
    let id = ObjectId::from_hex(head.as_bytes()).expect("id");
    let c = common::block_on(commit::load(&s.git, id, 3, usize::MAX)).expect("load");
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.status.letter(), f.old_path.clone(), f.new_path.clone()))
        .collect();
    assert_eq!(files, [('R', Some("a.txt".into()), Some("b.txt".into()))]);

    // Turning renames off in the config still works, once the app has
    // refreshed.
    s.sh(&["config", "diff.renames", "false"]);
    s.git.stop_processes();
    let c = common::block_on(commit::load(&s.git, id, 3, usize::MAX)).expect("load");
    let letters: Vec<char> = c.files.iter().map(|f| f.status.letter()).collect();
    assert_eq!(letters, ['D', 'A']);
}

/// With `diff.renames = copies`, git looks for the source of a copy among
/// the old versions of the files that the commit changed. The changed
/// file shows up too.
#[test]
fn a_copy_of_a_changed_file() {
    let s = Scratch::new();
    let old = "one\ntwo\nthree\nfour\nfive\n";
    s.write("a.txt", old);
    s.commit_all("init");
    s.write("a.txt", "changed\n");
    s.write("b.txt", old);
    let head = s.commit_all("copy");
    s.sh(&["config", "diff.renames", "copies"]);
    let id = ObjectId::from_hex(head.as_bytes()).expect("id");
    let c = common::block_on(commit::load(&s.git, id, 3, usize::MAX)).expect("load");
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.status.letter(), f.old_path.clone(), f.new_path.clone()))
        .collect();
    assert_eq!(
        files,
        [
            ('M', Some("a.txt".into()), Some("a.txt".into())),
            ('C', Some("a.txt".into()), Some("b.txt".into())),
        ]
    );
    // The copy is diffed against the old `a.txt`, which it equals.
    assert!(c.files[1].hunks.is_empty());
}

/// `commit::load` keeps git running between loads. It still sees
/// commits made after git started, and it sees a config change once
/// the processes are stopped, which the app does on every refresh.
#[tokio::test]
async fn commit_loads_keep_git_running() {
    let s = Scratch::new();
    s.write("a.txt", "one\ntwo\nthree\n");
    s.commit_all("init");
    s.sh(&["mv", "a.txt", "b.txt"]);
    let moved = ObjectId::from_hex(s.commit_all("move").as_bytes()).expect("id");
    let letters = |c: &commit::CommitDetail| -> Vec<char> {
        c.files.iter().map(|f| f.status.letter()).collect()
    };
    let c = commit::load(&s.git, moved, 3, usize::MAX)
        .await
        .expect("load");
    assert_eq!(letters(&c), ['R']);

    s.write("c.txt", "new\n");
    let later = ObjectId::from_hex(s.commit_all("later").as_bytes()).expect("id");
    let c = commit::load(&s.git, later, 3, usize::MAX)
        .await
        .expect("load");
    assert_eq!(letters(&c), ['A']);
    assert_eq!(c.parents, [moved]);

    // The running git read the config before this change.
    s.sh(&["config", "diff.renames", "false"]);
    let c = commit::load(&s.git, moved, 3, usize::MAX)
        .await
        .expect("load");
    assert_eq!(letters(&c), ['R']);
    s.git.stop_processes();
    let c = commit::load(&s.git, moved, 3, usize::MAX)
        .await
        .expect("load");
    assert_eq!(letters(&c), ['D', 'A']);
}

/// In a shallow clone, the oldest commit has parents in its object that
/// the clone doesn't have. Git treats it as a root commit, and so do we.
#[test]
fn a_shallow_commit_shows_as_a_root() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.commit_all("one");
    s.write("b.txt", "b\n");
    let head = s.commit_all("two");
    let clone = tempfile::tempdir().expect("tempdir");
    let url = format!("file://{}", s.path().display());
    let dest = clone.path().to_str().expect("utf-8");
    s.sh(&["clone", "-q", "--depth=1", &url, dest]);
    let git = tergi::git::cli::Git::new(clone.path());
    let id = ObjectId::from_hex(head.as_bytes()).expect("id");
    let c = common::block_on(commit::load(&git, id, 3, usize::MAX)).expect("load");
    assert!(c.parents.is_empty());
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.status.letter(), f.path().to_string()))
        .collect();
    assert_eq!(
        files,
        [('A', "a.txt".to_string()), ('A', "b.txt".to_string())]
    );
}

/// A load that is dropped halfway leaves an answer from git unread. The
/// next load mustn't read it as its own.
#[tokio::test]
async fn a_dropped_commit_load_does_not_confuse_the_next() {
    let s = Scratch::new();
    s.write("small.txt", "a\n");
    let small = ObjectId::from_hex(s.commit_all("small").as_bytes()).expect("id");
    // Big enough that git takes a while to hand over the blobs.
    for i in 0..20 {
        s.write(
            &format!("big{i}.txt"),
            &format!("{i} line\n").repeat(20_000),
        );
    }
    let big = ObjectId::from_hex(s.commit_all("big").as_bytes()).expect("id");
    let summary = |c: &commit::CommitDetail| -> Vec<(String, Option<usize>)> {
        c.files
            .iter()
            .zip(&c.texts)
            .map(|(f, t)| (f.path().to_string(), t.new.as_ref().map(|t| t.len())))
            .collect()
    };
    let load = |id| commit::load(&s.git, id, 3, usize::MAX);
    let want_small = summary(&load(small).await.expect("load"));
    let want_big = summary(&load(big).await.expect("load"));
    for n in 0..40 {
        let wait = std::time::Duration::from_micros(n * 200);
        let _ = tokio::time::timeout(wait, load(big)).await;
        assert_eq!(summary(&load(small).await.expect("load")), want_small);
        let _ = tokio::time::timeout(wait, load(small)).await;
        assert_eq!(summary(&load(big).await.expect("load")), want_big);
    }
}

/// The history and the commit details show names and emails as
/// `.mailmap` maps them, like `git log` does.
#[test]
fn names_go_through_the_mailmap() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.sh(&["add", "-A"]);
    s.sh(&[
        "-c",
        "user.name=Old Name",
        "-c",
        "user.email=old@example.com",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    s.write(".mailmap", "New Name <new@example.com> <old@example.com>\n");
    let head = s.sh(&["rev-parse", "HEAD"]);
    let id = ObjectId::from_hex(head.trim().as_bytes()).expect("id");

    let c = common::block_on(commit::load(&s.git, id, 3, usize::MAX)).expect("load");
    for p in [&c.author, &c.committer] {
        assert_eq!(
            (p.name.as_str(), p.email.as_str()),
            ("New Name", "new@example.com")
        );
    }

    let walks = Generation::default();
    let generation = walks.next();
    let mut commits = Vec::new();
    common::block_on(log::walk(&s.git, &[id], generation, &walks, |b| {
        commits.extend(b.commits);
    }))
    .expect("walk");
    let names: Vec<_> = commits.iter().map(|c| (&*c.author, &*c.email)).collect();
    assert_eq!(names, [("New Name", "new@example.com")]);
}

/// A walk stops as soon as a newer one starts, even while git hasn't
/// printed anything yet. Here git gets stuck because the file that lists
/// other object folders is a pipe that nobody writes to.
#[tokio::test]
async fn a_replaced_walk_stops_while_git_is_silent() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    let head = s.commit_all("init");
    let id = ObjectId::from_hex(head.as_bytes()).expect("id");
    let alternates = s.path().join(".git/objects/info/alternates");
    let made = std::process::Command::new("mkfifo")
        .arg(&alternates)
        .status()
        .expect("mkfifo");
    assert!(made.success());

    let walks = Generation::default();
    let generation = walks.next();
    let mut batches = 0;
    let tips = [id];
    let walk = log::walk(&s.git, &tips, generation, &walks, |_| batches += 1);
    let replace = async {
        // If git didn't get stuck, the walk finishes in this time and
        // sends a batch.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        walks.next();
    };
    let both = async { tokio::join!(walk, replace) };
    let (r, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), both)
        .await
        .expect("the walk didn't stop");
    r.expect("walk");
    assert_eq!(batches, 0);
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

/// The editor setting is a shell command, and it can name a program by a
/// path that isn't UTF-8.
#[tokio::test]
async fn editor_setting_keeps_its_bytes() {
    let s = Scratch::new();
    let editor = OsStr::from_bytes(b"/opt/caf\xe9/edit --wait");
    let status = std::process::Command::new("git")
        .current_dir(s.path())
        .args(["config", "core.editor"])
        .arg(editor)
        .status()
        .expect("run git");
    assert!(status.success());
    assert_eq!(s.git.editor().await.expect("editor"), editor);
}

/// Git allows ref names that aren't UTF-8. Here "caf\xe9" and "caf\xff"
/// look the same once they are text, so mixing them up would act on the
/// wrong branch.
#[tokio::test]
async fn ref_names_that_are_not_utf8() {
    let s = Scratch::new();
    if !s.allow_non_utf8_refs() {
        return;
    }
    s.write("f.txt", "a\n");
    s.commit_all("one");
    s.sh_bytes(&[b"branch", b"caf\xe9"]);
    s.sh_bytes(&[b"branch", b"caf\xff"]);
    s.sh_bytes(&[b"tag", b"t\xe9"]);
    s.sh_bytes(&[b"remote", b"add", b"r\xe9", b"/nowhere"]);
    s.sh_bytes(&[b"update-ref", b"refs/remotes/r\xe9/caf\xe9", b"HEAD"]);

    let refs = Refs::load(&s.git).await.expect("refs");
    let names = |v: Vec<&BString>| v.into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        names(refs.branches.iter().map(|b| &b.name).collect()),
        [&b"caf\xe9"[..], b"caf\xff", b"main"]
    );
    assert_eq!(
        names(refs.tags.iter().map(|t| &t.name).collect()),
        [b"t\xe9"]
    );
    assert_eq!(refs.remotes, [b"r\xe9"]);
    assert_eq!(refs.remote_branches[0].full_name(), b"r\xe9/caf\xe9");
    assert_eq!(
        refs.labels(&Default::default()).values().flatten().count(),
        5
    );

    let head_ref = || s.sh_bytes(&[b"symbolic-ref", b"HEAD"]);
    ops::checkout(&s.git, &refs.branches[1].name)
        .await
        .expect("checkout");
    assert_eq!(head_ref(), b"refs/heads/caf\xff\n");
    assert_eq!(
        ops::head_branch(&s.git).await,
        Some(BString::from(&b"caf\xff"[..]))
    );
    let status = Status::load(&s.git).await.expect("status");
    assert_eq!(status.branch, Some(BString::from(&b"caf\xff"[..])));

    ops::delete_branch(&s.git, &refs.branches[0].name, false)
        .await
        .expect("delete");
    ops::rename_branch(&s.git, &refs.branches[1].name, "renamed")
        .await
        .expect("rename");
    ops::delete_tag(&s.git, &refs.tags[0].name)
        .await
        .expect("delete tag");
    assert_eq!(
        s.sh(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/tags"
        ]),
        "refs/heads/main\nrefs/heads/renamed\n"
    );
    let rb = &refs.remote_branches[0];
    ops::checkout_remote(&s.git, &rb.remote, &rb.name, false)
        .await
        .expect("checkout remote");
    assert_eq!(head_ref(), b"refs/heads/caf\xe9\n");
}

/// Remote `a/b` keeps its branches under `refs/remotes/a/b/`. A local
/// branch `a/b/main` has the same short name as its branch `main`. The
/// ref `refs/remotes/a/c/main` belongs to no remote, so it goes under `a`.
#[tokio::test]
async fn remote_names_with_a_slash() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.commit_all("one");
    s.sh(&["branch", "a/b/main"]);
    s.write("f.txt", "b\n");
    let two = s.commit_all("two");
    s.sh(&["remote", "add", "a/b", "/nowhere"]);
    s.sh(&["update-ref", "refs/remotes/a/b/main", "HEAD"]);
    s.sh(&["update-ref", "refs/remotes/a/c/main", "HEAD~"]);

    let refs = Refs::load(&s.git).await.expect("refs");
    let names: Vec<(BString, BString)> = refs
        .remote_branches
        .iter()
        .map(|rb| (rb.remote.clone(), rb.name.clone()))
        .collect();
    assert_eq!(
        names,
        [("a/b".into(), "main".into()), ("a".into(), "c/main".into())]
    );

    s.sh(&["checkout", "-q", "--detach"]);
    s.sh(&["branch", "-D", "main"]);
    ops::checkout_remote(&s.git, "a/b", "main", false)
        .await
        .expect("checkout remote");
    assert_eq!(s.sh(&["symbolic-ref", "HEAD"]), "refs/heads/main\n");
    assert_eq!(
        s.sh(&["rev-parse", "--symbolic-full-name", "main@{upstream}"]),
        "refs/remotes/a/b/main\n"
    );
    assert_eq!(s.sh(&["rev-parse", "main"]).trim(), two);
}

#[test]
fn rebase_of_a_branch_whose_name_is_not_utf8() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path().join("rebase-merge");
    std::fs::create_dir(&d).expect("mkdir");
    std::fs::write(d.join("head-name"), b"refs/heads/caf\xe9\n").expect("write");
    match rebase::state_in(dir.path()) {
        RepoState::Rebase { head_name, .. } => {
            assert_eq!(head_name, Some(BString::from(&b"caf\xe9"[..])));
        }
        other => panic!("not a rebase: {other:?}"),
    }
}

/// ssh would ask on the terminal, which tergi owns. It has to run the
/// askpass program instead, and that has to fail so ssh gives up. The
/// fake ssh here does what ssh does with `SSH_ASKPASS_REQUIRE=force`.
#[tokio::test]
async fn ssh_never_asks_on_the_terminal() {
    if std::env::var_os("SSH_ASKPASS").is_some() {
        // tergi keeps the user's own askpass program.
        return;
    }
    let s = Scratch::new();
    s.sh(&["remote", "add", "origin", "ssh://example.invalid/repo"]);
    s.sh(&[
        "config",
        "core.sshCommand",
        "fake() { \
           if [ \"$SSH_ASKPASS_REQUIRE\" != force ]; then echo 'would use the terminal' >&2; \
           elif \"$SSH_ASKPASS\" 'Passphrase:' </dev/null; then echo 'askpass answered' >&2; \
           else echo 'askpass refused' >&2; \
           fi; \
           exit 255; \
         }; fake",
    ]);
    let Err(err) = s.git.run(&["fetch", "origin"]).await else {
        panic!("the fetch should fail");
    };
    assert!(format!("{err:#}").contains("askpass refused"), "{err:#}");
}
