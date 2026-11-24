//! Loading the working directory state: status plus the unstaged, staged
//! and untracked diffs shown in the staging view.

use std::ffi::OsString;
use std::sync::Arc;

use anyhow::Result;
use bstr::{BStr, ByteSlice as _};

use super::cli::{Git, RunOpts};
use super::commit::{BINARY_TEST_LEN, FileTexts, diff_texts, is_binary};
use super::diff::{FileDiff, FileStatus, parse_unified};
use super::status::{Entry, Status};
use crate::git::ObjectId;
use crate::util::fs_path;

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

/// How every `git diff` we parse starts. The user's config can change what
/// git prints. `diff.mnemonicPrefix` and `diff.noprefix` change the prefixes
/// on the paths, which `parse_unified` strips, so we name the usual ones.
const DIFF: &[&str] = &[
    "diff",
    "--no-ext-diff",
    "--no-color",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// Only this many untracked files get their contents read and shown.
const MAX_UNTRACKED_DIFFS: usize = 200;

impl Workdir {
    pub async fn load(git: &Git, context: u32, line_limit: usize) -> Result<Workdir> {
        let status = Status::load(git).await?;
        let u = format!("-U{context}");
        let unstaged_diff = if status.unstaged().next().is_some() {
            let mut args = DIFF.to_vec();
            args.extend(["--full-index", &u]);
            let out = git.read(&args).await?;
            Some(out.stdout)
        } else {
            None
        };
        let staged_diff = if status.has_staged() {
            let mut args = DIFF.to_vec();
            args.extend(["--cached", "--full-index", &u]);
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
            let data = read_worktree(&git.workdir().join(fs_path(e.path()))).await;
            conflicted_files.push((e.path().to_owned(), data));
        }

        let mut untracked_files = Vec::new();
        let mut modes = FileModes::new(git);
        for (i, e) in status.untracked().enumerate() {
            untracked_files.push(if i < MAX_UNTRACKED_DIFFS {
                read_untracked(&mut modes, e.path()).await
            } else {
                let mut f = FileDiff::new(FileStatus::Untracked, None, Some(e.path().to_owned()));
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
                unstaged.retain(|u| u.path() != path.as_bstr());
                match data {
                    Some(Contents::Binary) => f.binary = true,
                    Some(Contents::Text(text)) => {
                        f.hunks = diff_texts(b"", &text, context);
                        f.old_path = Some(path);
                    }
                    None => {}
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
                Some(p) if !f.binary => read_text(&git.workdir().join(fs_path(p))).await,
                _ => None,
            };
            wd.unstaged_texts.push(FileTexts { old, new });
        }
        Ok(wd)
    }

    pub fn change_count(&self) -> usize {
        let mut paths: Vec<&BStr> = self.status.entries.iter().map(Entry::path).collect();
        paths.dedup();
        paths.len()
    }
}

/// What a file in the working tree holds, as far as we read it.
enum Contents {
    Text(Vec<u8>),
    /// A binary file, of which we only read the start.
    Binary,
}

/// Read a file in the working tree the way git would store it. For a
/// symlink, that is the path it points to, not the file there. Anything
/// that is neither, such as a directory or a FIFO, and a file we can't
/// read, give `None`. Opening a FIFO would wait for a writer.
///
/// A binary file can be huge, so we only read as much of it as
/// `is_binary` looks at. Text files are read whatever their size.
async fn read_worktree(path: &std::path::Path) -> Option<Contents> {
    use tokio::io::AsyncReadExt as _;
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if meta.is_symlink() {
        let target = tokio::fs::read_link(path).await.ok()?;
        return Some(Contents::Text(target.into_os_string().into_encoded_bytes()));
    }
    if !meta.is_file() {
        return None;
    }
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut data = Vec::new();
    (&mut file)
        .take(BINARY_TEST_LEN as u64)
        .read_to_end(&mut data)
        .await
        .ok()?;
    if is_binary(&data) {
        return Some(Contents::Binary);
    }
    file.read_to_end(&mut data).await.ok()?;
    Some(Contents::Text(data))
}

/// The text of a file in the working tree, unless it's binary.
async fn read_text(path: &std::path::Path) -> Option<Arc<str>> {
    match read_worktree(path).await? {
        Contents::Text(data) => Some(String::from_utf8_lossy(&data).into()),
        Contents::Binary => None,
    }
}

/// Read the text of each blob. Ids that aren't there or aren't blobs, and
/// blobs that are binary, give `None`. If git fails, every id does.
///
/// The ids come from the diff's `index` lines. The caller leaves out the
/// files that git's diff calls binary, so we don't read those. The id of
/// a submodule names a commit in the submodule, which usually isn't
/// here.
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
    let mut input = Vec::new();
    for (_, id) in &asked {
        input.extend_from_slice(id.as_bytes());
        input.push(b'\n');
    }
    let opts = RunOpts {
        stdin: Some(input),
        ..Default::default()
    };
    let Ok(out) = git.read_opts(&["cat-file", "--batch"], &opts).await else {
        return texts;
    };
    // Each object comes as "<id> <type> <size>\n", its contents and a
    // newline. An id that isn't there gives only "<id> missing\n".
    let mut rest = out.stdout.as_slice();
    for (i, id) in asked {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&rest[..end]);
        rest = &rest[end + 1..];
        let fields: Vec<&str> = header.split(' ').collect();
        let [got, kind, size] = fields[..] else {
            continue;
        };
        let Some(data) = size.parse::<usize>().ok().and_then(|n| rest.get(..n)) else {
            break;
        };
        rest = rest.get(data.len() + 1..).unwrap_or_default();
        if got != id {
            break;
        }
        if kind == "blob" && !is_binary(data) {
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

/// An untracked file with its mode filled in, and its contents if we
/// could read them.
struct Untracked {
    f: FileDiff,
    data: Option<Contents>,
}

async fn read_untracked(modes: &mut FileModes<'_>, rel: &BStr) -> Untracked {
    let mut f = FileDiff::new(FileStatus::Untracked, None, Some(rel.to_owned()));
    let path = modes.git.workdir().join(fs_path(rel));
    let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
        return Untracked { f, data: None };
    };
    if meta.is_symlink() {
        // Git stores a symlink as a file that holds the path it points at.
        f.new_mode = Some("120000".into());
    } else if meta.is_file() {
        f.new_mode = Some(modes.regular(&meta).await.into());
    }
    let data = read_worktree(&path).await;
    Untracked { f, data }
}

/// The diff for an untracked file, which adds every line, and its text.
/// Files with more than `line_limit` lines are marked `too_large` instead.
fn diff_untracked(u: Untracked, context: u32, line_limit: usize) -> (FileDiff, FileTexts) {
    let Untracked { mut f, data } = u;
    let mut texts = FileTexts::default();
    match data {
        Some(Contents::Binary) => f.binary = true,
        Some(Contents::Text(data)) => {
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

/// Load an untracked file's diff without the line limit, for "show
/// anyway".
pub async fn load_full_untracked(git: &Git, path: &BStr, context: u32) -> (FileDiff, FileTexts) {
    let u = read_untracked(&mut FileModes::new(git), path).await;
    crate::util::blocking(move || diff_untracked(u, context, usize::MAX)).await
}

/// Load the diff of `file` again without the line limit, for "show anyway".
pub async fn load_full_diff(
    git: &Git,
    file: &FileDiff,
    staged: bool,
    context: u32,
) -> Result<Option<FileDiff>> {
    let u = format!("-U{context}");
    let mut args: Vec<OsString> = DIFF
        .iter()
        .chain([&u.as_str()])
        .map(OsString::from)
        .collect();
    if staged {
        args.push("--cached".into());
    }
    args.extend(["--".into(), super::ops::pathspec(file.path())]);
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
