//! Rewriting history with `git rebase -i`, driven without an editor.
//!
//! We write the todo list ourselves and point `GIT_SEQUENCE_EDITOR` at
//! `tergi __seq-editor <our todo>`, which swaps it in. Commit messages for
//! reword and squash are applied with `exec git commit --amend -F <file>`
//! lines, so git never needs to open an editor.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use super::cli::{Git, RunOpts, shell_quote};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Pick,
    /// Keep the commit but give it this message.
    Reword(String),
    /// Meld into the previous commit and use this message for the result.
    Squash(String),
    /// Meld into the previous commit and keep its message.
    Fixup,
    Drop,
    /// Stop after applying this commit so it can be changed.
    Edit,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword(_) => "reword",
            Action::Squash(_) => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
            Action::Edit => "edit",
        }
    }

    pub fn melds(&self) -> bool {
        matches!(self, Action::Squash(_) | Action::Fixup)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: ObjectId,
    pub summary: String,
    pub action: Action,
}

/// A linear run of commits to rewrite, oldest first, on top of `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The commit the rewritten commits are replayed onto. `None` means the
    /// range starts at the root commit.
    pub base: Option<ObjectId>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The rebase stopped, for an `edit` step or a conflict.
    Stopped,
}

/// The commits from `oldest` up to HEAD, which must be a straight line.
pub async fn plan_from(git: &Git, oldest: ObjectId) -> Result<Plan> {
    let oldest_hex = oldest.to_string();
    let parents = git.read_text(&["rev-list", "--parents", "-n1", &oldest_hex]).await?;
    let parent_ids: Vec<&str> = parents.split_whitespace().skip(1).collect();
    if parent_ids.len() > 1 {
        bail!("Editing merge commits isn't supported.");
    }
    if !super::ops::is_ancestor(git, &oldest_hex, "HEAD").await {
        bail!("That commit isn't part of the current branch.");
    }
    let base = parent_ids
        .first()
        .map(|p| ObjectId::from_hex(p.as_bytes()))
        .transpose()
        .context("bad commit id")?;
    let range = match base {
        Some(b) => format!("{b}..HEAD"),
        None => "HEAD".to_string(),
    };
    let out = git.read_text(&["rev-list", "--reverse", "--topo-order", "--parents", "--format=%s", &range]).await?;
    let mut steps = Vec::new();
    let mut lines = out.lines();
    while let Some(header) = lines.next() {
        let Some(ids) = header.strip_prefix("commit ") else {
            continue;
        };
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if ids.len() > 2 {
            bail!("There's a merge commit between that commit and HEAD, which isn't supported yet.");
        }
        let summary = lines.next().unwrap_or("").to_string();
        steps.push(Step {
            id: ObjectId::from_hex(ids[0].as_bytes()).context("bad commit id")?,
            summary,
            action: Action::Pick,
        });
    }
    Ok(Plan { base, steps })
}

/// Write the todo list for `plan`, with message files in `dir`.
pub fn build_todo(plan: &Plan, dir: &Path) -> Result<String> {
    if plan.steps.first().is_some_and(|s| s.action.melds()) {
        bail!("The oldest commit can't be squashed or fixed up, because there's nothing before it to meld into.");
    }
    let mut todo = String::new();
    for (i, step) in plan.steps.iter().enumerate() {
        let id = step.id;
        let summary = step.summary.replace('\n', " ");
        match &step.action {
            Action::Pick => todo.push_str(&format!("pick {id} {summary}\n")),
            Action::Edit => todo.push_str(&format!("edit {id} {summary}\n")),
            Action::Drop => todo.push_str(&format!("drop {id} {summary}\n")),
            Action::Fixup => todo.push_str(&format!("fixup {id} {summary}\n")),
            Action::Reword(msg) | Action::Squash(msg) => {
                let verb = if matches!(step.action, Action::Reword(_)) { "pick" } else { "fixup" };
                let file = dir.join(format!("msg-{i}"));
                std::fs::write(&file, msg).context("could not write message file")?;
                todo.push_str(&format!("{verb} {id} {summary}\n"));
                todo.push_str(&format!(
                    "exec git commit --amend --allow-empty --no-verify --cleanup=whitespace -F {}\n",
                    shell_quote(&file.to_string_lossy())
                ));
            }
        }
    }
    Ok(todo)
}

async fn scratch_dir(git: &Git) -> Result<PathBuf> {
    let git_dir = git.read_text(&["rev-parse", "--absolute-git-dir"]).await?;
    let dir = PathBuf::from(git_dir.trim()).join("tergi");
    tokio::fs::create_dir_all(&dir).await.context("could not create scratch dir")?;
    Ok(dir)
}

/// Fail if a rebase, merge, cherry-pick or revert is unfinished. Git would
/// refuse to start a rebase then anyway, but `finish` would see the old
/// operation and report that the new rebase stopped.
async fn ensure_idle(git: &Git) -> Result<()> {
    let what = match state(git).await {
        RepoState::Clean => return Ok(()),
        RepoState::Rebase { .. } => "rebase",
        RepoState::Merge => "merge",
        RepoState::CherryPick => "cherry-pick",
        RepoState::Revert => "revert",
    };
    bail!("A {what} is in progress. Continue or abort it first.");
}

/// Run the rebase described by `plan`. `exe` is the tergi executable, used
/// as the sequence editor.
pub async fn run(git: &Git, plan: &Plan, exe: &Path) -> Result<Outcome> {
    if plan.steps.is_empty() {
        return Ok(Outcome::Done);
    }
    ensure_idle(git).await?;
    let dir = scratch_dir(git).await?;
    let todo = build_todo(plan, &dir)?;
    let todo_file = dir.join("todo");
    tokio::fs::write(&todo_file, todo).await.context("could not write todo")?;
    let editor = format!(
        "{} __seq-editor {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&todo_file.to_string_lossy())
    );
    let base = plan.base.map(|b| b.to_string());
    let mut args = vec!["rebase", "-i", "--autostash"];
    match &base {
        Some(b) => args.push(b),
        None => args.push("--root"),
    }
    let result = git.run_opts(
        &args,
        &RunOpts {
            env: vec![("GIT_SEQUENCE_EDITOR".into(), editor)],
            ..Default::default()
        },
    ).await;
    finish(git, result.map(|_| ())).await
}

async fn finish(git: &Git, result: Result<()>) -> Result<Outcome> {
    let stopped = matches!(state(git).await, RepoState::Rebase { .. });
    match result {
        Ok(()) if stopped => Ok(Outcome::Stopped),
        Ok(()) => Ok(Outcome::Done),
        Err(_) if stopped => Ok(Outcome::Stopped),
        Err(e) => Err(e),
    }
}

/// Called as `tergi __seq-editor <prepared> <git's todo>`. Replace git's todo
/// with ours, after checking both mention the same commits. If they don't,
/// the history changed under us and the rebase is aborted.
pub fn seq_editor_main(prepared: &Path, target: &Path) -> Result<()> {
    let ours = std::fs::read_to_string(prepared).context("could not read prepared todo")?;
    let theirs = std::fs::read_to_string(target).context("could not read git's todo")?;
    let ids = |s: &str| -> Vec<String> {
        let mut v: Vec<String> = s
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let verb = it.next()?;
                matches!(verb, "pick" | "p" | "fixup" | "f" | "drop" | "d" | "edit" | "e" | "squash" | "s" | "reword" | "r")
                    .then(|| it.next().map(str::to_string))
                    .flatten()
            })
            .collect();
        v.sort();
        v
    };
    let a = ids(&ours);
    let b = ids(&theirs);
    let same = a.len() == b.len()
        && a.iter().all(|full| b.iter().any(|short| full.starts_with(short.as_str())));
    if !same {
        bail!("The commits to rebase changed since tergi planned this rebase.");
    }
    std::fs::write(target, ours).context("could not write todo")?;
    Ok(())
}

pub async fn continue_rebase(git: &Git) -> Result<Outcome> {
    let r = git.run(&["rebase", "--continue"]).await.map(|_| ());
    finish(git, r).await
}

pub async fn skip_rebase(git: &Git) -> Result<Outcome> {
    let r = git.run(&["rebase", "--skip"]).await.map(|_| ());
    finish(git, r).await
}

pub async fn abort_rebase(git: &Git) -> Result<()> {
    git.run(&["rebase", "--abort"]).await?;
    Ok(())
}

/// Rebase the current branch onto another commit.
pub async fn rebase_onto(git: &Git, upstream: &str) -> Result<Outcome> {
    ensure_idle(git).await?;
    let r = git.run(&["rebase", "--autostash", upstream]).await.map(|_| ());
    finish(git, r).await
}

/// Commit the staged changes as a fixup of `target` and squash them in.
pub async fn fixup_staged_into(git: &Git, target: ObjectId) -> Result<Outcome> {
    let t = target.to_string();
    if !super::ops::is_ancestor(git, &t, "HEAD").await {
        bail!("That commit isn't part of the current branch.");
    }
    ensure_idle(git).await?;
    let plan = plan_from(git, target).await?;
    git.run(&["commit", "--no-verify", &format!("--fixup={t}")]).await?;
    let fixup = git.read_text(&["rev-parse", "HEAD"]).await?;
    let base = plan.base.map(|b| b.to_string());
    let mut args = vec!["rebase", "-i", "--autosquash", "--autostash"];
    match &base {
        Some(b) => args.push(b),
        None => args.push("--root"),
    }
    let r = git
        .run_opts(
            &args,
            &RunOpts {
                env: vec![("GIT_SEQUENCE_EDITOR".into(), "true".into())],
                ..Default::default()
            },
        ).await
        .map(|_| ());
    if r.is_err()
        && state(git).await == RepoState::Clean
        && git.read_text(&["rev-parse", "HEAD"]).await.is_ok_and(|head| head == fixup)
    {
        // The rebase never started. Take the fixup commit off again, so the
        // changes are staged as they were.
        let _ = git.run(&["reset", "--soft", "HEAD~1"]).await;
    }
    finish(git, r).await
}

/// Change the message of a single commit.
pub async fn reword(git: &Git, exe: &Path, id: ObjectId, message: &str) -> Result<Outcome> {
    let head = git.read_text(&["rev-parse", "HEAD"]).await?;
    if ObjectId::from_hex(head.trim().as_bytes()).ok() == Some(id) {
        git.run_opts(
            &["commit", "--amend", "--only", "--allow-empty", "--cleanup=whitespace", "-F", "-"],
            &RunOpts {
                stdin: Some(message.as_bytes().to_vec()),
                ..Default::default()
            },
        ).await?;
        return Ok(Outcome::Done);
    }
    let mut plan = plan_from(git, id).await?;
    plan.steps[0].action = Action::Reword(message.to_string());
    run(git, &plan, exe).await
}

/// Apply one action to a single commit, keeping everything else.
pub async fn single(git: &Git, exe: &Path, id: ObjectId, action: Action) -> Result<Outcome> {
    let mut plan = if action.melds() {
        // Meld into the parent, so the range has to start one earlier.
        let parent = git.read_text(&["rev-parse", &format!("{id}^")]).await?;
        let parent = ObjectId::from_hex(parent.trim().as_bytes()).context("commit has no parent")?;
        plan_from(git, parent).await?
    } else {
        plan_from(git, id).await?
    };
    let step = plan
        .steps
        .iter_mut()
        .find(|s| s.id == id)
        .context("commit not found in range")?;
    step.action = action;
    run(git, &plan, exe).await
}

/// Move a commit one step towards HEAD (`newer`) or away from it.
pub async fn move_commit(git: &Git, exe: &Path, id: ObjectId, newer: bool) -> Result<Outcome> {
    let start = if newer {
        id
    } else {
        let parent = git.read_text(&["rev-parse", &format!("{id}^")]).await?;
        ObjectId::from_hex(parent.trim().as_bytes()).context("commit has no parent")?
    };
    let mut plan = plan_from(git, start).await?;
    let pos = plan.steps.iter().position(|s| s.id == id).context("commit not found")?;
    let other = if newer { pos + 1 } else { pos.wrapping_sub(1) };
    if other >= plan.steps.len() {
        bail!("Can't move the commit any further.");
    }
    plan.steps.swap(pos, other);
    run(git, &plan, exe).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Rebase {
        head_name: Option<String>,
        step: Option<u32>,
        total: Option<u32>,
        /// The commit the rebase stopped at, for `edit` steps and conflicts.
        stopped: Option<String>,
    },
    Merge,
    CherryPick,
    Revert,
}

pub async fn state(git: &Git) -> RepoState {
    let Ok(dir) = git.read_text(&["rev-parse", "--absolute-git-dir"]).await else {
        return RepoState::Clean;
    };
    state_in(Path::new(dir.trim()))
}

pub fn state_in(git_dir: &Path) -> RepoState {
    let read = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    for sub in ["rebase-merge", "rebase-apply"] {
        let d = git_dir.join(sub);
        if d.is_dir() {
            let (step, total) = if sub == "rebase-merge" {
                (read(&d.join("msgnum")), read(&d.join("end")))
            } else {
                (read(&d.join("next")), read(&d.join("last")))
            };
            return RepoState::Rebase {
                head_name: read(&d.join("head-name"))
                    .map(|h| h.strip_prefix("refs/heads/").unwrap_or(&h).to_string()),
                step: step.and_then(|s| s.parse().ok()),
                total: total.and_then(|s| s.parse().ok()),
                stopped: read(&d.join("stopped-sha")),
            };
        }
    }
    if git_dir.join("MERGE_HEAD").exists() {
        RepoState::Merge
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        RepoState::CherryPick
    } else if git_dir.join("REVERT_HEAD").exists() {
        RepoState::Revert
    } else {
        RepoState::Clean
    }
}

pub async fn continue_op(git: &Git, state: &RepoState) -> Result<Outcome> {
    match state {
        RepoState::Rebase { .. } => continue_rebase(git).await,
        RepoState::Merge => {
            git.run(&["merge", "--continue"]).await?;
            Ok(Outcome::Done)
        }
        RepoState::CherryPick => {
            git.run(&["cherry-pick", "--continue"]).await?;
            Ok(Outcome::Done)
        }
        RepoState::Revert => {
            git.run(&["revert", "--continue"]).await?;
            Ok(Outcome::Done)
        }
        RepoState::Clean => Ok(Outcome::Done),
    }
}

pub async fn abort_op(git: &Git, state: &RepoState) -> Result<()> {
    match state {
        RepoState::Rebase { .. } => abort_rebase(git).await,
        RepoState::Merge => git.run(&["merge", "--abort"]).await.map(|_| ()),
        RepoState::CherryPick => git.run(&["cherry-pick", "--abort"]).await.map(|_| ()),
        RepoState::Revert => git.run(&["revert", "--abort"]).await.map(|_| ()),
        RepoState::Clean => Ok(()),
    }
}

pub async fn skip_op(git: &Git, state: &RepoState) -> Result<Outcome> {
    match state {
        RepoState::Rebase { .. } => skip_rebase(git).await,
        RepoState::CherryPick => {
            git.run(&["cherry-pick", "--skip"]).await?;
            Ok(Outcome::Done)
        }
        RepoState::Revert => {
            git.run(&["revert", "--skip"]).await?;
            Ok(Outcome::Done)
        }
        _ => Ok(Outcome::Done),
    }
}
