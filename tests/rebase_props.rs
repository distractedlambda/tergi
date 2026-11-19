//! Property tests for rewriting history through the planner.
//!
//! Each case makes a straight history in which commit `k` only adds
//! `f<k>.txt`, so no reordering can conflict. It builds a `Planner` from
//! `plan_from`, moves rows around, gives them random actions, and runs the
//! plan with the real `git rebase`. A small model works out which commits
//! should come out, with which messages and files.

use crate::common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::Scratch;
use common::settings::{Settings, settings};
use gix::ObjectId;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::app::planner::{PlanAction, Planner};
use tergi::git::rebase::{self, Outcome, RepoState};

#[derive(Debug, Clone)]
enum Move {
    /// `move_row(from, to)`.
    Row(usize, usize),
    /// Select a row and move it up or down with `move_selected`.
    Selected(usize, bool),
}

#[derive(Debug, Clone)]
struct Case {
    /// The messages of the commits, oldest first.
    messages: Vec<String>,
    /// The oldest commit in the plan, as an index into `messages`.
    start: usize,
    moves: Vec<Move>,
    /// The action and message for each row, newest first, after the moves.
    /// There is one for each commit from `start` on.
    actions: Vec<(PlanAction, Option<String>)>,
    settings: Settings,
}

fn message_line() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "Fix the crash",
        "#42 Fix the crash",
        "# not a comment",
        "",
        "",
        "   ",
        "say \"hi\" and 'bye'",
        "naïve café ✓",
        "日本語のテキスト",
        "trailing spaces   ",
        "\tindented with a tab",
        "no-break space\u{a0}",
        "$(echo no) `ls` \\n",
        "-F not an option",
        "exec git commit",
    ])
}

/// A message with `lines` lines. Some of the lines are blank, and so are
/// some whole messages.
fn message(lines: std::ops::Range<usize>) -> impl Strategy<Value = String> {
    (prop::collection::vec(message_line(), lines), any::<bool>()).prop_map(|(lines, eol)| {
        let mut m = lines.join("\n");
        if eol {
            m.push('\n');
        }
        m
    })
}

fn action() -> impl Strategy<Value = PlanAction> {
    prop_oneof![
        4 => Just(PlanAction::Pick),
        2 => Just(PlanAction::Reword),
        2 => Just(PlanAction::Squash),
        2 => Just(PlanAction::Fixup),
        1 => Just(PlanAction::Drop),
        1 => Just(PlanAction::Edit),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (0usize..3, 1usize..7)
        .prop_flat_map(|(before, n)| {
            let mv = prop_oneof![
                (0..n, 0..n).prop_map(|(a, b)| Move::Row(a, b)),
                (0..n, any::<bool>()).prop_map(|(a, up)| Move::Selected(a, up)),
            ];
            let row = (action(), prop::option::weighted(0.95, message(1..6)));
            (
                prop::collection::vec(message(0..6), before + n),
                Just(before),
                prop::collection::vec(mv, 0..12),
                prop::collection::vec(row, n),
                settings(),
            )
        })
        .prop_map(|(messages, start, moves, actions, settings)| Case {
            messages,
            start,
            moves,
            actions,
            settings,
        })
}

/// What `git commit --cleanup=whitespace` does to a message. It strips
/// trailing whitespace from each line, drops blank lines at the start and
/// the end, collapses runs of blank lines into one, and ends every line
/// with a newline. Unlike the default cleanup, it keeps lines that start
/// with `#`.
fn cleanup(msg: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in msg.split('\n') {
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            blank = true;
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The commit messages the history starts with. Commit messages can't be
/// empty, so an empty one becomes a summary.
fn original(case: &Case, k: usize) -> String {
    let m = cleanup(&case.messages[k]);
    if m.is_empty() {
        format!("commit {k}\n")
    } else {
        m
    }
}

fn file(k: usize) -> String {
    format!("f{k}.txt")
}

#[derive(Debug, PartialEq, Eq)]
struct Commit {
    message: String,
    files: BTreeSet<String>,
}

/// The commits the plan should make, oldest first, or why the planner
/// should refuse it. `rows` are oldest first, with the index of the
/// commit each one came from.
fn model(
    case: &Case,
    rows: &[(usize, PlanAction, Option<String>)],
) -> Result<Vec<Commit>, &'static str> {
    let mut out: Vec<Commit> = Vec::new();
    let mut prev: Option<PlanAction> = None;
    for (k, action, message) in rows {
        let melds = matches!(action, PlanAction::Squash | PlanAction::Fixup);
        if melds && prev.is_none() {
            return Err("a squash or fixup is the oldest commit");
        }
        if melds && prev == Some(PlanAction::Drop) {
            return Err("a squash or fixup melds into a dropped commit");
        }
        let new_message = match (action, message) {
            (PlanAction::Reword | PlanAction::Squash, None) => {
                return Err("a reword or squash has no message");
            }
            (PlanAction::Reword | PlanAction::Squash, Some(m)) if cleanup(m).is_empty() => {
                return Err("a reword or squash has an empty message");
            }
            (PlanAction::Reword | PlanAction::Squash, Some(m)) => Some(cleanup(m)),
            _ => None,
        };
        match action {
            PlanAction::Pick | PlanAction::Edit | PlanAction::Reword => out.push(Commit {
                message: new_message.unwrap_or_else(|| original(case, *k)),
                files: BTreeSet::from([file(*k)]),
            }),
            PlanAction::Squash | PlanAction::Fixup => {
                // A meld can't come first or follow a drop, so the commit it
                // melds into is the last one made from this plan.
                let last = out.last_mut().expect("something to meld into");
                last.files.insert(file(*k));
                if let Some(m) = new_message {
                    last.message = m;
                }
            }
            PlanAction::Drop => {}
        }
        prev = Some(*action);
    }
    if case.start == 0 && out.is_empty() {
        return Err("every commit down to the root is dropped");
    }
    Ok(out)
}

fn id(s: &str) -> ObjectId {
    ObjectId::from_hex(s.trim().as_bytes()).expect("id")
}

/// The message of a commit, exactly as git stored it.
fn stored_message(s: &Scratch, commit: &str) -> String {
    let raw = s.sh(&["cat-file", "commit", commit]);
    match raw.split_once("\n\n") {
        Some((_, message)) => message.to_string(),
        None => String::new(),
    }
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Scratch::with(&case.settings);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_tergi"));
    let msg_file = s.path().join(".git").join("test-message");
    let mut ids = Vec::new();
    for k in 0..case.messages.len() {
        s.write(&file(k), &format!("{k}\n"));
        std::fs::write(&msg_file, original(case, k)).expect("write message");
        s.sh(&["add", "-A"]);
        s.sh(&[
            "commit",
            "-q",
            "--cleanup=verbatim",
            "-F",
            &msg_file.to_string_lossy(),
        ]);
        ids.push(s.sh(&["rev-parse", "HEAD"]).trim().to_string());
    }
    std::fs::remove_file(&msg_file).expect("remove message");

    let plan = rt
        .block_on(rebase::plan_from(&s.git, id(&ids[case.start])))
        .expect("plan");
    let mut p = Planner::new(plan);
    prop_assert_eq!(p.rows.len(), case.actions.len());
    for m in &case.moves {
        match *m {
            Move::Row(from, to) => p.move_row(from, to),
            Move::Selected(i, up) => {
                p.selected = i;
                p.move_selected(up);
            }
        }
    }
    for (i, (action, message)) in case.actions.iter().enumerate() {
        p.rows[i].action = *action;
        p.rows[i].message = message.clone();
    }

    // Work out which commit each row came from, oldest first.
    let rows: Vec<(usize, PlanAction, Option<String>)> = p
        .rows
        .iter()
        .rev()
        .map(|r| {
            let k = ids
                .iter()
                .position(|c| id(c) == r.id)
                .expect("row for a known commit");
            (k, r.action, r.message.clone())
        })
        .collect();
    let want = model(case, &rows);
    let plan = match (p.to_plan(), want) {
        (Ok(plan), Ok(want)) => (plan, want),
        (Err(_), Err(_)) => return Ok(()),
        (Ok(_), Err(why)) => {
            return Err(TestCaseError::fail(format!(
                "the planner accepted a plan in which {why}"
            )));
        }
        (Err(e), Ok(_)) => {
            return Err(TestCaseError::fail(format!(
                "the planner refused a valid plan: {e}"
            )));
        }
    };
    let (plan, want) = plan;

    // Each edit step stops the rebase once. Nothing else should.
    let edits = rows.iter().filter(|r| r.1 == PlanAction::Edit).count();
    let mut stops = 0;
    let mut outcome = rt.block_on(rebase::run(&s.git, &plan, &exe));
    loop {
        match outcome {
            Ok(Outcome::Done) => break,
            Ok(Outcome::Stopped) => {
                stops += 1;
                prop_assert!(
                    stops <= edits,
                    "the rebase stopped more often than it has edits"
                );
                outcome = rt.block_on(rebase::continue_rebase(&s.git));
            }
            Err(e) => return Err(TestCaseError::fail(format!("the rebase failed: {e:#}"))),
        }
    }
    prop_assert_eq!(stops, edits);
    prop_assert_eq!(rt.block_on(rebase::state(&s.git)), RepoState::Clean);
    prop_assert!(!s.file(".git/tergi").exists(), ".git/tergi is still there");

    // The commits before the plan stay as they were.
    let log: Vec<String> = s
        .sh(&["rev-list", "--reverse", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    prop_assert_eq!(&log[..case.start], &ids[..case.start]);
    let got: Vec<Commit> = log[case.start..]
        .iter()
        .map(|c| Commit {
            message: stored_message(&s, c),
            files: s
                .sh(&[
                    "diff-tree",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    "--root",
                    c,
                ])
                .lines()
                .map(str::to_string)
                .collect(),
        })
        .collect();
    prop_assert_eq!(got, want);

    // The tree has every file that wasn't dropped, with its content.
    let mut files: BTreeSet<String> = (0..case.start).map(file).collect();
    for (k, action, _) in &rows {
        if *action != PlanAction::Drop {
            files.insert(file(*k));
        }
    }
    let tree: BTreeSet<String> = s
        .sh(&["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    prop_assert_eq!(&tree, &files);
    for f in &files {
        let k = &f[1..f.len() - 4];
        prop_assert_eq!(s.read(f), format!("{k}\n"));
    }
    prop_assert_eq!(s.sh(&["status", "--porcelain"]), "");
    Ok(())
}

proptest! {
    #[test]
    fn running_a_plan(case in case()) {
        check(&case)?;
    }
}
