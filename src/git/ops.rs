//! Operations that change the repository. They all run the git CLI.
//!
//! A ref name can start with a dash, for example a tag fetched from
//! someone else's repository. Git would read a name like `-f` or
//! `--exec=cmd` as an option, so we put `--end-of-options` in front of
//! every ref and revision we pass.
//!
//! Ref names needn't be UTF-8, so the functions that take one take bytes.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt as _;

use anyhow::{Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::cli::{Git, RunOpts};
use super::diff::{DiffLine, FileDiff, LineKind};
use super::patch::build_patch;
use crate::util::os_str;

/// A ref name or other bytes as an argument for git.
fn arg(name: &impl AsRef<[u8]>) -> &OsStr {
    os_str(name.as_ref())
}

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
    // Without context lines around a hunk, `git apply` can't check where
    // it belongs, so it refuses the patch unless we say that this is
    // expected. That happens when `context_lines` is 0. Such a hunk can
    // still have context lines in the middle, because git joins hunks that
    // are close together when the user sets `diff.interHunkContext`. With
    // more context, only a hunk that covers the whole file starts and ends
    // with a change.
    let changed = |l: Option<&DiffLine>| l.is_some_and(|l| l.kind != LineKind::Context);
    let zero_context = file
        .hunks
        .iter()
        .all(|h| changed(h.lines.first()) && changed(h.lines.last()));
    if zero_context {
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
/// matches `a.txt`, and a leading colon starts special syntax. Mark every
/// path as literal so a command only touches the file we mean.
///
/// When the user has set `GIT_LITERAL_PATHSPECS`, git already reads every
/// path literally, and it would take the prefix as part of the file name.
/// Git children get our environment, so we can check it here.
///
/// The path is bytes, and so is the result, because a file name needn't be
/// UTF-8.
pub fn pathspec(path: &[u8]) -> OsString {
    pathspec_with(
        path,
        env_true(std::env::var_os("GIT_LITERAL_PATHSPECS").as_deref()),
    )
}

fn pathspec_with(path: &[u8], literal: bool) -> OsString {
    let prefix: &[u8] = if literal { b"" } else { b":(literal)" };
    OsString::from_vec([prefix, path].concat())
}

/// Whether git reads this value of a boolean environment variable as true.
/// Git also accepts numbers with a unit, such as `1k`. We read those as
/// false. A path then keeps its prefix, so the command fails instead of
/// matching other files.
fn env_true(value: Option<&std::ffi::OsStr>) -> bool {
    let Some(v) = value.and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    ["true", "yes", "on"]
        .iter()
        .any(|t| v.eq_ignore_ascii_case(t))
        || v.parse::<i64>().is_ok_and(|n| n != 0)
}

/// Run a git command on a list of paths.
async fn run_on_paths(git: &Git, command: &[&str], paths: &[impl AsRef<[u8]>]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args: Vec<OsString> = command.iter().map(OsString::from).collect();
    args.push("--".into());
    args.extend(paths.iter().map(|p| pathspec(p.as_ref())));
    git.run(&args).await?;
    Ok(())
}

pub async fn stage_paths(git: &Git, paths: &[impl AsRef<[u8]>]) -> Result<()> {
    run_on_paths(git, &["add", "-A"], paths).await
}

pub async fn stage_all(git: &Git) -> Result<()> {
    git.run(&["add", "-A"]).await?;
    Ok(())
}

pub async fn unstage_paths(git: &Git, paths: &[impl AsRef<[u8]>], has_head: bool) -> Result<()> {
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
pub async fn head_branch(git: &Git) -> Option<BString> {
    let out = git
        .read(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()?;
    Some(out.stdout.trim_ascii_end().into())
}

/// Throw away unstaged changes to tracked files.
pub async fn discard_paths(git: &Git, paths: &[impl AsRef<[u8]>]) -> Result<()> {
    run_on_paths(git, &["restore", "--worktree"], paths).await
}

/// Delete untracked files.
pub async fn delete_untracked(git: &Git, paths: &[impl AsRef<[u8]>]) -> Result<()> {
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

pub async fn checkout(git: &Git, target: impl AsRef<[u8]>) -> Result<()> {
    git.run(&[
        OsStr::new("checkout"),
        OsStr::new("--end-of-options"),
        arg(&target),
    ])
    .await?;
    Ok(())
}

/// Check out branch `name` of `remote`, creating or reusing a local
/// tracking branch with the same name.
///
/// We name the remote branch by its full ref. A remote can have a slash
/// in its name, and a short name like `a/b/main` could also be a local
/// branch.
pub async fn checkout_remote(
    git: &Git,
    remote: impl AsRef<[u8]>,
    name: impl AsRef<[u8]>,
    local_exists: bool,
) -> Result<()> {
    let name = name.as_ref();
    if local_exists {
        checkout(git, name).await
    } else {
        let full = [b"refs/remotes/", remote.as_ref(), b"/", name].concat();
        git.run(&[
            OsStr::new("checkout"),
            OsStr::new("--track"),
            OsStr::new("-b"),
            os_str(name),
            OsStr::new("--end-of-options"),
            os_str(&full),
        ])
        .await?;
        Ok(())
    }
}

/// Create a branch named `name`, which the user typed, at the ref or
/// commit `at`.
pub async fn create_branch(
    git: &Git,
    name: &str,
    at: impl AsRef<[u8]>,
    checkout: bool,
) -> Result<()> {
    if checkout {
        git.run(&[
            OsStr::new("checkout"),
            OsStr::new("-b"),
            OsStr::new(name),
            OsStr::new("--end-of-options"),
            arg(&at),
        ])
        .await?;
    } else {
        git.run(&[
            OsStr::new("branch"),
            OsStr::new("--end-of-options"),
            OsStr::new(name),
            arg(&at),
        ])
        .await?;
    }
    Ok(())
}

pub async fn rename_branch(git: &Git, old: impl AsRef<[u8]>, new: &str) -> Result<()> {
    git.run(&[
        OsStr::new("branch"),
        OsStr::new("-m"),
        OsStr::new("--end-of-options"),
        arg(&old),
        OsStr::new(new),
    ])
    .await?;
    Ok(())
}

pub async fn delete_branch(git: &Git, name: impl AsRef<[u8]>, force: bool) -> Result<()> {
    git.run(&[
        OsStr::new("branch"),
        OsStr::new(if force { "-D" } else { "-d" }),
        OsStr::new("--end-of-options"),
        arg(&name),
    ])
    .await?;
    Ok(())
}

pub async fn delete_remote_branch(
    git: &Git,
    remote: impl AsRef<[u8]>,
    name: impl AsRef<[u8]>,
) -> Result<()> {
    git.run(&[
        OsStr::new("push"),
        OsStr::new("--delete"),
        OsStr::new("--end-of-options"),
        arg(&remote),
        arg(&name),
    ])
    .await?;
    Ok(())
}

/// Make `branch` track `upstream`, which the user typed.
pub async fn set_upstream(git: &Git, branch: impl AsRef<[u8]>, upstream: &str) -> Result<()> {
    git.run(&[
        OsStr::new("branch"),
        OsStr::new("--set-upstream-to"),
        OsStr::new(upstream),
        OsStr::new("--end-of-options"),
        arg(&branch),
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

pub async fn delete_tag(git: &Git, name: impl AsRef<[u8]>) -> Result<()> {
    git.run(&[
        OsStr::new("tag"),
        OsStr::new("-d"),
        OsStr::new("--end-of-options"),
        arg(&name),
    ])
    .await?;
    Ok(())
}

pub async fn merge(git: &Git, rev: impl AsRef<[u8]>) -> Result<()> {
    git.run(&[
        OsStr::new("merge"),
        OsStr::new("--no-edit"),
        OsStr::new("--end-of-options"),
        arg(&rev),
    ])
    .await?;
    Ok(())
}

/// Cherry-pick commits in the order given.
pub async fn cherry_pick(git: &Git, revs: &[String]) -> Result<()> {
    let mut args = vec!["cherry-pick", "--end-of-options"];
    args.extend(revs.iter().map(String::as_str));
    git.run(&args).await?;
    Ok(())
}

/// Revert commits in the order given.
pub async fn revert(git: &Git, revs: &[String]) -> Result<()> {
    let mut args = vec!["revert", "--no-edit", "--end-of-options"];
    args.extend(revs.iter().map(String::as_str));
    git.run(&args).await?;
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
pub async fn take_side(git: &Git, path: &[u8], ours: bool) -> Result<()> {
    let spec = pathspec(path);
    let args = |cmd: &[&str]| -> Vec<OsString> {
        let mut args: Vec<OsString> = cmd.iter().map(OsString::from).collect();
        args.extend(["--".into(), spec.clone()]);
        args
    };
    // Unmerged entries are listed as "<mode> <id> <stage>\t<path>", where
    // stage 2 is our version and stage 3 is theirs. A side that deleted the
    // file has no entry, and taking that side means deleting the file.
    let entries = git
        .read_text(&args(&["ls-files", "--unmerged", "-z"]))
        .await?;
    let stage = if ours { "2" } else { "3" };
    let has_side = entries
        .split('\0')
        .filter_map(|e| e.split_once('\t'))
        .any(|(meta, _)| meta.split(' ').nth(2) == Some(stage));
    if has_side {
        let side = if ours { "--ours" } else { "--theirs" };
        git.run(&args(&["checkout", side])).await?;
        git.run(&args(&["add"])).await?;
    } else {
        git.run(&args(&["rm", "-q"])).await?;
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn literal_pathspecs_setting() {
        for v in ["1", "true", "YES", "on", "2", "-1"] {
            assert!(env_true(Some(OsStr::new(v))), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "1k", "junk"] {
            assert!(!env_true(Some(OsStr::new(v))), "{v}");
        }
        assert!(!env_true(None));
        assert_eq!(pathspec_with(b"a*", false), ":(literal)a*");
        assert_eq!(pathspec_with(b"a*", true), "a*");
        assert_eq!(
            pathspec_with(b"caf\xe9", false),
            OsString::from_vec(b":(literal)caf\xe9".to_vec())
        );
    }
}
