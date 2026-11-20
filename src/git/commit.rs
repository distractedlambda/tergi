//! Loading a commit's metadata and its diff against its first parent, or
//! against another commit, using gitoxide for the tree diff and imara-diff
//! for the text diff.

use std::sync::Arc;

use anyhow::{Context, Result};
use gix::ObjectId;
use imara_diff::{Algorithm, Diff, InternedInput};

use super::diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub time: i64,
    /// Offset from UTC in seconds.
    pub offset: i32,
}

/// The full text of both sides of a file, kept for syntax highlighting.
#[derive(Debug, Clone, Default)]
pub struct FileTexts {
    pub old: Option<Arc<str>>,
    pub new: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
pub struct CommitDetail {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub message: String,
    pub author: Person,
    pub committer: Person,
    pub files: Vec<Arc<FileDiff>>,
    pub texts: Vec<FileTexts>,
    /// The commit that `files` start from, when it isn't the first parent.
    /// This is set when the user compares two commits.
    pub base: Option<Base>,
}

#[derive(Debug, Clone)]
pub struct Base {
    pub id: ObjectId,
    pub message: String,
}

/// Blobs bigger than this aren't diffed unless the user asks.
pub const MAX_BLOB: usize = 4 * 1024 * 1024;

pub fn load(
    repo: &gix::Repository,
    id: ObjectId,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    load_against(repo, id, None, context, line_limit)
}

/// Load `to` with its diff against `from` instead of its parent. The two
/// commits needn't be related.
pub fn compare(
    repo: &gix::Repository,
    from: ObjectId,
    to: ObjectId,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    load_against(repo, to, Some(from), context, line_limit)
}

/// Load `id` with its diff against `base`, or against its first parent if
/// there is no base.
pub fn load_against(
    repo: &gix::Repository,
    id: ObjectId,
    base: Option<ObjectId>,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    let commit = repo.find_commit(id).context("commit not found")?;
    let (message, author, committer, parents) = {
        let c = commit.decode().context("bad commit")?;
        let person = |s: gix::actor::SignatureRef<'_>| {
            let t = s.time().unwrap_or_default();
            Person {
                name: String::from_utf8_lossy(s.name).trim().to_string(),
                email: String::from_utf8_lossy(s.email).trim().to_string(),
                time: t.seconds,
                offset: t.offset,
            }
        };
        (
            String::from_utf8_lossy(c.message).into_owned(),
            person(c.author().context("bad author")?),
            person(c.committer().context("bad committer")?),
            c.parents().collect::<Vec<_>>(),
        )
    };
    let new_tree = commit.tree().context("commit has no tree")?;
    let (old_tree, base) = match base {
        Some(b) => {
            let c = repo.find_commit(b).context("commit not found")?;
            let message = String::from_utf8_lossy(c.decode().context("bad commit")?.message);
            let message = message.into_owned();
            let base = Base { id: b, message };
            (c.tree().context("commit has no tree")?, Some(base))
        }
        None => match parents.first() {
            Some(p) => (
                repo.find_commit(*p)
                    .context("parent not found")?
                    .tree()
                    .context("parent has no tree")?,
                None,
            ),
            None => (repo.empty_tree(), None),
        },
    };
    let (mut files, mut texts) = diff_trees(repo, &old_tree, &new_tree, context, line_limit)?;
    // Only a stash's own diff shows its untracked files.
    if base.is_none()
        && let Some(untracked) = stash_untracked(repo, &parents)?
    {
        let tree = untracked.tree().context("stash has no tree")?;
        let (more, more_texts) = diff_trees(repo, &repo.empty_tree(), &tree, context, line_limit)?;
        for mut f in more {
            f.status = FileStatus::Untracked;
            files.push(f);
        }
        texts.extend(more_texts);
        (files, texts) = sort_by_path(files, texts);
    }
    Ok(CommitDetail {
        id,
        parents,
        message,
        author,
        committer,
        files: files.into_iter().map(Arc::new).collect(),
        texts,
        base,
    })
}

pub fn diff_trees(
    repo: &gix::Repository,
    old_tree: &gix::Tree<'_>,
    new_tree: &gix::Tree<'_>,
    context: u32,
    line_limit: usize,
) -> Result<(Vec<FileDiff>, Vec<FileTexts>)> {
    use gix::object::tree::diff::ChangeDetached as C;
    let options = gix::diff::Options::default().with_rewrites(rewrites(repo)?);
    let changes = repo
        .diff_tree_to_tree(Some(old_tree), Some(new_tree), options)
        .context("tree diff failed")?;
    let mut files = Vec::new();
    let mut texts = Vec::new();
    for change in changes {
        let (status, old_path, new_path, old, new) = match change {
            C::Addition {
                entry_mode,
                id,
                location,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (
                    FileStatus::Added,
                    None,
                    Some(location),
                    None,
                    Some((id, entry_mode)),
                )
            }
            C::Deletion {
                entry_mode,
                id,
                location,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (
                    FileStatus::Deleted,
                    Some(location),
                    None,
                    Some((id, entry_mode)),
                    None,
                )
            }
            C::Modification {
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
                location,
            } => {
                if entry_mode.is_tree() && previous_entry_mode.is_tree() {
                    continue;
                }
                // Git treats a file that only gains or loses its executable
                // bit as modified, but gix gives the two modes different
                // kinds.
                let same_type = (previous_entry_mode.is_blob() && entry_mode.is_blob())
                    || previous_entry_mode.kind() == entry_mode.kind();
                let status = if same_type {
                    FileStatus::Modified
                } else {
                    FileStatus::TypeChange
                };
                (
                    status,
                    Some(location.clone()),
                    Some(location),
                    Some((previous_id, previous_entry_mode)),
                    Some((id, entry_mode)),
                )
            }
            C::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                entry_mode,
                id,
                copy,
                location,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (
                    if copy {
                        FileStatus::Copied
                    } else {
                        FileStatus::Renamed
                    },
                    Some(source_location),
                    Some(location),
                    Some((source_id, source_entry_mode)),
                    Some((id, entry_mode)),
                )
            }
        };
        let mut f = FileDiff::new(status, old_path, new_path);
        f.old_id = old.map(|o| o.0);
        f.new_id = new.map(|n| n.0);
        f.old_mode = old.map(|o| o.1.kind().as_octal_str().to_string());
        f.new_mode = new.map(|n| n.1.kind().as_octal_str().to_string());

        let old_text = old
            .map(|(id, mode)| blob_text(repo, id, mode))
            .transpose()?;
        let new_text = new
            .map(|(id, mode)| blob_text(repo, id, mode))
            .transpose()?;
        let binary = old_text.as_ref().is_some_and(|t| t.is_none())
            || new_text.as_ref().is_some_and(|t| t.is_none());
        let old_text = old_text.flatten();
        let new_text = new_text.flatten();
        if binary {
            f.binary = true;
            texts.push(FileTexts::default());
        } else if old_text.as_ref().map_or(0, |t| t.len()) > MAX_BLOB
            || new_text.as_ref().map_or(0, |t| t.len()) > MAX_BLOB
        {
            f.too_large = true;
            texts.push(FileTexts::default());
        } else {
            f.hunks = diff_texts(
                old_text.as_deref().unwrap_or("").as_bytes(),
                new_text.as_deref().unwrap_or("").as_bytes(),
                context,
            );
            if f.line_count() > line_limit {
                f.too_large = true;
                f.hunks.clear();
            }
            texts.push(FileTexts {
                old: old_text,
                new: new_text,
            });
        }
        files.push(f);
    }
    Ok(sort_by_path(files, texts))
}

/// How to find renames and copies, from `diff.renames` and
/// `diff.renameLimit`, the way gix reads them. Unlike gix, git also pairs
/// up empty files, so we do too.
fn rewrites(repo: &gix::Repository) -> Result<Option<gix::diff::Rewrites>> {
    let config = repo.config_snapshot();
    let (rewrites, configured) =
        gix::diff::new_rewrites(config.plumbing(), true).context("bad rename config")?;
    let rewrites = match rewrites {
        Some(r) => r,
        None if configured => return Ok(None),
        None => gix::diff::Rewrites::default(),
    };
    Ok(Some(gix::diff::Rewrites {
        track_empty: true,
        ..rewrites
    }))
}

/// Sort the files by path, keeping `texts` in the same order as `files`.
fn sort_by_path(files: Vec<FileDiff>, texts: Vec<FileTexts>) -> (Vec<FileDiff>, Vec<FileTexts>) {
    let mut paired: Vec<(FileDiff, FileTexts)> = files.into_iter().zip(texts).collect();
    paired.sort_by(|a, b| a.0.path().cmp(b.0.path()));
    paired.into_iter().unzip()
}

/// If these are the parents of a stash made with `--include-untracked`,
/// the commit that holds its untracked files.
///
/// Such a stash has three parents: the commit it was made on, a commit
/// with the staged changes on top of that, and a root commit with the
/// untracked files. Git always gives the last one a message that starts
/// with "untracked files on ". We check all of that, so an ordinary merge
/// with three parents isn't mistaken for a stash.
fn stash_untracked<'r>(
    repo: &'r gix::Repository,
    parents: &[ObjectId],
) -> Result<Option<gix::Commit<'r>>> {
    let [base, index, untracked] = parents else {
        return Ok(None);
    };
    let index = repo.find_commit(*index).context("parent not found")?;
    if index.parent_ids().map(|p| p.detach()).ne([*base]) {
        return Ok(None);
    }
    let untracked = repo.find_commit(*untracked).context("parent not found")?;
    let c = untracked.decode().context("bad commit")?;
    if c.parents().next().is_some() || !c.message.starts_with(b"untracked files on ") {
        return Ok(None);
    }
    drop(c);
    Ok(Some(untracked))
}

/// The text of a blob, or `None` if it looks binary. Submodule entries are
/// shown the way git shows them.
fn blob_text(
    repo: &gix::Repository,
    id: ObjectId,
    mode: gix::object::tree::EntryMode,
) -> Result<Option<Arc<str>>> {
    if mode.is_commit() {
        return Ok(Some(format!("Subproject commit {id}\n").into()));
    }
    let blob = repo.find_blob(id).context("blob not found")?;
    if is_binary(&blob.data) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&blob.data).into()))
}

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Compute unified-diff hunks between two texts. They are bytes because an
/// untracked file needn't be UTF-8, and staging its lines needs the bytes.
pub fn diff_texts(old: &[u8], new: &[u8], context: u32) -> Vec<Hunk> {
    let old_lines: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
    let new_lines: Vec<&[u8]> = new.split_inclusive(|&b| b == b'\n').collect();
    let mut input: InternedInput<&[u8]> = InternedInput::default();
    input.update_before(old_lines.iter().copied());
    input.update_after(new_lines.iter().copied());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let changes: Vec<imara_diff::Hunk> = diff.hunks().collect();
    let ctx = context as usize;
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Group changes whose gaps are small enough to share context.
        let mut j = i;
        while j + 1 < changes.len()
            && (changes[j + 1].before.start - changes[j].before.end) as usize <= 2 * ctx
        {
            j += 1;
        }
        let first = &changes[i];
        let last = &changes[j];
        let old_begin = (first.before.start as usize).saturating_sub(ctx);
        let new_begin = (first.after.start as usize).saturating_sub(ctx);
        let old_end = (last.before.end as usize + ctx).min(old_lines.len());

        let mut lines = Vec::new();
        let mut o = old_begin;
        let mut n = new_begin;
        let push = |lines: &mut Vec<DiffLine>,
                    kind,
                    text: &[u8],
                    o: Option<usize>,
                    n: Option<usize>,
                    last: bool| {
            let mut line = DiffLine::new(
                kind,
                text.strip_suffix(b"\n").unwrap_or(text),
                o.map(|x| x as u32 + 1),
                n.map(|x| x as u32 + 1),
            );
            line.no_eol = last && !text.ends_with(b"\n");
            lines.push(line);
        };
        for c in &changes[i..=j] {
            while o < c.before.start as usize {
                push(
                    &mut lines,
                    LineKind::Context,
                    old_lines[o],
                    Some(o),
                    Some(n),
                    o + 1 == old_lines.len(),
                );
                o += 1;
                n += 1;
            }
            for k in c.before.clone() {
                let k = k as usize;
                push(
                    &mut lines,
                    LineKind::Del,
                    old_lines[k],
                    Some(k),
                    None,
                    k + 1 == old_lines.len(),
                );
            }
            for k in c.after.clone() {
                let k = k as usize;
                push(
                    &mut lines,
                    LineKind::Add,
                    new_lines[k],
                    None,
                    Some(k),
                    k + 1 == new_lines.len(),
                );
            }
            o = c.before.end as usize;
            n = c.after.end as usize;
        }
        while o < old_end {
            push(
                &mut lines,
                LineKind::Context,
                old_lines[o],
                Some(o),
                Some(n),
                o + 1 == old_lines.len(),
            );
            o += 1;
            n += 1;
        }
        let old_len = (o - old_begin) as u32;
        let new_len = (n - new_begin) as u32;
        hunks.push(Hunk {
            old_start: if old_len == 0 {
                old_begin as u32
            } else {
                old_begin as u32 + 1
            },
            old_len,
            new_start: if new_len == 0 {
                new_begin as u32
            } else {
                new_begin as u32 + 1
            },
            new_len,
            section: String::new(),
            lines,
        });
        i = j + 1;
    }
    hunks
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::git::diff::parse_unified;

    fn render(hunks: &[Hunk]) -> String {
        let mut s = String::new();
        for h in hunks {
            s.push_str(&h.header());
            s.push('\n');
            for l in &h.lines {
                s.push(match l.kind {
                    LineKind::Add => '+',
                    LineKind::Del => '-',
                    LineKind::Context => ' ',
                });
                s.push_str(&l.text);
                s.push('\n');
                if l.no_eol {
                    s.push_str("\\ No newline at end of file\n");
                }
            }
        }
        s
    }

    #[test]
    fn matches_git_style() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\nthree\n4\n5\n6\n7\n8\n9\n10\neleven";
        insta::assert_snapshot!(render(&diff_texts(old.as_bytes(), new.as_bytes(), 3)), @r"
        @@ -1,6 +1,6 @@
         1
         2
        -3
        +three
         4
         5
         6
        @@ -8,3 +8,4 @@
         8
         9
         10
        +eleven
        \ No newline at end of file
        ");
    }

    /// Some lines look like diff syntax, to check that the parser goes by
    /// the hunk headers and not by what lines look like.
    fn text() -> impl Strategy<Value = String> {
        let line = prop::sample::select(vec![
            "a",
            "b",
            "c",
            "",
            " ",
            "x\r",
            "+a",
            "-a",
            "\\ b",
            "@@ -1 +1 @@",
            "diff --git a/f b/f",
            "--- a/f",
        ]);
        (prop::collection::vec(line, 0..30), any::<bool>()).prop_map(|(lines, eol)| {
            let mut s = lines.join("\n");
            if eol && !lines.is_empty() {
                s.push('\n');
            }
            s
        })
    }

    /// Apply hunks to the old text.
    fn apply(old: &str, hunks: &[Hunk]) -> String {
        let old: Vec<&str> = old.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut pos = 0;
        for h in hunks {
            let first = if h.old_len == 0 {
                h.old_start
            } else {
                h.old_start - 1
            } as usize;
            assert!(first >= pos, "hunks overlap or are out of order");
            out.extend(old[pos..first].iter().copied());
            pos = first;
            for l in &h.lines {
                if l.kind != LineKind::Add {
                    assert_eq!(old[pos].strip_suffix('\n').unwrap_or(old[pos]), l.text);
                    pos += 1;
                }
                if l.kind != LineKind::Del {
                    out.push_str(&l.text);
                    if !l.no_eol {
                        out.push('\n');
                    }
                }
            }
        }
        out.extend(old[pos..].iter().copied());
        out
    }

    proptest! {
        #[test]
        fn hunks_turn_old_into_new(old in text(), new in text(), context in 0u32..5) {
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            prop_assert_eq!(hunks.is_empty(), old == new);
            prop_assert_eq!(apply(&old, &hunks), new);

            let ctx = context as usize;
            for h in &hunks {
                let count = |k: LineKind| h.lines.iter().filter(|l| l.kind == k).count() as u32;
                prop_assert_eq!(h.old_len, count(LineKind::Context) + count(LineKind::Del));
                prop_assert_eq!(h.new_len, count(LineKind::Context) + count(LineKind::Add));
                // Line numbers count up from the header, on each side.
                let (mut o, mut n) = (h.old_start.max(1), h.new_start.max(1));
                for l in &h.lines {
                    let want_old = (l.kind != LineKind::Add).then_some(o);
                    let want_new = (l.kind != LineKind::Del).then_some(n);
                    prop_assert_eq!((l.old_no, l.new_no), (want_old, want_new));
                    o += u32::from(want_old.is_some());
                    n += u32::from(want_new.is_some());
                }
                // Runs of context are at most `context` long at the ends of
                // a hunk, and at most twice that between changes.
                let runs: Vec<usize> = h
                    .lines
                    .split(|l| l.kind != LineKind::Context)
                    .map(<[DiffLine]>::len)
                    .collect();
                prop_assert!(runs[0] <= ctx && runs[runs.len() - 1] <= ctx, "{:?}", runs);
                prop_assert!(runs.iter().all(|&r| r <= 2 * ctx), "{:?}", runs);
                prop_assert!(h.lines.iter().any(|l| l.kind != LineKind::Context));
            }
            // Hunks that would touch are merged into one.
            let first = |h: &Hunk| if h.old_len == 0 { h.old_start } else { h.old_start - 1 };
            for w in hunks.windows(2) {
                prop_assert!(first(&w[1]) > first(&w[0]) + w[0].old_len, "hunks touch");
            }

            // Printed as a unified diff, the parser reads the same hunks back.
            let text = format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n{}", render(&hunks));
            let parsed = parse_unified(&text, usize::MAX);
            if hunks.is_empty() {
                prop_assert!(parsed[0].hunks.is_empty());
            } else {
                prop_assert_eq!(&parsed[0].hunks, &hunks);
            }
        }
    }

    #[test]
    fn new_and_empty() {
        insta::assert_snapshot!(render(&diff_texts(b"", b"a\nb\n", 3)), @r"
        @@ -0,0 +1,2 @@
        +a
        +b
        ");
        insta::assert_snapshot!(render(&diff_texts(b"a\n", b"", 3)), @r"
        @@ -1 +0,0 @@
        -a
        ");
    }
}
