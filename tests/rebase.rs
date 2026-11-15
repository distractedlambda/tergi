mod common;

use std::path::PathBuf;

use common::Scratch;
use gix::ObjectId;
use tergi::git::rebase::{self, Action, Outcome, RepoState};

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
