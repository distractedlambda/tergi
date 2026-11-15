//! Loading the working directory state: status plus the unstaged, staged
//! and untracked diffs shown in the staging view.

use std::sync::Arc;

use anyhow::Result;

use super::cli::{Git, RunOpts};
use super::commit::{FileTexts, MAX_BLOB, diff_texts, is_binary};
use super::diff::{FileDiff, FileStatus, parse_unified};
use super::status::{Entry, Status};

#[derive(Debug, Clone, Default)]
pub struct Workdir {
    pub status: Status,
    pub unstaged: Vec<Arc<FileDiff>>,
    pub staged: Vec<Arc<FileDiff>>,
    pub untracked: Vec<Arc<FileDiff>>,
    pub conflicted: Vec<Arc<FileDiff>>,
    /// Full text of the working tree version of each unstaged and untracked
    /// file, for syntax highlighting. Indexed like the lists above.
    pub unstaged_texts: Vec<FileTexts>,
    pub untracked_texts: Vec<FileTexts>,
}

/// Only this many untracked files get their contents read and shown.
const MAX_UNTRACKED_DIFFS: usize = 200;

impl Workdir {
    pub async fn load(git: &Git, context: u32, line_limit: usize) -> Result<Workdir> {
        let status = Status::load(git).await?;
        let u = format!("-U{context}");
        let unstaged_diff = if status.unstaged().next().is_some() {
            let out = git
                .read(&["diff", "--no-ext-diff", "--no-color", "--no-renames", &u])
                .await?;
            Some(out.stdout)
        } else {
            None
        };
        let staged_diff = if status.has_staged() {
            let mut args = vec![
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-color",
                "--no-renames",
                &u,
            ];
            if status.head_oid.is_none() {
                // There is no HEAD to compare against yet.
                args.push("--root");
            }
            Some(git.read(&args).await?.stdout)
        } else {
            None
        };

        let mut conflicted_files = Vec::new();
        for e in status.conflicted() {
            let data = tokio::fs::read(git.workdir().join(e.path())).await.ok();
            conflicted_files.push((e.path().to_string(), data));
        }

        let mut untracked_files = Vec::new();
        let mut modes = FileModes::new(git);
        for (i, e) in status.untracked().enumerate() {
            untracked_files.push(if i < MAX_UNTRACKED_DIFFS {
                read_untracked(&mut modes, e.path(), MAX_BLOB).await
            } else {
                let mut f = FileDiff::new(FileStatus::Untracked, None, Some(e.path().to_string()));
                f.too_large = true;
                Untracked { f, data: None }
            });
        }

        // Parsing and diffing can take a while for big changes, so do it
        // off the async threads.
        let mut wd = crate::util::blocking(move || {
            let mut unstaged =
                unstaged_diff.map_or_else(Vec::new, |d| parse_unified(&d, line_limit));
            let staged = staged_diff.map_or_else(Vec::new, |d| parse_unified(&d, line_limit));

            let mut conflicted = Vec::new();
            for (path, data) in conflicted_files {
                let mut f = FileDiff::new(
                    FileStatus::Conflicted,
                    Some(path.clone()),
                    Some(path.clone()),
                );
                // `git diff` lists conflicted files as "unmerged" with a
                // combined diff. We show them separately, so drop them from
                // `unstaged`.
                unstaged.retain(|u| u.path() != path);
                if let Some(text) = data {
                    if is_binary(&text) {
                        f.binary = true;
                    } else {
                        f.hunks = diff_texts(b"", &text, context);
                        f.old_path = Some(path);
                    }
                }
                conflicted.push(f);
            }

            let (untracked, untracked_texts): (Vec<_>, _) = untracked_files
                .into_iter()
                .map(|u| diff_untracked(u, context, line_limit))
                .unzip();

            let arcs = |files: Vec<FileDiff>| files.into_iter().map(Arc::new).collect();
            Workdir {
                status,
                unstaged: arcs(unstaged),
                staged: arcs(staged),
                untracked: arcs(untracked),
                conflicted: arcs(conflicted),
                unstaged_texts: Vec::new(),
                untracked_texts,
            }
        })
        .await;

        for f in &wd.unstaged {
            let new = match &f.new_path {
                Some(p) => read_text(&git.workdir().join(p)).await,
                None => None,
            };
            wd.unstaged_texts.push(FileTexts { old: None, new });
        }
        Ok(wd)
    }

    pub fn change_count(&self) -> usize {
        let mut paths: Vec<&str> = self.status.entries.iter().map(Entry::path).collect();
        paths.dedup();
        paths.len()
    }
}

async fn read_text(path: &std::path::Path) -> Option<Arc<str>> {
    let data = tokio::fs::read(path).await.ok()?;
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
        FileModes {
            git,
            trust_executable: None,
        }
    }

    async fn regular(&mut self, meta: &std::fs::Metadata) -> &'static str {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 == 0 {
            return "100644";
        }
        let trust = match self.trust_executable {
            Some(t) => t,
            None => {
                let t = self
                    .git
                    .read_text(&["config", "--type=bool", "core.fileMode"])
                    .await
                    .map_or(true, |v| v.trim() != "false");
                self.trust_executable = Some(t);
                t
            }
        };
        if trust { "100755" } else { "100644" }
    }
}

/// An untracked file with its mode filled in, and its contents if they
/// are small enough to show.
struct Untracked {
    f: FileDiff,
    data: Option<Vec<u8>>,
}

/// Read an untracked file. Files bigger than `size_limit` bytes are marked
/// `too_large` and not read.
async fn read_untracked(modes: &mut FileModes<'_>, rel: &str, size_limit: usize) -> Untracked {
    let mut f = FileDiff::new(FileStatus::Untracked, None, Some(rel.to_string()));
    let path = modes.git.workdir().join(rel);
    let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
        return Untracked { f, data: None };
    };
    let data = if meta.is_symlink() {
        // Git stores a symlink as a file that holds the path it points at.
        f.new_mode = Some("120000".into());
        tokio::fs::read_link(&path)
            .await
            .ok()
            .map(|t| t.into_os_string().into_encoded_bytes())
    } else if meta.is_file() {
        f.new_mode = Some(modes.regular(&meta).await.into());
        if meta.len() > size_limit as u64 {
            f.too_large = true;
            return Untracked { f, data: None };
        }
        tokio::fs::read(&path).await.ok()
    } else {
        None
    };
    Untracked { f, data }
}

/// The diff for an untracked file, which adds every line, and its text.
/// Files with more than `line_limit` lines are marked `too_large` instead.
fn diff_untracked(u: Untracked, context: u32, line_limit: usize) -> (FileDiff, FileTexts) {
    let Untracked { mut f, data } = u;
    let mut texts = FileTexts::default();
    match data {
        Some(data) if is_binary(&data) => f.binary = true,
        Some(data) => {
            f.hunks = diff_texts(b"", &data, context);
            if f.line_count() > line_limit {
                f.too_large = true;
                f.hunks.clear();
            }
            texts.new = Some(String::from_utf8_lossy(&data).into());
        }
        None => {}
    }
    (f, texts)
}

/// Load an untracked file's diff without the size limits, for "show anyway".
pub async fn load_full_untracked(git: &Git, path: &str, context: u32) -> (FileDiff, FileTexts) {
    let u = read_untracked(&mut FileModes::new(git), path, usize::MAX).await;
    crate::util::blocking(move || diff_untracked(u, context, usize::MAX)).await
}

/// Load the diff of `file` again without the size limit, for "show anyway".
pub async fn load_full_diff(
    git: &Git,
    file: &FileDiff,
    staged: bool,
    context: u32,
) -> Result<Option<FileDiff>> {
    let u = format!("-U{context}");
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--no-renames", &u];
    if staged {
        args.push("--cached");
    }
    let spec = super::ops::pathspec(file.path());
    args.extend(["--", &spec]);
    let out = git.read_opts(&args, &RunOpts::default()).await?;
    let (old_path, new_path) = (file.old_path.clone(), file.new_path.clone());
    // A file that changed type shows up twice under the same path, once
    // deleted and once added.
    Ok(crate::util::blocking(move || {
        parse_unified(&out.stdout, usize::MAX)
            .into_iter()
            .find(|d| d.old_path == old_path && d.new_path == new_path)
    })
    .await)
}
