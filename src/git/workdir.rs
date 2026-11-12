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
        let mut modes = FileModes::new(git);
        for (i, e) in status.untracked().enumerate() {
            let (f, texts) = if i < MAX_UNTRACKED_DIFFS {
                untracked_file(&mut modes, e.path(), context, line_limit, MAX_BLOB)
            } else {
                let mut f = FileDiff::new(FileStatus::Untracked, None, Some(e.path().to_string()));
                f.too_large = true;
                (f, FileTexts::default())
            };
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

/// Works out the mode git would record for a new file.
struct FileModes<'a> {
    git: &'a Git,
    /// Whether git looks at the executable bit, from `core.fileMode`. We
    /// only ask once we find an executable file.
    trust_executable: Option<bool>,
}

impl<'a> FileModes<'a> {
    fn new(git: &'a Git) -> Self {
        FileModes { git, trust_executable: None }
    }

    fn regular(&mut self, meta: &std::fs::Metadata) -> &'static str {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 == 0 {
            return "100644";
        }
        let git = self.git;
        let trust = *self.trust_executable.get_or_insert_with(|| {
            git.read_text(&["config", "--type=bool", "core.fileMode"])
                .map_or(true, |v| v.trim() != "false")
        });
        if trust { "100755" } else { "100644" }
    }
}

/// The diff for an untracked file, which adds every line, and its text.
/// Files bigger than `size_limit` bytes or with more than `line_limit`
/// lines are marked `too_large` instead.
fn untracked_file(
    modes: &mut FileModes<'_>,
    rel: &str,
    context: u32,
    line_limit: usize,
    size_limit: usize,
) -> (FileDiff, FileTexts) {
    let mut f = FileDiff::new(FileStatus::Untracked, None, Some(rel.to_string()));
    let mut texts = FileTexts::default();
    let path = modes.git.workdir().join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return (f, texts);
    };
    let data = if meta.is_symlink() {
        // Git stores a symlink as a file that holds the path it points at.
        f.new_mode = Some("120000".into());
        std::fs::read_link(&path).ok().map(|t| t.into_os_string().into_encoded_bytes())
    } else if meta.is_file() {
        f.new_mode = Some(modes.regular(&meta).into());
        if meta.len() > size_limit as u64 {
            f.too_large = true;
            return (f, texts);
        }
        std::fs::read(&path).ok()
    } else {
        None
    };
    match data {
        Some(data) if is_binary(&data) => f.binary = true,
        Some(data) => {
            let text: std::sync::Arc<str> = String::from_utf8_lossy(&data).into();
            f.hunks = diff_texts("", &text, context);
            if f.line_count() > line_limit {
                f.too_large = true;
                f.hunks.clear();
            }
            texts.new = Some(text);
        }
        None => {}
    }
    (f, texts)
}

/// Load an untracked file's diff without the size limits, for "show anyway".
pub fn load_full_untracked(git: &Git, path: &str, context: u32) -> (FileDiff, FileTexts) {
    untracked_file(&mut FileModes::new(git), path, context, usize::MAX, usize::MAX)
}

/// Load the diff of `file` again without the size limit, for "show anyway".
pub fn load_full_diff(git: &Git, file: &FileDiff, staged: bool, context: u32) -> Result<Option<FileDiff>> {
    let u = format!("-U{context}");
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--no-renames", &u];
    if staged {
        args.push("--cached");
    }
    args.extend(["--", path]);
    let out = git.read_opts(&args, &RunOpts::default())?;
    Ok(parse_unified(&out.text(), usize::MAX).into_iter().next())
}
