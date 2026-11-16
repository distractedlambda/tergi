//! Loading the working directory state: status plus the unstaged, staged
//! and untracked diffs shown in the staging view.

use std::sync::Arc;

use anyhow::Result;
use gix::ObjectId;

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
    /// Full text of both sides of each unstaged, staged and untracked file,
    /// for syntax highlighting. Indexed like the lists above.
    pub unstaged_texts: Vec<FileTexts>,
    pub staged_texts: Vec<FileTexts>,
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
                .read(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-color",
                    "--no-renames",
                    "--full-index",
                    &u,
                ])
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
                "--full-index",
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
                staged_texts: Vec::new(),
                untracked_texts,
            }
        })
        .await;

        // The old side of a staged file is in HEAD, and its new side is in
        // the index. The index also has the old side of an unstaged file.
        // `--full-index` gives us their ids, so we never have to name a
        // blob by its path.
        let blob = |f: &FileDiff, id: Option<ObjectId>| id.filter(|id| !f.binary && !id.is_null());
        let mut ids = Vec::new();
        for f in &wd.staged {
            ids.push(blob(f, f.old_id));
            ids.push(blob(f, f.new_id));
        }
        for f in &wd.unstaged {
            ids.push(blob(f, f.old_id));
        }
        let mut blobs = read_blobs(git, &ids).await.into_iter();
        for _ in &wd.staged {
            let old = blobs.next().flatten();
            let new = blobs.next().flatten();
            wd.staged_texts.push(FileTexts { old, new });
        }
        for f in &wd.unstaged {
            let old = blobs.next().flatten();
            let new = match &f.new_path {
                Some(p) if !f.binary => read_text(&git.workdir().join(p)).await,
                _ => None,
            };
            wd.unstaged_texts.push(FileTexts { old, new });
        }
        Ok(wd)
    }

    pub fn change_count(&self) -> usize {
        let mut paths: Vec<&str> = self.status.entries.iter().map(Entry::path).collect();
        paths.dedup();
        paths.len()
    }
}

/// The text of a file in the working tree, as git would store it. For a
/// symlink, that is the path it points to, not the file there.
async fn read_text(path: &std::path::Path) -> Option<Arc<str>> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    let data = if meta.is_symlink() {
        tokio::fs::read_link(path)
            .await
            .ok()?
            .into_os_string()
            .into_encoded_bytes()
    } else {
        tokio::fs::read(path).await.ok()?
    };
    if data.len() > MAX_BLOB || is_binary(&data) {
        return None;
    }
    Some(String::from_utf8_lossy(&data).into())
}

/// Read the text of each blob. Ids that aren't blobs, and blobs that are
/// binary or over `MAX_BLOB`, give `None`. If git fails, every id does.
async fn read_blobs(git: &Git, ids: &[Option<ObjectId>]) -> Vec<Option<Arc<str>>> {
    let mut texts = vec![None; ids.len()];
    let asked: Vec<(usize, String)> = ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| Some((i, id.as_ref()?.to_string())))
        .collect();
    if asked.is_empty() {
        return texts;
    }
    let lines = |items: &mut dyn Iterator<Item = &str>| {
        let mut input = Vec::new();
        for item in items {
            input.extend_from_slice(item.as_bytes());
            input.push(b'\n');
        }
        RunOpts {
            stdin: Some(input),
            ..Default::default()
        }
    };

    // First find out what each id is and how big it is, so we don't read
    // large blobs only to throw them away.
    let Ok(out) = git
        .read_opts(
            &[
                "cat-file",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
            ],
            &lines(&mut asked.iter().map(|(_, id)| id.as_str())),
        )
        .await
    else {
        return texts;
    };
    let info = String::from_utf8_lossy(&out.stdout);
    let mut wanted: Vec<(usize, String)> = Vec::new();
    for ((i, _), line) in asked.iter().zip(info.lines()) {
        // An id that isn't there gives "<id> missing" instead.
        let fields: Vec<&str> = line.split(' ').collect();
        if let [id, "blob", size] = fields[..]
            && size.parse().is_ok_and(|n: usize| n <= MAX_BLOB)
        {
            wanted.push((*i, id.to_string()));
        }
    }
    if wanted.is_empty() {
        return texts;
    }

    let Ok(out) = git
        .read_opts(
            &["cat-file", "--batch"],
            &lines(&mut wanted.iter().map(|(_, id)| id.as_str())),
        )
        .await
    else {
        return texts;
    };
    // Each blob comes as "<id> blob <size>\n", its contents and a newline.
    let mut rest = out.stdout.as_slice();
    for (i, id) in wanted {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&rest[..end]);
        let mut fields = header.split(' ');
        let size = match (fields.next(), fields.next(), fields.next()) {
            (Some(got), Some("blob"), Some(size)) if got == id => size.parse::<usize>().ok(),
            _ => None,
        };
        let Some(data) = size.and_then(|n| rest.get(end + 1..end + 1 + n)) else {
            break;
        };
        rest = rest.get(end + 2 + data.len()..).unwrap_or_default();
        if !is_binary(data) {
            texts[i] = Some(String::from_utf8_lossy(data).into());
        }
    }
    texts
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
