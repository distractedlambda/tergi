//! Property tests for stash operations.
//!
//! tergi names a stash by its commit id, and `stash_ref` looks up the
//! stash's current `stash@{n}` name just before it runs git. Each case
//! runs random steps that push, drop and store stashes, some through
//! tergi and some with git directly, which renumbers the others behind
//! tergi's back. Then it applies, pops or drops stashes through tergi by
//! id. Every stash holds its own text in `f.txt`, so applying one shows
//! which stash git used. After each step the test checks the stash list
//! against a model, both from git and from `Refs::load`.

use crate::common;

use common::Scratch;
use common::settings::{Settings, settings};
use gix::ObjectId;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::git::ops;
use tergi::git::refs::Refs;

#[derive(Debug, Clone)]
enum Step {
    /// Make a stash. `staged` stashes only the staged change, which git
    /// doesn't allow together with untracked files.
    Push {
        tergi: bool,
        untracked: bool,
        staged: bool,
        message: bool,
    },
    /// Apply or pop a stash through tergi. `pick` is any stash made so
    /// far, so it can be one that is gone.
    Apply { pick: usize, pop: bool },
    /// Drop a stash through tergi by id, like `Apply`.
    Drop { pick: usize },
    /// Drop `stash@{n}` with git, where n is `at` modulo the length.
    GitDrop { at: usize },
    /// Put a stash made earlier back on the list with `git stash store`.
    /// This is how people get back a stash they dropped. If the stash is
    /// still there, the list then holds it twice, unless it is the newest
    /// one. Git doesn't add an entry that leaves `refs/stash` where it was.
    Store { pick: usize },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
            |(tergi, untracked, staged, message)| Step::Push {
                tergi,
                untracked,
                staged: staged && !untracked,
                message,
            }
        ),
        3 => (0..8usize, any::<bool>()).prop_map(|(pick, pop)| Step::Apply { pick, pop }),
        2 => (0..8usize).prop_map(|pick| Step::Drop { pick }),
        1 => (0..8usize).prop_map(|at| Step::GitDrop { at }),
        1 => (0..8usize).prop_map(|pick| Step::Store { pick }),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    settings: Settings,
    steps: Vec<Step>,
}

fn case() -> impl Strategy<Value = Case> {
    (settings(), prop::collection::vec(step(), 1..16))
        .prop_map(|(settings, steps)| Case { settings, steps })
}

/// A stash that the case made.
#[derive(Debug, Clone)]
struct Made {
    id: ObjectId,
    /// The text of `f.txt` in the stash.
    text: String,
    /// The text of the untracked file `u.txt`, if the stash has one.
    untracked: Option<String>,
}

fn rev(s: &Scratch, rev: &str) -> ObjectId {
    ObjectId::from_hex(s.sh(&["rev-parse", rev]).trim().as_bytes()).expect("object id")
}

/// Throw away every change, so that each step starts from a clean tree.
fn clean(s: &Scratch) {
    s.sh(&["reset", "-q", "--hard"]);
    s.sh(&["clean", "-q", "-f", "-d"]);
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = Scratch::with(&case.settings);
    s.write("f.txt", "base\n");
    s.write("g.txt", "base\n");
    s.commit_all("init");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let mut made: Vec<Made> = Vec::new();
    // The stash ids in the order `git stash list` gives them, newest first.
    let mut list: Vec<ObjectId> = Vec::new();

    for (n, step) in case.steps.iter().enumerate() {
        clean(&s);
        match step {
            Step::Push {
                tergi,
                untracked,
                staged,
                message,
            } => {
                let text = format!("stash {n}\n");
                s.write("f.txt", &text);
                if *staged {
                    // The change to g.txt isn't staged, so it stays behind.
                    s.sh(&["add", "f.txt"]);
                    s.write("g.txt", &format!("left behind {n}\n"));
                }
                let untracked = untracked.then(|| format!("untracked {n}\n"));
                if let Some(u) = &untracked {
                    s.write("u.txt", u);
                }
                let msg = if *message {
                    format!("step {n}")
                } else {
                    String::new()
                };
                if *tergi {
                    rt.block_on(ops::stash_push(&s.git, &msg, untracked.is_some(), *staged))
                        .map_err(|e| TestCaseError::fail(format!("push: {e:#}")))?;
                } else {
                    let mut args = vec!["stash", "push", "-q"];
                    if untracked.is_some() {
                        args.push("--include-untracked");
                    }
                    if *staged {
                        args.push("--staged");
                    }
                    if *message {
                        args.extend(["-m", &msg]);
                    }
                    s.sh(&args);
                }
                prop_assert!(!s.file("u.txt").exists(), "u.txt stayed after the push");
                let id = rev(&s, "refs/stash");
                list.insert(0, id);
                made.push(Made {
                    id,
                    text,
                    untracked,
                });
            }
            Step::Apply { pick, pop } => {
                let Some(m) = made.get(pick % made.len().max(1)).cloned() else {
                    continue;
                };
                let result = rt.block_on(ops::stash_apply(&s.git, m.id, *pop));
                match list.iter().position(|&id| id == m.id) {
                    Some(i) => {
                        result.map_err(|e| TestCaseError::fail(format!("apply: {e:#}")))?;
                        prop_assert_eq!(s.read("f.txt"), m.text.clone());
                        match &m.untracked {
                            Some(u) => prop_assert_eq!(&s.read("u.txt"), u),
                            None => prop_assert!(!s.file("u.txt").exists()),
                        }
                        if *pop {
                            list.remove(i);
                        }
                    }
                    None => {
                        prop_assert!(result.is_err(), "applied a stash that is gone");
                        prop_assert_eq!(s.read("f.txt"), "base\n");
                    }
                }
            }
            Step::Drop { pick } => {
                let Some(m) = made.get(pick % made.len().max(1)) else {
                    continue;
                };
                let result = rt.block_on(ops::stash_drop(&s.git, m.id));
                match list.iter().position(|&id| id == m.id) {
                    Some(i) => {
                        result.map_err(|e| TestCaseError::fail(format!("drop: {e:#}")))?;
                        list.remove(i);
                    }
                    None => prop_assert!(result.is_err(), "dropped a stash that is gone"),
                }
            }
            Step::GitDrop { at } => {
                if list.is_empty() {
                    continue;
                }
                let i = at % list.len();
                s.sh(&["stash", "drop", "-q", &format!("stash@{{{i}}}")]);
                list.remove(i);
            }
            Step::Store { pick } => {
                let Some(m) = made.get(pick % made.len().max(1)) else {
                    continue;
                };
                s.sh(&["stash", "store", "-q", "-m", "stored", &m.id.to_string()]);
                if list.first() != Some(&m.id) {
                    list.insert(0, m.id);
                }
            }
        }

        let listed: Vec<ObjectId> = s
            .sh(&["stash", "list", "--format=%H"])
            .lines()
            .map(|l| ObjectId::from_hex(l.as_bytes()).expect("stash id"))
            .collect();
        prop_assert_eq!(&listed, &list, "git's stash list after {:?}", step);

        let refs = rt.block_on(Refs::load(&s.git)).expect("load refs");
        let sidebar: Vec<(usize, ObjectId)> =
            refs.stashes.iter().map(|st| (st.index, st.id)).collect();
        let want: Vec<(usize, ObjectId)> = list.iter().copied().enumerate().collect();
        prop_assert_eq!(sidebar, want, "the sidebar's stashes after {:?}", step);
    }
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

    fn stash_steps_find_the_right_stash(case in case()) {
        check(&case)?;
    }
}
