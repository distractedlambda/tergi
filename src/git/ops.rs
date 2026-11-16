//! Operations that change the repository. They all run the git CLI.
//!
//! A ref name can start with a dash, for example a tag fetched from
//! someone else's repository. Git would read a name like `-f` or
//! `--exec=cmd` as an option, so we put `--end-of-options` in front of
//! every ref and revision we pass.

use anyhow::{Result, bail};
use gix::ObjectId;

use super::cli::{Git, RunOpts};
use super::diff::{FileDiff, LineKind};
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
pub async fn apply_lines(
    git: &Git,
    file: &FileDiff,
    mode: ApplyMode,
    selected: &(dyn Fn(usize, usize) -> bool + Sync),
) -> Result<()> {
    let reverse = mode != ApplyMode::Stage;
    let Some(patch) = build_patch(file, selected, reverse)? else {
        bail!("No changed lines are selected.");
    };
    let mut args = vec!["apply", "--recount", "--whitespace=nowarn"];
    // Without context lines, `git apply` can't check where a hunk belongs,
    // so it refuses the patch unless we say that this is expected. A diff
    // has no context lines at all when `context_lines` is 0.
    let has_context = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .any(|l| l.kind == LineKind::Context);
    if !has_context {
        args.push("--unidiff-zero");
    }
    match mode {
        ApplyMode::Stage => args.push("--cached"),
        ApplyMode::Unstage => args.extend(["--cached", "--reverse"]),
        ApplyMode::Discard => args.push("--reverse"),
    }
    args.push("-");
    git.run_opts(
        &args,
        &RunOpts {
            stdin: Some(patch),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Git reads paths on the command line as patterns, so `[ab].txt` also
/// matches `a.txt`, and a leading colon starts special syntax. Mark such
/// paths as literal so a command only touches the file we mean.
pub fn pathspec(path: &str) -> String {
    if path.starts_with(':') || path.contains(['*', '?', '[', '\\']) {
        format!(":(literal){path}")
    } else {
        path.to_string()
    }
}

/// Run a git command on a list of paths.
async fn run_on_paths(git: &Git, command: &[&str], paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let specs: Vec<String> = paths.iter().map(|p| pathspec(p)).collect();
    let mut args = command.to_vec();
    args.push("--");
    args.extend(specs.iter().map(String::as_str));
    git.run(&args).await?;
    Ok(())
}

pub async fn stage_paths(git: &Git, paths: &[&str]) -> Result<()> {
    run_on_paths(git, &["add", "-A"], paths).await
}

pub async fn stage_all(git: &Git) -> Result<()> {
    git.run(&["add", "-A"]).await?;
    Ok(())
}

pub async fn unstage_paths(git: &Git, paths: &[&str], has_head: bool) -> Result<()> {
    if has_head {
        run_on_paths(git, &["restore", "--staged"], paths).await
    } else {
        // Before the first commit there's nothing to restore from. Without
        // `-f`, git refuses when the file changed after it was staged. With
        // `--cached` the file itself is never touched.
        run_on_paths(git, &["rm", "--cached", "-r", "-f", "-q"], paths).await
    }
}

pub async fn unstage_all(git: &Git, has_head: bool) -> Result<()> {
    if has_head {
        git.run(&["reset", "-q"]).await?;
    } else {
        git.run(&["rm", "--cached", "-r", "-f", "-q", "."]).await?;
    }
    Ok(())
}

/// The branch that is checked out, or `None` when HEAD is detached.
pub async fn head_branch(git: &Git) -> Option<String> {
    let name = git
        .read_text(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()?;
    Some(name.trim().to_string())
}

/// Throw away unstaged changes to tracked files.
pub async fn discard_paths(git: &Git, paths: &[&str]) -> Result<()> {
    run_on_paths(git, &["restore", "--worktree"], paths).await
}

/// Delete untracked files.
pub async fn delete_untracked(git: &Git, paths: &[&str]) -> Result<()> {
    run_on_paths(git, &["clean", "-f", "-q"], paths).await
}

pub async fn commit(git: &Git, message: &str, amend: bool) -> Result<String> {
    // `--cleanup=strip` would also remove every line that starts with "#",
    // such as a subject like "#42 Fix the crash". Nothing we pass in has
    // comment lines, so only tidy up the whitespace.
    let mut args = vec!["commit", "-F", "-", "--cleanup=whitespace"];
    if amend {
        args.push("--amend");
    }
    let out = git
        .run_opts(
            &args,
            &RunOpts {
                stdin: Some(message.as_bytes().to_vec()),
                ..Default::default()
            },
        )
        .await?;
    // Hooks can print anything, but git's own summary line looks like
    // "[main 1147b8e] Subject", and it's the one worth showing.
    let text = out.text();
    let summary = text
        .lines()
        .find(|l| l.starts_with('['))
        .unwrap_or("Committed");
    Ok(summary.to_string())
}

/// The full message of a commit, for amending and rewording.
pub async fn commit_message(git: &Git, rev: &str) -> Result<String> {
    Ok(git
        .read_text(&["log", "-1", "--format=%B", "--end-of-options", rev])
        .await?
        .trim_end()
        .to_string())
}

pub async fn checkout(git: &Git, target: &str) -> Result<()> {
    git.run(&["checkout", "--end-of-options", target]).await?;
    Ok(())
}

/// Check out a remote branch, creating or reusing a local tracking branch.
pub async fn checkout_remote(git: &Git, remote_branch: &str, local_exists: bool) -> Result<()> {
    let local = remote_branch
        .split_once('/')
        .map_or(remote_branch, |(_, b)| b);
    if local_exists {
        git.run(&["checkout", "--end-of-options", local]).await?;
    } else {
        git.run(&["checkout", "--track", "--end-of-options", remote_branch])
            .await?;
    }
    Ok(())
}

pub async fn create_branch(git: &Git, name: &str, at: &str, checkout: bool) -> Result<()> {
    if checkout {
        git.run(&["checkout", "-b", name, "--end-of-options", at])
            .await?;
    } else {
        git.run(&["branch", "--end-of-options", name, at]).await?;
    }
    Ok(())
}

pub async fn rename_branch(git: &Git, old: &str, new: &str) -> Result<()> {
    git.run(&["branch", "-m", "--end-of-options", old, new])
        .await?;
    Ok(())
}

pub async fn delete_branch(git: &Git, name: &str, force: bool) -> Result<()> {
    git.run(&[
        "branch",
        if force { "-D" } else { "-d" },
        "--end-of-options",
        name,
    ])
    .await?;
    Ok(())
}

pub async fn delete_remote_branch(git: &Git, remote: &str, name: &str) -> Result<()> {
    git.run(&["push", "--delete", "--end-of-options", remote, name])
        .await?;
    Ok(())
}

pub async fn set_upstream(git: &Git, branch: &str, upstream: &str) -> Result<()> {
    git.run(&[
        "branch",
        "--set-upstream-to",
        upstream,
        "--end-of-options",
        branch,
    ])
    .await?;
    Ok(())
}

pub async fn create_tag(git: &Git, name: &str, at: &str, message: Option<&str>) -> Result<()> {
    match message {
        Some(m) if !m.trim().is_empty() => {
            git.run(&["tag", "-a", "-m", m, "--end-of-options", name, at])
                .await?
        }
        _ => git.run(&["tag", "--end-of-options", name, at]).await?,
    };
    Ok(())
}

pub async fn delete_tag(git: &Git, name: &str) -> Result<()> {
    git.run(&["tag", "-d", "--end-of-options", name]).await?;
    Ok(())
}

pub async fn merge(git: &Git, rev: &str) -> Result<()> {
    git.run(&["merge", "--no-edit", "--end-of-options", rev])
        .await?;
    Ok(())
}

pub async fn cherry_pick(git: &Git, rev: &str) -> Result<()> {
    git.run(&["cherry-pick", "--end-of-options", rev]).await?;
    Ok(())
}

pub async fn revert(git: &Git, rev: &str) -> Result<()> {
    git.run(&["revert", "--no-edit", "--end-of-options", rev])
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

pub async fn reset(git: &Git, rev: &str, mode: ResetMode) -> Result<()> {
    let flag = match mode {
        ResetMode::Soft => "--soft",
        ResetMode::Mixed => "--mixed",
        ResetMode::Hard => "--hard",
    };
    git.run(&["reset", "-q", flag, "--end-of-options", rev])
        .await?;
    Ok(())
}

pub async fn stash_push(
    git: &Git,
    message: &str,
    include_untracked: bool,
    staged_only: bool,
) -> Result<()> {
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
    git.run(&args).await?;
    Ok(())
}

pub async fn stash_apply(git: &Git, stash: ObjectId, pop: bool) -> Result<()> {
    git.run(&[
        "stash",
        if pop { "pop" } else { "apply" },
        &stash_ref(git, stash).await?,
    ])
    .await?;
    Ok(())
}

pub async fn stash_drop(git: &Git, stash: ObjectId) -> Result<()> {
    git.run(&["stash", "drop", &stash_ref(git, stash).await?])
        .await?;
    Ok(())
}

/// Find the current `stash@{n}` name of a stash. Pushing or dropping a
/// stash elsewhere renumbers the others, so an index from an earlier
/// listing could name a different stash. Pop and drop only accept these
/// names, not a commit id.
async fn stash_ref(git: &Git, stash: ObjectId) -> Result<String> {
    let out = git.read_text(&["stash", "list", "--format=%H"]).await?;
    match out
        .lines()
        .position(|l| ObjectId::from_hex(l.as_bytes()).ok() == Some(stash))
    {
        Some(i) => Ok(format!("stash@{{{i}}}")),
        None => bail!("The stash no longer exists."),
    }
}

/// Resolve a conflicted file by taking one side.
pub async fn take_side(git: &Git, path: &str, ours: bool) -> Result<()> {
    let spec = pathspec(path);
    // Unmerged entries are listed as "<mode> <id> <stage>\t<path>", where
    // stage 2 is our version and stage 3 is theirs. A side that deleted the
    // file has no entry, and taking that side means deleting the file.
    let entries = git
        .read_text(&["ls-files", "--unmerged", "-z", "--", &spec])
        .await?;
    let stage = if ours { "2" } else { "3" };
    let has_side = entries
        .split('\0')
        .filter_map(|e| e.split_once('\t'))
        .any(|(meta, _)| meta.split(' ').nth(2) == Some(stage));
    if has_side {
        git.run(&[
            "checkout",
            if ours { "--ours" } else { "--theirs" },
            "--",
            &spec,
        ])
        .await?;
        git.run(&["add", "--", &spec]).await?;
    } else {
        git.run(&["rm", "-q", "--", &spec]).await?;
    }
    Ok(())
}

pub async fn is_ancestor(git: &Git, ancestor: &str, descendant: &str) -> bool {
    git.read(&[
        "merge-base",
        "--is-ancestor",
        "--end-of-options",
        ancestor,
        descendant,
    ])
    .await
    .is_ok()
}
