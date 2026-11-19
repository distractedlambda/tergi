//! Property tests for rebases that stop.
//!
//! Each case makes a history in which most commits change one line of
//! `shared.txt`, so reordering, dropping and melding commits can conflict.
//! It starts a rebase through the planner or one of the single commit
//! operations. At each stop it picks a way to go on. At a conflict it can
//! take a side, edit the file, skip or abort. At an `edit` step it can
//! continue, amend the commit, skip or abort. A model replays the todo
//! list and says where the rebase should stop, what `state_in` should
//! report there, and which commits should come out.

use crate::common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use common::Scratch;
use common::settings::{Settings, settings};
use gix::ObjectId;
use gix::bstr::BString;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tergi::app::planner::{PlanAction, Planner};
use tergi::git::ops;
use tergi::git::rebase::{self, Action, Outcome, RepoState};

/// The number of lines in `shared.txt` that commits change. Between them
/// are lines that never change, so git merges changes to different slots
/// without a conflict, and changes to the same slot conflict.
const SLOTS: usize = 3;

type Shared = Vec<String>;

fn render(shared: &Shared) -> String {
    let mut out = String::new();
    for (i, value) in shared.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("--- {} ---\n", i - 1));
        }
        out.push_str(value);
        out.push('\n');
    }
    out
}

fn parse(text: &str) -> Shared {
    let shared: Shared = text.lines().step_by(2).map(str::to_string).collect();
    if render(&shared) == text {
        shared
    } else {
        vec![format!("not in slots: {text:?}")]
    }
}

/// The content of every file other than `shared.txt` follows from its name.
fn content(name: &str) -> String {
    format!("{name}\n")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tree {
    shared: Option<Shared>,
    files: BTreeSet<String>,
}

impl Tree {
    fn with(&self, file: &str) -> Tree {
        let mut t = self.clone();
        t.files.insert(file.to_string());
        t
    }
}

/// What git's merge should do with one value, or `None` for a conflict.
fn merge3<T: PartialEq + Clone>(base: &T, ours: &T, theirs: &T) -> Option<T> {
    if base == theirs || ours == theirs {
        Some(ours.clone())
    } else if base == ours {
        Some(theirs.clone())
    } else {
        None
    }
}

struct Merged {
    files: BTreeSet<String>,
    /// The merged `shared.txt`. On a conflict, what editing the file to
    /// resolve it writes: the merged slots, with `fix` in each slot that
    /// conflicts. If the file is missing on a side, every slot is `fix`.
    shared: Result<Option<Shared>, Shared>,
}

/// Apply the change from `base` to `theirs` onto `ours`.
fn merge(base: &Tree, ours: &Tree, theirs: &Tree, fix: &str) -> Merged {
    let names: BTreeSet<&String> = base
        .files
        .iter()
        .chain(&ours.files)
        .chain(&theirs.files)
        .collect();
    let files = names
        .into_iter()
        .filter(|n| {
            let has = |t: &Tree| t.files.contains(*n);
            merge3(&has(base), &has(ours), &has(theirs)).expect("files are only added")
        })
        .cloned()
        .collect();
    let shared = match merge3(&base.shared, &ours.shared, &theirs.shared) {
        Some(s) => Ok(s),
        None => match (&base.shared, &ours.shared, &theirs.shared) {
            (Some(b), Some(o), Some(t)) => {
                let slots: Vec<Option<String>> =
                    (0..SLOTS).map(|i| merge3(&b[i], &o[i], &t[i])).collect();
                if slots.iter().all(Option::is_some) {
                    Ok(Some(slots.into_iter().flatten().collect()))
                } else {
                    Err(slots
                        .into_iter()
                        .map(|s| s.unwrap_or_else(|| fix.to_string()))
                        .collect())
                }
            }
            _ => Err(vec![fix.to_string(); SLOTS]),
        },
    };
    Merged { files, shared }
}

/// A commit of the original history.
#[derive(Debug, Clone)]
struct Orig {
    parent: Tree,
    tree: Tree,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Pick(usize),
    Edit(usize),
    Fixup(usize),
    Drop(usize),
    /// The `exec` line that gives the commit before it a new message.
    Amend(String),
}

#[derive(Debug)]
enum Stop {
    Edit(usize),
    Conflict {
        k: usize,
        /// Whether the commit melds into the one before it.
        meld: bool,
        files: BTreeSet<String>,
        ours: Option<Shared>,
        theirs: Option<Shared>,
        edit: Shared,
    },
}

/// The model of a rebase. It replays the todo list on top of the commits
/// before the plan.
struct Sim<'a> {
    origs: &'a [Orig],
    /// The commits of the branch, oldest first.
    commits: Vec<(Tree, String)>,
    todo: VecDeque<Line>,
    done: usize,
}

impl Sim<'_> {
    fn head(&self) -> Tree {
        self.commits.last().map(|c| c.0.clone()).unwrap_or_default()
    }

    fn commit(&mut self, k: usize, tree: Tree, meld: bool) {
        if meld {
            self.commits.last_mut().expect("a commit to meld into").0 = tree;
        } else {
            self.commits.push((tree, self.origs[k].message.clone()));
        }
    }

    /// Commit the resolution of a conflict.
    fn resolve(&mut self, k: usize, tree: Tree, meld: bool) {
        // A rebase with `--root` starts on an empty commit with an empty
        // message. A clean first pick replaces it, but one that conflicted
        // goes on top of it. `todo/bugs.md` has this.
        if self.commits.is_empty() {
            self.commits.push((Tree::default(), String::new()));
        }
        self.commit(k, tree, meld);
    }

    /// Run until the rebase stops, or `None` when it's done.
    fn run(&mut self) -> Option<Stop> {
        while let Some(line) = self.todo.pop_front() {
            self.done += 1;
            let (k, meld, edit) = match line {
                Line::Drop(_) => continue,
                Line::Amend(m) => {
                    self.commits.last_mut().expect("a commit to amend").1 = m;
                    continue;
                }
                Line::Pick(k) => (k, false, false),
                Line::Fixup(k) => (k, true, false),
                Line::Edit(k) => (k, false, true),
            };
            let orig = &self.origs[k];
            let ours = self.head();
            let m = merge(&orig.parent, &ours, &orig.tree, &format!("fixed {k}"));
            match m.shared {
                Ok(shared) => {
                    let files = m.files;
                    self.commit(k, Tree { shared, files }, meld);
                    if edit {
                        return Some(Stop::Edit(k));
                    }
                }
                // An edit step that conflicts stops only for the conflict.
                Err(fix) => {
                    return Some(Stop::Conflict {
                        k,
                        meld,
                        files: m.files,
                        ours: ours.shared,
                        theirs: orig.tree.shared.clone(),
                        edit: fix,
                    });
                }
            }
        }
        None
    }

    /// The commit at a conflict is skipped. So is the message it was about
    /// to get, if it's a reword or squash.
    fn skip(&mut self) {
        if let Some(Line::Amend(_)) = self.todo.front() {
            self.todo.pop_front();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Resolve {
    Ours,
    Theirs,
    Edit,
    Skip,
    Abort,
}

#[derive(Debug, Clone, Copy)]
enum AtEdit {
    Continue,
    /// Add a file to the commit and continue.
    Amend,
    Skip,
    Abort,
}

#[derive(Debug, Clone)]
enum Op {
    /// Run a plan from the planner. `order` holds the commits from
    /// `start` on, oldest first, and `actions` says what to do with each
    /// one, with a choice of message.
    Plan {
        start: usize,
        order: Vec<usize>,
        actions: Vec<(PlanAction, u8)>,
    },
    Reword(usize, u8),
    /// `rebase::single` with a drop, edit, fixup or squash.
    Single(usize, PlanAction, u8),
    /// Move a commit towards HEAD, or away from it.
    Move(usize, bool),
    /// Stage a change to a slot and a new file, and fix up a commit with
    /// them.
    FixupStaged(usize, usize),
}

#[derive(Debug, Clone)]
struct Case {
    /// For commit 0, which sets up the repository, and each commit after
    /// it: the slot it changes and a choice of message.
    commits: Vec<(Option<usize>, u8)>,
    op: Op,
    /// What to do at each stop, by the kind of stop. When they run out,
    /// conflicts take our side and edit steps continue.
    stops: Vec<(Resolve, AtEdit)>,
    /// Whether `other.txt` has a change that isn't staged, and whether
    /// there's an untracked file.
    dirty: (bool, bool),
    settings: Settings,
}

/// The messages have lines that start with `#` and `;`, which git must
/// keep, whatever `core.commentChar` says. Each subject names its commit,
/// because `--autosquash` finds the commit to fix up by its subject.
fn message(k: usize, v: u8) -> String {
    match v % 3 {
        0 => format!("Commit {k}\n"),
        1 => format!("#{k} Fix the crash\n\n# not a comment\n"),
        _ => format!("Commit {k}: say \"hi\"\n\n; not a comment either\n"),
    }
}

fn new_message(k: usize, v: u8) -> String {
    match v % 2 {
        0 => format!("New message for {k}\n"),
        _ => format!("#{k} New message\n\n# kept\n"),
    }
}

fn plan_action() -> impl Strategy<Value = PlanAction> {
    prop_oneof![
        4 => Just(PlanAction::Pick),
        2 => Just(PlanAction::Reword),
        2 => Just(PlanAction::Squash),
        2 => Just(PlanAction::Fixup),
        2 => Just(PlanAction::Drop),
        2 => Just(PlanAction::Edit),
    ]
}

fn op(n: usize) -> impl Strategy<Value = Op> {
    let plan = (1..=n)
        .prop_flat_map(move |start| {
            (
                Just(start),
                Just((start..=n).collect::<Vec<_>>()).prop_shuffle(),
                prop::collection::vec((plan_action(), 0u8..2), n + 1 - start),
            )
        })
        .prop_map(|(start, order, actions)| Op::Plan {
            start,
            order,
            actions,
        });
    let single = prop::sample::select(vec![
        PlanAction::Drop,
        PlanAction::Edit,
        PlanAction::Fixup,
        PlanAction::Squash,
    ]);
    prop_oneof![
        4 => plan,
        1 => (1..=n, 0u8..2).prop_map(|(k, v)| Op::Reword(k, v)),
        2 => (1..=n, single, 0u8..2).prop_map(|(k, a, v)| Op::Single(k, a, v)),
        2 => (1..=n, any::<bool>()).prop_map(move |(k, newer)| Op::Move(k, newer && k < n)),
        2 => (1..=n, 0..SLOTS).prop_map(|(k, slot)| Op::FixupStaged(k, slot)),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    let resolve = prop_oneof![
        3 => Just(Resolve::Ours),
        3 => Just(Resolve::Theirs),
        3 => Just(Resolve::Edit),
        2 => Just(Resolve::Skip),
        1 => Just(Resolve::Abort),
    ];
    let at_edit = prop_oneof![
        3 => Just(AtEdit::Continue),
        3 => Just(AtEdit::Amend),
        1 => Just(AtEdit::Skip),
        1 => Just(AtEdit::Abort),
    ];
    (1usize..7)
        .prop_flat_map(move |n| {
            (
                prop::collection::vec((prop::option::weighted(0.8, 0..SLOTS), 0u8..3), n + 1),
                op(n),
                prop::collection::vec((resolve.clone(), at_edit.clone()), 0..8),
                any::<(bool, bool)>(),
                settings(),
            )
        })
        .prop_map(|(commits, op, stops, dirty, settings)| Case {
            commits,
            op,
            stops,
            dirty,
            settings,
        })
}

/// The original history: commit 0 adds `shared.txt` and `other.txt`, and
/// every commit after it adds `f<k>.txt` and maybe changes a slot.
fn history(case: &Case) -> Vec<Orig> {
    let mut origs: Vec<Orig> = Vec::new();
    for (k, &(slot, v)) in case.commits.iter().enumerate() {
        let message = message(k, v);
        if k == 0 {
            let tree = Tree {
                shared: Some((0..SLOTS).map(|i| format!("base {i}")).collect()),
                files: BTreeSet::from(["other.txt".to_string()]),
            };
            origs.push(Orig {
                parent: Tree::default(),
                tree,
                message,
            });
            continue;
        }
        let parent = origs[k - 1].tree.clone();
        let mut tree = parent.with(&format!("f{k}.txt"));
        if let (Some(s), Some(shared)) = (slot, &mut tree.shared) {
            shared[s] = format!("commit {k}");
        }
        origs.push(Orig {
            parent,
            tree,
            message,
        });
    }
    origs
}

/// The rows of the plan that the operation should run, oldest first, and
/// the first commit in it. `None` means the operation doesn't rebase.
fn rows(case: &Case) -> Option<(usize, Vec<(usize, Action)>)> {
    let n = case.commits.len() - 1;
    let picks =
        |start: usize| -> Vec<(usize, Action)> { (start..=n).map(|k| (k, Action::Pick)).collect() };
    let set = |rows: &mut Vec<(usize, Action)>, k: usize, action: Action| {
        if let Some(r) = rows.iter_mut().find(|r| r.0 == k) {
            r.1 = action;
        }
    };
    Some(match &case.op {
        Op::Plan {
            start,
            order,
            actions,
        } => (*start, plan_rows(order, actions)),
        Op::Reword(k, _) if *k == n => return None,
        Op::Reword(k, v) => {
            let mut rows = picks(*k);
            set(&mut rows, *k, Action::Reword(new_message(*k, *v)));
            (*k, rows)
        }
        Op::Single(k, action, v) => {
            let (start, action) = match action {
                PlanAction::Drop => (*k, Action::Drop),
                PlanAction::Edit => (*k, Action::Edit),
                PlanAction::Fixup => (k - 1, Action::Fixup),
                _ => (k - 1, Action::Squash(new_message(*k, *v))),
            };
            let mut rows = picks(start);
            set(&mut rows, *k, action);
            (start, rows)
        }
        Op::Move(k, newer) => {
            let start = if *newer { *k } else { k - 1 };
            let mut rows = picks(start);
            rows.swap(k - start, if *newer { k - start + 1 } else { 0 });
            (start, rows)
        }
        Op::FixupStaged(target, _) => {
            let mut rows = picks(*target);
            rows.insert(1, (n + 1, Action::Fixup));
            (*target, rows)
        }
    })
}

/// The rows the planner gets, oldest first. A squash or fixup that has
/// nothing to meld into becomes a pick, because the planner would refuse
/// it. `rebase_props` checks that.
fn plan_rows(order: &[usize], actions: &[(PlanAction, u8)]) -> Vec<(usize, Action)> {
    let mut rows: Vec<(usize, Action)> = Vec::new();
    for (&k, &(action, v)) in order.iter().zip(actions) {
        let melds = matches!(action, PlanAction::Squash | PlanAction::Fixup);
        let after_drop = matches!(rows.last(), None | Some((_, Action::Drop)));
        let action = match action {
            _ if melds && after_drop => Action::Pick,
            PlanAction::Pick => Action::Pick,
            PlanAction::Reword => Action::Reword(new_message(k, v)),
            PlanAction::Squash => Action::Squash(new_message(k, v)),
            PlanAction::Fixup => Action::Fixup,
            PlanAction::Drop => Action::Drop,
            PlanAction::Edit => Action::Edit,
        };
        rows.push((k, action));
    }
    rows
}

fn todo(rows: &[(usize, Action)]) -> VecDeque<Line> {
    let mut lines = VecDeque::new();
    for (k, action) in rows {
        match action {
            Action::Pick => lines.push_back(Line::Pick(*k)),
            Action::Edit => lines.push_back(Line::Edit(*k)),
            Action::Drop => lines.push_back(Line::Drop(*k)),
            Action::Fixup => lines.push_back(Line::Fixup(*k)),
            Action::Reword(m) => lines.extend([Line::Pick(*k), Line::Amend(m.clone())]),
            Action::Squash(m) => lines.extend([Line::Fixup(*k), Line::Amend(m.clone())]),
        }
    }
    lines
}

fn id(s: &str) -> ObjectId {
    ObjectId::from_hex(s.trim().as_bytes()).expect("id")
}

fn write_shared(s: &Scratch, shared: &Shared) {
    s.write("shared.txt", &render(shared));
}

/// The message of a commit, exactly as git stored it.
fn stored_message(s: &Scratch, commit: &str) -> String {
    let raw = s.sh(&["cat-file", "commit", commit]);
    match raw.split_once("\n\n") {
        Some((_, message)) => message.to_string(),
        None => String::new(),
    }
}

fn stored_tree(s: &Scratch, commit: &str) -> Tree {
    let mut files: BTreeSet<String> = s
        .sh(&["ls-tree", "-r", "--name-only", commit])
        .lines()
        .map(str::to_string)
        .collect();
    let shared = files
        .remove("shared.txt")
        .then(|| parse(&s.sh(&["cat-file", "blob", &format!("{commit}:shared.txt")])));
    Tree { shared, files }
}

fn worktree(root: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(root)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".git")
        .map(|name| {
            let text = std::fs::read_to_string(root.join(&name)).expect("read");
            (name, text)
        })
        .collect()
}

const DIRTY: &str = "a change that isn't staged\n";

/// The files the working tree should have when HEAD has `tree`.
fn expected_worktree(tree: &Tree, dirty: (bool, bool)) -> BTreeMap<String, String> {
    let mut files: BTreeMap<String, String> =
        tree.files.iter().map(|f| (f.clone(), content(f))).collect();
    if let Some(shared) = &tree.shared {
        files.insert("shared.txt".into(), render(shared));
    }
    if dirty.0
        && let Some(text) = files.get_mut("other.txt")
    {
        text.push_str(DIRTY);
    }
    if dirty.1 {
        files.insert("untracked.txt".into(), content("untracked.txt"));
    }
    files
}

/// Everything that aborting should give back.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    head: String,
    branch: String,
    index: String,
    worktree: BTreeMap<String, String>,
}

fn snapshot(s: &Scratch) -> Snapshot {
    Snapshot {
        head: s.sh(&["rev-parse", "HEAD"]),
        branch: s.sh(&["symbolic-ref", "HEAD"]),
        index: s.sh(&["ls-files", "--stage"]),
        worktree: worktree(s.path()),
    }
}

fn fail(what: &str, e: &anyhow::Error) -> TestCaseError {
    TestCaseError::fail(format!("{what} failed: {e:#}"))
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Scratch::with(&case.settings);
    let git_dir = s.path().join(".git");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_tergi"));
    let n = case.commits.len() - 1;

    let mut origs = history(case);
    let msg_file = git_dir.join("test-message");
    let mut ids = Vec::new();
    for orig in &origs {
        for f in &orig.tree.files {
            s.write(f, &content(f));
        }
        write_shared(&s, orig.tree.shared.as_ref().expect("shared.txt"));
        std::fs::write(&msg_file, &orig.message).expect("write message");
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

    // Commit 0 adds `other.txt`. When commit 1 moves below it, a conflict
    // can lead to skipping commit 0, and then `--autostash` has nowhere to
    // put the change back.
    let dirty = (
        case.dirty.0 && !matches!(case.op, Op::Move(1, false)),
        case.dirty.1,
    );
    if dirty.0 {
        s.write("other.txt", &(content("other.txt") + DIRTY));
    }
    if dirty.1 {
        s.write("untracked.txt", &content("untracked.txt"));
    }
    if let Op::FixupStaged(_, slot) = case.op {
        let parent = origs[n].tree.clone();
        let mut tree = parent.with("staged.txt");
        if let Some(shared) = &mut tree.shared {
            shared[slot] = "staged".into();
            write_shared(&s, shared);
        }
        s.write("staged.txt", &content("staged.txt"));
        rt.block_on(ops::stage_paths(&s.git, &["shared.txt", "staged.txt"]))
            .map_err(|e| fail("staging", &e))?;
        origs.push(Orig {
            parent,
            tree,
            message: String::new(),
        });
    }
    let before = snapshot(&s);

    let (start, rows) = rows(case).unwrap_or((n + 1, Vec::new()));
    let mut sim = Sim {
        origs: &origs,
        commits: origs[..start]
            .iter()
            .map(|o| (o.tree.clone(), o.message.clone()))
            .collect(),
        todo: todo(&rows),
        done: 0,
    };

    let mut outcome = match &case.op {
        Op::Plan { start, .. } => {
            let plan = rt
                .block_on(rebase::plan_from(&s.git, id(&ids[*start])))
                .map_err(|e| fail("planning", &e))?;
            let mut p = Planner::new(plan);
            let original = p.rows.clone();
            p.rows = rows
                .iter()
                .rev()
                .map(|(k, action)| {
                    let mut row = original
                        .iter()
                        .find(|r| r.id == id(&ids[*k]))
                        .expect("a row for each commit")
                        .clone();
                    (row.action, row.message) = match action {
                        Action::Pick => (PlanAction::Pick, None),
                        Action::Reword(m) => (PlanAction::Reword, Some(m.clone())),
                        Action::Squash(m) => (PlanAction::Squash, Some(m.clone())),
                        Action::Fixup => (PlanAction::Fixup, None),
                        Action::Drop => (PlanAction::Drop, None),
                        Action::Edit => (PlanAction::Edit, None),
                    };
                    row
                })
                .collect();
            let plan = p.to_plan().map_err(|e| fail("to_plan", &e))?;
            rt.block_on(rebase::run(&s.git, &plan, &exe))
        }
        Op::Reword(k, v) => rt.block_on(rebase::reword(
            &s.git,
            &exe,
            id(&ids[*k]),
            &new_message(*k, *v),
        )),
        Op::Single(k, action, v) => {
            let action = match action {
                PlanAction::Drop => Action::Drop,
                PlanAction::Edit => Action::Edit,
                PlanAction::Fixup => Action::Fixup,
                _ => Action::Squash(new_message(*k, *v)),
            };
            rt.block_on(rebase::single(&s.git, &exe, id(&ids[*k]), action))
        }
        Op::Move(k, newer) => rt.block_on(rebase::move_commit(&s.git, &exe, id(&ids[*k]), *newer)),
        Op::FixupStaged(target, _) => {
            rt.block_on(rebase::fixup_staged_into(&s.git, id(&ids[*target])))
        }
    };
    if let Op::Reword(k, v) = case.op
        && k == n
    {
        sim.commits[k].1 = new_message(k, v);
    }

    let mut stop = sim.run();
    let mut choices = case.stops.iter();
    loop {
        let o = outcome.map_err(|e| fail("the rebase", &e))?;
        let stop_here = match (o, stop) {
            (Outcome::Done, None) => break,
            (Outcome::Stopped, Some(stop)) => stop,
            (o, stop) => {
                return Err(TestCaseError::fail(format!(
                    "the rebase gave {o:?}, but the model says {stop:?}"
                )));
            }
        };

        let k = match &stop_here {
            Stop::Edit(k) | Stop::Conflict { k, .. } => *k,
        };
        // The commit that fixes up with the staged changes didn't exist
        // when the case started. The rebase began from it.
        let stopped = if k > n {
            std::fs::read_to_string(git_dir.join("rebase-merge/orig-head"))
                .expect("orig-head")
                .trim()
                .to_string()
        } else {
            ids[k].clone()
        };
        let state = rebase::state_in(&git_dir);
        prop_assert_eq!(
            &state,
            &RepoState::Rebase {
                head_name: Some(BString::from("main")),
                step: Some(sim.done as u32),
                total: Some((sim.done + sim.todo.len()) as u32),
                stopped: Some(stopped),
            }
        );

        let (resolve, at_edit) = choices
            .next()
            .copied()
            .unwrap_or((Resolve::Ours, AtEdit::Continue));
        let abort = match stop_here {
            Stop::Conflict { .. } => matches!(resolve, Resolve::Abort),
            Stop::Edit(_) => matches!(at_edit, AtEdit::Abort),
        };
        if abort {
            rt.block_on(rebase::abort_op(&s.git, &state))
                .map_err(|e| fail("aborting", &e))?;
            prop_assert_eq!(rebase::state_in(&git_dir), RepoState::Clean);
            prop_assert!(!s.file(".git/tergi").exists(), ".git/tergi is still there");
            prop_assert_eq!(snapshot(&s), before);
            return Ok(());
        }
        let skip = match stop_here {
            Stop::Conflict {
                k,
                meld,
                files,
                ours,
                theirs,
                edit,
            } => {
                let shared = match resolve {
                    Resolve::Ours | Resolve::Theirs => {
                        let ours_side = matches!(resolve, Resolve::Ours);
                        rt.block_on(ops::take_side(&s.git, b"shared.txt", ours_side))
                            .map_err(|e| fail("take_side", &e))?;
                        if ours_side { ours } else { theirs }
                    }
                    Resolve::Edit => {
                        write_shared(&s, &edit);
                        rt.block_on(ops::stage_paths(&s.git, &["shared.txt"]))
                            .map_err(|e| fail("staging", &e))?;
                        Some(edit)
                    }
                    Resolve::Skip | Resolve::Abort => None,
                };
                if matches!(resolve, Resolve::Skip) {
                    sim.skip();
                    true
                } else {
                    sim.resolve(k, Tree { shared, files }, meld);
                    false
                }
            }
            Stop::Edit(k) => {
                if matches!(at_edit, AtEdit::Amend) {
                    let name = format!("e{k}.txt");
                    s.write(&name, &content(&name));
                    rt.block_on(ops::stage_paths(&s.git, &[&name]))
                        .map_err(|e| fail("staging", &e))?;
                    let last = sim.commits.last_mut().expect("the commit to amend");
                    last.0 = last.0.with(&name);
                }
                matches!(at_edit, AtEdit::Skip)
            }
        };
        outcome = if skip {
            rt.block_on(rebase::skip_op(&s.git, &state))
        } else {
            rt.block_on(rebase::continue_op(&s.git, &state))
        };
        stop = sim.run();
    }

    prop_assert_eq!(rebase::state_in(&git_dir), RepoState::Clean);
    prop_assert!(!s.file(".git/tergi").exists(), ".git/tergi is still there");
    prop_assert_eq!(s.sh(&["symbolic-ref", "HEAD"]), "refs/heads/main\n");
    let got: Vec<(Tree, String)> = s
        .sh(&["rev-list", "--reverse", "HEAD"])
        .lines()
        .map(|c| (stored_tree(&s, c), stored_message(&s, c)))
        .collect();
    prop_assert_eq!(got, sim.commits.clone());
    prop_assert_eq!(worktree(s.path()), expected_worktree(&sim.head(), dirty));
    // The stash that `--autostash` made only had changes that weren't
    // staged, so nothing is staged afterwards.
    prop_assert_eq!(s.sh(&["diff-index", "--cached", "--name-only", "HEAD"]), "");
    Ok(())
}

common::split_proptest! {
    config = ProptestConfig::default();
    parts = [part_1 part_2 part_3 part_4 part_5 part_6 part_7 part_8];

    fn rebases_that_stop(case in case()) {
        check(&case)?;
    }
}
