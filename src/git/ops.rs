//! Operations that change the repository. They all run the git CLI.

use anyhow::{Result, bail};

use super::cli::{Git, RunOpts};
use super::diff::FileDiff;
use super::patch::build_patch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// Apply a working tree diff to the index.
    Stage,
    /// Remove a staged diff from the index.
    Unstage,
    /// Remove an unstaged diff from the working tree.
    Discard,
}

/// Stage, unstage or discard the selected lines of `file`.
pub fn apply_lines(
    git: &Git,
    file: &FileDiff,
    mode: ApplyMode,
    selected: &dyn Fn(usize, usize) -> bool,
) -> Result<()> {
    let reverse = mode != ApplyMode::Stage;
    let Some(patch) = build_patch(file, selected, reverse)? else {
        bail!("No changed lines are selected.");
    };
    let mut args = vec!["apply", "--recount", "--whitespace=nowarn"];
    match mode {
        ApplyMode::Stage => args.push("--cached"),
        ApplyMode::Unstage => args.extend(["--cached", "--reverse"]),
        ApplyMode::Discard => args.push("--reverse"),
    }
    args.push("-");
    git.run_opts(
        &args,
        &RunOpts {
            stdin: Some(patch.into_bytes()),
            ..Default::default()
        },
    )?;
    Ok(())
}

pub fn stage_paths(git: &Git, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add", "-A", "--"];
    args.extend_from_slice(paths);
    git.run(&args)?;
    Ok(())
}

pub fn stage_all(git: &Git) -> Result<()> {
    git.run(&["add", "-A"])?;
    Ok(())
}

pub fn unstage_paths(git: &Git, paths: &[&str], has_head: bool) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = if has_head {
        vec!["restore", "--staged", "--"]
    } else {
        // Before the first commit there's nothing to restore from.
        vec!["rm", "--cached", "-r", "-q", "--"]
    };
    args.extend_from_slice(paths);
    git.run(&args)?;
    Ok(())
}

pub fn unstage_all(git: &Git, has_head: bool) -> Result<()> {
    if has_head {
        git.run(&["reset", "-q"])?;
    } else {
        git.run(&["rm", "--cached", "-r", "-q", "."])?;
    }
    Ok(())
}

/// Throw away unstaged changes to tracked files.
pub fn discard_paths(git: &Git, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["restore", "--worktree", "--"];
    args.extend_from_slice(paths);
    git.run(&args)?;
    Ok(())
}

/// Delete untracked files.
pub fn delete_untracked(git: &Git, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["clean", "-f", "-q", "--"];
    args.extend_from_slice(paths);
    git.run(&args)?;
    Ok(())
}

pub fn commit(git: &Git, message: &str, amend: bool) -> Result<String> {
    let mut args = vec!["commit", "-F", "-", "--cleanup=strip"];
    if amend {
        args.push("--amend");
    }
    let out = git.run_opts(
        &args,
        &RunOpts {
            stdin: Some(message.as_bytes().to_vec()),
            ..Default::default()
        },
    )?;
    // Hooks can print anything, but git's own summary line looks like
    // "[main 1147b8e] Subject", and it's the one worth showing.
    let text = out.text();
    let summary = text.lines().find(|l| l.starts_with('[')).unwrap_or("Committed");
    Ok(summary.to_string())
}

/// The full message of a commit, for amending and rewording.
pub fn commit_message(git: &Git, rev: &str) -> Result<String> {
    Ok(git.read_text(&["log", "-1", "--format=%B", rev])?.trim_end().to_string())
}

pub fn checkout(git: &Git, target: &str) -> Result<()> {
    git.run(&["checkout", target])?;
    Ok(())
}

/// Check out a remote branch, creating or reusing a local tracking branch.
pub fn checkout_remote(git: &Git, remote_branch: &str, local_exists: bool) -> Result<()> {
    let local = remote_branch.split_once('/').map_or(remote_branch, |(_, b)| b);
    if local_exists {
        git.run(&["checkout", local])?;
    } else {
        git.run(&["checkout", "--track", remote_branch])?;
    }
    Ok(())
}

pub fn create_branch(git: &Git, name: &str, at: &str, checkout: bool) -> Result<()> {
    if checkout {
        git.run(&["checkout", "-b", name, at])?;
    } else {
        git.run(&["branch", name, at])?;
    }
    Ok(())
}

pub fn rename_branch(git: &Git, old: &str, new: &str) -> Result<()> {
    git.run(&["branch", "-m", old, new])?;
    Ok(())
}

pub fn delete_branch(git: &Git, name: &str, force: bool) -> Result<()> {
    git.run(&["branch", if force { "-D" } else { "-d" }, name])?;
    Ok(())
}

pub fn delete_remote_branch(git: &Git, remote: &str, name: &str) -> Result<()> {
    git.run(&["push", remote, "--delete", name])?;
    Ok(())
}

pub fn set_upstream(git: &Git, branch: &str, upstream: &str) -> Result<()> {
    git.run(&["branch", "--set-upstream-to", upstream, branch])?;
    Ok(())
}

pub fn create_tag(git: &Git, name: &str, at: &str, message: Option<&str>) -> Result<()> {
    match message {
        Some(m) if !m.trim().is_empty() => git.run(&["tag", "-a", name, "-m", m, at])?,
        _ => git.run(&["tag", name, at])?,
    };
    Ok(())
}

pub fn delete_tag(git: &Git, name: &str) -> Result<()> {
    git.run(&["tag", "-d", name])?;
    Ok(())
}

pub fn merge(git: &Git, rev: &str) -> Result<()> {
    git.run(&["merge", "--no-edit", rev])?;
    Ok(())
}

pub fn cherry_pick(git: &Git, rev: &str) -> Result<()> {
    git.run(&["cherry-pick", rev])?;
    Ok(())
}

pub fn revert(git: &Git, rev: &str) -> Result<()> {
    git.run(&["revert", "--no-edit", rev])?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

pub fn reset(git: &Git, rev: &str, mode: ResetMode) -> Result<()> {
    let flag = match mode {
        ResetMode::Soft => "--soft",
        ResetMode::Mixed => "--mixed",
        ResetMode::Hard => "--hard",
    };
    git.run(&["reset", "-q", flag, rev])?;
    Ok(())
}

pub fn stash_push(git: &Git, message: &str, include_untracked: bool, staged_only: bool) -> Result<()> {
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if staged_only {
        args.push("--staged");
    }
    if !message.trim().is_empty() {
        args.extend(["-m", message]);
    }
    git.run(&args)?;
    Ok(())
}

pub fn stash_apply(git: &Git, stash: &str, pop: bool) -> Result<()> {
    git.run(&["stash", if pop { "pop" } else { "apply" }, stash])?;
    Ok(())
}

pub fn stash_drop(git: &Git, stash: &str) -> Result<()> {
    git.run(&["stash", "drop", stash])?;
    Ok(())
}

/// Resolve a conflicted file by taking one side.
pub fn take_side(git: &Git, path: &str, ours: bool) -> Result<()> {
    git.run(&["checkout", if ours { "--ours" } else { "--theirs" }, "--", path])?;
    git.run(&["add", "--", path])?;
    Ok(())
}

pub fn is_ancestor(git: &Git, ancestor: &str, descendant: &str) -> bool {
    git.read(&["merge-base", "--is-ancestor", ancestor, descendant])
        .is_ok()
}
