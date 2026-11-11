//! Loading the working directory state: status plus the unstaged, staged
//! and untracked diffs shown in the staging view.

use anyhow::Result;

use super::cli::{Git, RunOpts};
use super::commit::{FileTexts, MAX_BLOB, diff_texts, is_binary};
use super::diff::{FileDiff, FileStatus, parse_unified};
use super::status::{Entry, Status};

#[derive(Debug, Clone, Default)]
pub struct Workdir {
    pub status: Status,
    pub unstaged: Vec<FileDiff>,
    pub staged: Vec<FileDiff>,
    pub untracked: Vec<FileDiff>,
    pub conflicted: Vec<FileDiff>,
    /// Full text of the working tree version of each unstaged and untracked
    /// file, for syntax highlighting. Indexed like the lists above.
    pub unstaged_texts: Vec<FileTexts>,
    pub untracked_texts: Vec<FileTexts>,
}

/// Only this many untracked files get their contents read and shown.
const MAX_UNTRACKED_DIFFS: usize = 200;

impl Workdir {
    pub fn load(git: &Git, context: u32, line_limit: usize) -> Result<Workdir> {
        let status = Status::load(git)?;
        let u = format!("-U{context}");
        let mut unstaged = if status.unstaged().next().is_some() {
            let out = git.read(&["diff", "--no-ext-diff", "--no-color", "--no-renames", &u])?;
            parse_unified(&out.text(), line_limit)
        } else {
            Vec::new()
        };
        let staged = if status.has_staged() {
            let mut args = vec!["diff", "--cached", "--no-ext-diff", "--no-color", "--no-renames", &u];
            if status.head_oid.is_none() {
                // There is no HEAD to compare against yet.
                args.push("--root");
            }
            let out = git.read(&args)?;
            parse_unified(&out.text(), line_limit)
        } else {
            Vec::new()
        };

        let mut conflicted = Vec::new();
        for e in status.conflicted() {
            let mut f = FileDiff::new(
                FileStatus::Conflicted,
                Some(e.path().to_string()),
                Some(e.path().to_string()),
            );
            // `git diff` lists conflicted files as "unmerged" with a combined
            // diff. We show them separately, so drop them from `unstaged`.
            unstaged.retain(|u| u.path() != e.path());
            if let Ok(text) = std::fs::read(git.workdir().join(e.path())) {
                if is_binary(&text) {
                    f.binary = true;
                } else {
                    let text = String::from_utf8_lossy(&text);
                    f.hunks = diff_texts("", &text, context);
                    f.old_path = Some(e.path().to_string());
                }
            }
            conflicted.push(f);
        }

        let unstaged_texts = unstaged
            .iter()
            .map(|f| FileTexts {
                old: None,
                new: f
                    .new_path
                    .as_ref()
                    .and_then(|p| read_text(&git.workdir().join(p))),
            })
            .collect();

        let mut untracked = Vec::new();
        let mut untracked_texts = Vec::new();
        for (i, e) in status.untracked().enumerate() {
            let mut f = FileDiff::new(FileStatus::Untracked, None, Some(e.path().to_string()));
            let mut texts = FileTexts::default();
            if i < MAX_UNTRACKED_DIFFS {
                let path = git.workdir().join(e.path());
                match std::fs::metadata(&path) {
                    Ok(m) if m.len() as usize > MAX_BLOB => f.too_large = true,
                    Ok(m) if m.is_file() => match std::fs::read(&path) {
                        Ok(data) if is_binary(&data) => f.binary = true,
                        Ok(data) => {
                            let text: std::sync::Arc<str> = String::from_utf8_lossy(&data).into();
                            f.hunks = diff_texts("", &text, context);
                            if f.line_count() > line_limit {
                                f.too_large = true;
                                f.hunks.clear();
                            }
                            texts.new = Some(text);
                        }
                        Err(_) => {}
                    },
                    _ => {}
                }
            } else {
                f.too_large = true;
            }
            untracked.push(f);
            untracked_texts.push(texts);
        }

        Ok(Workdir {
            status,
            unstaged,
            staged,
            untracked,
            conflicted,
            unstaged_texts,
            untracked_texts,
        })
    }

    pub fn change_count(&self) -> usize {
        let mut paths: Vec<&str> = self.status.entries.iter().map(Entry::path).collect();
        paths.dedup();
        paths.len()
    }
}

fn read_text(path: &std::path::Path) -> Option<std::sync::Arc<str>> {
    let data = std::fs::read(path).ok()?;
    if data.len() > MAX_BLOB || is_binary(&data) {
        return None;
    }
    Some(String::from_utf8_lossy(&data).into())
}

/// Load one file's diff without the size limit, for "show anyway".
pub fn load_full_diff(git: &Git, path: &str, staged: bool, context: u32) -> Result<Option<FileDiff>> {
    let u = format!("-U{context}");
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--no-renames", &u];
    if staged {
        args.push("--cached");
    }
    args.extend(["--", path]);
    let out = git.read_opts(&args, &RunOpts::default())?;
    Ok(parse_unified(&out.text(), usize::MAX).into_iter().next())
}
