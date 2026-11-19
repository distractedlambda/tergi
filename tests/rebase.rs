use crate::common;

use std::path::PathBuf;

use common::Scratch;
use gix::ObjectId;
use tergi::git::rebase::{self, Action, Outcome, RepoState};
use tergi::util::fs_path;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tergi"))
}

fn id(s: &str) -> ObjectId {
    ObjectId::from_hex(s.as_bytes()).expect("id")
}

/// A repo with commits "one" to "four", each adding a file.
fn four() -> (Scratch, Vec<String>) {
    let s = Scratch::new();
    let mut ids = Vec::new();
    for name in ["one", "two", "three", "four"] {
        s.write(&format!("{name}.txt"), &format!("{name}\n"));
        ids.push(s.commit_all(name));
    }
    (s, ids)
}

#[tokio::test]
async fn reword_head_and_older() {
    let (s, ids) = four();
    rebase::reword(&s.git, &exe(), id(&ids[3]), "FOUR")
        .await
        .expect("reword head");
    assert_eq!(s.log(), ["FOUR", "three", "two", "one"]);
    rebase::reword(&s.git, &exe(), id(&ids[1]), "TWO\n\nbody")
        .await
        .expect("reword");
    assert_eq!(s.log(), ["FOUR", "three", "TWO", "one"]);
    assert_eq!(
        s.sh(&["log", "-1", "--format=%B", "HEAD~2"]).trim(),
        "TWO\n\nbody"
    );
}

#[tokio::test]
async fn reword_keeps_lines_starting_with_hash() {
    let (s, ids) = four();
    rebase::reword(&s.git, &exe(), id(&ids[3]), "#4 four\n\n# not a comment")
        .await
        .expect("reword head");
    rebase::reword(&s.git, &exe(), id(&ids[1]), "#2 two\n\n# not a comment")
        .await
        .expect("reword");
    assert_eq!(s.log(), ["#4 four", "three", "#2 two", "one"]);
    assert_eq!(
        s.sh(&["log", "-1", "--format=%B", "HEAD~2"]).trim(),
        "#2 two\n\n# not a comment"
    );
}

/// Git refuses an empty message, which would stop the rebase partway, and
/// it can't leave a branch with no commits.
#[tokio::test]
async fn refuses_plans_git_cannot_finish() {
    let (s, ids) = four();
    assert!(
        rebase::reword(&s.git, &exe(), id(&ids[1]), " \n\n")
            .await
            .is_err()
    );
    assert!(
        rebase::single(&s.git, &exe(), id(&ids[2]), Action::Squash("\n".into()))
            .await
            .is_err()
    );
    let mut plan = rebase::plan_from(&s.git, id(&ids[0])).await.expect("plan");
    for step in &mut plan.steps {
        step.action = Action::Drop;
    }
    assert!(rebase::run(&s.git, &plan, &exe()).await.is_err());
    assert_eq!(s.log(), ["four", "three", "two", "one"]);
    assert_eq!(rebase::state(&s.git).await, RepoState::Clean);
}

/// Git runs the message files and tergi itself by their paths, through
/// the shell. Here both paths hold bytes that aren't UTF-8: the repository
/// is in a folder named "café" in Latin-1, and tergi runs from another one.
#[tokio::test]
async fn reword_where_paths_are_not_utf8() {
    if !common::non_utf8_file_names() {
        eprintln!("skipped: this file system rejects names that aren't UTF-8");
        return;
    }
    let s = Scratch::in_folder(b"caf\xe9");
    let mut ids = Vec::new();
    for name in ["one", "two", "three"] {
        s.write(&format!("{name}.txt"), &format!("{name}\n"));
        ids.push(s.commit_all(name));
    }
    let bin = tempfile::tempdir().expect("tempdir");
    let folder = bin.path().join(fs_path(b"bin\xff"));
    std::fs::create_dir(&folder).expect("mkdir");
    let exe = folder.join("tergi");
    std::os::unix::fs::symlink(self::exe(), &exe).expect("symlink");

    rebase::reword(&s.git, &exe, id(&ids[0]), "ONE")
        .await
        .expect("reword");
    // The reword gave "three" a new id.
    let head = s.sh(&["rev-parse", "HEAD"]);
    rebase::single(
        &s.git,
        &exe,
        id(head.trim()),
        Action::Squash("two and three".into()),
    )
    .await
    .expect("squash");
    assert_eq!(s.log(), ["two and three", "ONE"]);
    assert_eq!(rebase::state(&s.git).await, RepoState::Clean);
    assert!(!s.file(".git/tergi").exists(), "message files are removed");
}

#[tokio::test]
async fn reword_root() {
    let (s, ids) = four();
    rebase::reword(&s.git, &exe(), id(&ids[0]), "ONE")
        .await
        .expect("reword root");
    assert_eq!(s.log(), ["four", "three", "two", "ONE"]);
}

#[tokio::test]
async fn drop_commit() {
    let (s, ids) = four();
    let r = rebase::single(&s.git, &exe(), id(&ids[1]), Action::Drop)
        .await
        .expect("drop");
    assert_eq!(r, Outcome::Done);
    assert_eq!(s.log(), ["four", "three", "one"]);
    assert!(!s.file("two.txt").exists());
}

#[tokio::test]
async fn fixup_and_squash_into_parent() {
    let (s, ids) = four();
    rebase::single(&s.git, &exe(), id(&ids[2]), Action::Fixup)
        .await
        .expect("fixup");
    assert_eq!(s.log(), ["four", "two", "one"]);
    let files = s.sh(&["show", "--name-only", "--format=", "HEAD~1"]);
    assert!(files.contains("three.txt") && files.contains("two.txt"));

    let head = s.sh(&["rev-parse", "HEAD"]);
    rebase::single(
        &s.git,
        &exe(),
        id(head.trim()),
        Action::Squash("two and four".into()),
    )
    .await
    .expect("squash");
    assert_eq!(s.log(), ["two and four", "one"]);
}

#[tokio::test]
async fn move_commits() {
    let (s, ids) = four();
    rebase::move_commit(&s.git, &exe(), id(&ids[1]), true)
        .await
        .expect("move up");
    assert_eq!(s.log(), ["four", "two", "three", "one"]);
    let three = s.sh(&["rev-parse", "HEAD~2"]);
    rebase::move_commit(&s.git, &exe(), id(three.trim()), false)
        .await
        .expect("move down");
    assert_eq!(s.log(), ["four", "two", "one", "three"]);
}

#[tokio::test]
async fn edit_stops_and_continues() {
    let (s, ids) = four();
    let r = rebase::single(&s.git, &exe(), id(&ids[1]), Action::Edit)
        .await
        .expect("edit");
    assert_eq!(r, Outcome::Stopped);
    assert!(matches!(
        rebase::state(&s.git).await,
        RepoState::Rebase { .. }
    ));
    s.write("two.txt", "two changed\n");
    s.sh(&["commit", "-q", "-a", "--amend", "--no-edit"]);
    let r = rebase::continue_rebase(&s.git).await.expect("continue");
    assert_eq!(r, Outcome::Done);
    assert_eq!(rebase::state(&s.git).await, RepoState::Clean);
    assert_eq!(s.read("two.txt"), "two changed\n");
    assert_eq!(s.log(), ["four", "three", "two", "one"]);
}

/// The message files are only needed while the rebase runs.
#[tokio::test]
async fn message_files_are_removed_when_the_rebase_ends() {
    let (s, ids) = four();
    let scratch = s.file(".git/tergi");
    let mut plan = rebase::plan_from(&s.git, id(&ids[1])).await.expect("plan");
    plan.steps[0].action = Action::Reword("TWO".into());
    plan.steps[1].action = Action::Edit;
    plan.steps[2].action = Action::Squash("three and four".into());
    let r = rebase::run(&s.git, &plan, &exe()).await.expect("run");
    assert_eq!(r, Outcome::Stopped);
    assert!(scratch.join("msg-2").exists(), "the squash still needs it");
    let r = rebase::continue_rebase(&s.git).await.expect("continue");
    assert_eq!(r, Outcome::Done);
    assert_eq!(s.log(), ["three and four", "TWO", "one"]);
    assert!(!scratch.exists());

    let (s, ids) = four();
    let scratch = s.file(".git/tergi");
    let mut plan = rebase::plan_from(&s.git, id(&ids[1])).await.expect("plan");
    plan.steps[0].action = Action::Edit;
    plan.steps[1].action = Action::Reword("THREE".into());
    rebase::run(&s.git, &plan, &exe()).await.expect("run");
    assert!(scratch.exists());
    rebase::abort_rebase(&s.git).await.expect("abort");
    assert!(!scratch.exists());
}

#[tokio::test]
async fn planner_style_plan() {
    let (s, ids) = four();
    let mut plan = rebase::plan_from(&s.git, id(&ids[1])).await.expect("plan");
    assert_eq!(plan.steps.len(), 3);
    // Reorder to two, four, three; squash four into two.
    plan.steps.swap(1, 2);
    plan.steps[1].action = Action::Squash("two+four".into());
    plan.steps[2].action = Action::Reword("THREE".into());
    let r = rebase::run(&s.git, &plan, &exe()).await.expect("run");
    assert_eq!(r, Outcome::Done);
    assert_eq!(s.log(), ["THREE", "two+four", "one"]);
}

#[tokio::test]
async fn autostash_keeps_local_changes() {
    let (s, ids) = four();
    s.write("one.txt", "dirty\n");
    rebase::single(&s.git, &exe(), id(&ids[2]), Action::Drop)
        .await
        .expect("drop");
    assert_eq!(s.log(), ["four", "two", "one"]);
    assert_eq!(s.read("one.txt"), "dirty\n");
}

#[tokio::test]
async fn conflict_stops_and_aborts() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    let a = s.commit_all("a");
    s.write("f.txt", "b\n");
    s.commit_all("b");
    s.write("f.txt", "c\n");
    s.commit_all("c");
    // Dropping "b" makes "c" conflict.
    let _ = a;
    let b = s.sh(&["rev-parse", "HEAD~1"]);
    let r = rebase::single(&s.git, &exe(), id(b.trim()), Action::Drop)
        .await
        .expect("drop");
    assert_eq!(r, Outcome::Stopped);
    let st = rebase::state(&s.git).await;
    assert!(matches!(st, RepoState::Rebase { .. }));
    rebase::abort_op(&s.git, &st).await.expect("abort");
    assert_eq!(s.log(), ["c", "b", "a"]);
}

/// A reword is a pick and then an `exec` line that amends the message.
/// Skipping the pick must skip the new message too, or it would go to the
/// commit before.
#[tokio::test]
async fn skipping_a_conflicted_reword_skips_its_message() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    let a = s.commit_all("a");
    s.write("f.txt", "b\n");
    let b = s.commit_all("b");
    s.write("f.txt", "c\n");
    s.commit_all("c");
    let mut plan = rebase::plan_from(&s.git, id(&b)).await.expect("plan");
    plan.steps[0].action = Action::Drop;
    plan.steps[1].action = Action::Reword("C".into());
    let r = rebase::run(&s.git, &plan, &exe()).await.expect("run");
    assert_eq!(r, Outcome::Stopped);
    let st = rebase::state(&s.git).await;
    let r = rebase::skip_op(&s.git, &st).await.expect("skip");
    assert_eq!(r, Outcome::Done);
    assert_eq!(s.log(), ["a"]);
    assert_eq!(s.sh(&["rev-parse", "HEAD"]).trim(), a);
}

/// `fixup_staged_into` commits the staged changes before it rebases.
/// Aborting must stage them again rather than leave that commit.
#[tokio::test]
async fn aborting_a_staged_fixup_stages_the_changes_again() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    let a = s.commit_all("a");
    s.write("f.txt", "b\n");
    let b = s.commit_all("b");
    s.write("f.txt", "x\n");
    s.sh(&["add", "f.txt"]);
    let r = rebase::fixup_staged_into(&s.git, id(&a))
        .await
        .expect("fixup");
    assert_eq!(r, Outcome::Stopped);
    let st = rebase::state(&s.git).await;
    rebase::abort_op(&s.git, &st).await.expect("abort");
    assert_eq!(rebase::state(&s.git).await, RepoState::Clean);
    assert_eq!(s.sh(&["rev-parse", "HEAD"]).trim(), b);
    assert_eq!(s.index("f.txt"), "x\n");
    assert_eq!(s.read("f.txt"), "x\n");
}

#[tokio::test]
async fn fixup_staged_changes() {
    let (s, ids) = four();
    s.write("two.txt", "two fixed\n");
    s.sh(&["add", "two.txt"]);
    rebase::fixup_staged_into(&s.git, id(&ids[1]))
        .await
        .expect("fixup staged");
    assert_eq!(s.log(), ["four", "three", "two", "one"]);
    assert_eq!(s.sh(&["show", "HEAD~2:two.txt"]), "two fixed\n");
}

#[tokio::test]
async fn refuses_merges_in_range() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    let base = s.commit_all("base");
    s.sh(&["checkout", "-q", "-b", "side"]);
    s.write("b.txt", "b\n");
    s.commit_all("side");
    s.sh(&["checkout", "-q", "main"]);
    s.write("c.txt", "c\n");
    s.commit_all("main");
    s.sh(&["merge", "-q", "--no-edit", "side"]);
    let main_commit = s.sh(&["rev-parse", "HEAD^1"]);
    let _ = base;
    assert!(
        rebase::plan_from(&s.git, id(main_commit.trim()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refuses_to_start_during_another_rebase() {
    let (s, ids) = four();
    let r = rebase::single(&s.git, &exe(), id(&ids[1]), Action::Edit)
        .await
        .expect("edit");
    assert_eq!(r, Outcome::Stopped);
    let head = s.sh(&["rev-parse", "HEAD"]);

    // A second rebase can't start, and saying it "stopped" would be wrong.
    assert!(
        rebase::single(&s.git, &exe(), id(&ids[0]), Action::Edit)
            .await
            .is_err()
    );
    assert!(rebase::rebase_onto(&s.git, "main").await.is_err());
    // This one would otherwise leave a fixup commit behind.
    s.write("one.txt", "one more\n");
    s.sh(&["add", "-A"]);
    assert!(
        rebase::fixup_staged_into(&s.git, id(&ids[0]))
            .await
            .is_err()
    );
    assert_eq!(s.sh(&["rev-parse", "HEAD"]), head);
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "one.txt");
}

#[tokio::test]
async fn rebase_onto_a_branch_that_looks_like_an_option() {
    let (s, ids) = four();
    s.sh(&["checkout", "-q", "-b", "side", &ids[2]]);
    s.write("side.txt", "side\n");
    let side = s.commit_all("side");
    s.sh(&["checkout", "-q", "main"]);
    // As an option, this would add an exec line after each commit.
    let name = "--exec=touch${IFS}marker";
    s.sh(&["update-ref", &format!("refs/heads/{name}"), &side]);
    let outcome = rebase::rebase_onto(&s.git, name).await.expect("rebase");
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(s.log(), ["four", "side", "three", "two", "one"]);
    assert!(!s.file("marker").exists());
}
