//! Loading a commit's metadata and its diff against its first parent, or
//! against another commit. Git reads the commits, lists the changed files
//! and hands over the blobs. imara-diff diffs the texts.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bstr::{BString, ByteSlice as _};
use imara_diff::{Algorithm, Diff, InternedInput};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

use super::ObjectId;
use super::cli::{Git, Process, RunOpts};
use super::diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
use super::log::parse_raw_date;

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

pub async fn load(
    git: &Git,
    id: ObjectId,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    load_against(git, id, None, context, line_limit).await
}

/// Load `to` with its diff against `from` instead of its parent. The two
/// commits needn't be related.
pub async fn compare(
    git: &Git,
    from: ObjectId,
    to: ObjectId,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    load_against(git, to, Some(from), context, line_limit).await
}

/// Load `id` with its diff against `base`, or against its first parent if
/// there is no base.
pub async fn load_against(
    git: &Git,
    id: ObjectId,
    base: Option<ObjectId>,
    context: u32,
    line_limit: usize,
) -> Result<CommitDetail> {
    // A request that names only the commit gives its parents as git sees
    // them, which needn't be what the commit object says, such as in a
    // shallow clone. A request that also names a commit to diff against
    // gives that one as the parent instead. So a comparison asks about
    // `id` on its own too, even though git diffs it against its parent
    // for nothing.
    let mut requests = vec![id.to_string()];
    if let Some(b) = base {
        requests.push(format!("{id} {b}"));
        // Diffing the base against itself costs nothing, and gives us
        // its message.
        requests.push(format!("{b} {b}"));
    }
    let mut answers = diff_tree(git, &requests).await?.into_iter();
    let mut next = || answers.next().flatten().context("commit not found");
    let mut commit = next()?;
    let base = match base {
        Some(b) => {
            commit.changes = next()?.changes;
            Some(Base {
                id: b,
                message: next()?.message,
            })
        }
        None => None,
    };
    // Git doesn't diff a merge on its own, so we ask again with the first
    // parent.
    if let (None, [first, rest @ ..]) = (&base, &commit.parents[..])
        && !rest.is_empty()
    {
        let mut requests = vec![format!("{id} {first}")];
        if let [index, untracked] = rest {
            requests.extend([index.to_string(), untracked.to_string()]);
        }
        let mut answers = diff_tree(git, &requests).await?.into_iter();
        let mut next = || answers.next().flatten().context("commit not found");
        let mut changes = next()?.changes;
        if rest.len() == 2 {
            let (index, untracked) = (next()?, next()?);
            if is_stash(*first, &index, &untracked) {
                // A stash keeps its untracked files in a root commit,
                // which git diffs against the empty tree.
                for mut c in untracked.changes {
                    c.status = FileStatus::Untracked;
                    changes.push(c);
                }
            }
        }
        commit.changes = changes;
    }
    let changes = std::mem::take(&mut commit.changes);
    let blobs = read_blobs(git, &changes).await?;
    // Diffing can take a while for big commits, so do it off the async
    // threads.
    let (files, texts) =
        crate::util::blocking(move || files_of(changes, &blobs, context, line_limit)).await;
    Ok(CommitDetail {
        id,
        parents: commit.parents,
        message: commit.message,
        author: commit.author,
        committer: commit.committer,
        files: files.into_iter().map(Arc::new).collect(),
        texts,
        base,
    })
}

/// A commit and its changed files, as `diff_tree` gets them from git.
struct Answer {
    parents: Vec<ObjectId>,
    author: Person,
    committer: Person,
    message: String,
    changes: Vec<Change>,
}

/// What `diff_tree` asks git to print about each commit, as fields that
/// each end with a NUL. The id comes first, because `read_answer` needs
/// the first byte of an answer to be hex.
const FORMAT: &str = "--format=%H%x00%P%x00%aN%x00%aE%x00%ad%x00%cN%x00%cE%x00%cd%x00%B%x00";
const FIELDS: usize = 9;

/// The line `diff_tree` sends after each request. Git prints lines that
/// don't start with an object id back as they are, so this shows where
/// an answer ends. An id is hex, so it can't start with `t`.
const END: &[u8] = b"tergi\n";

/// Ask the running `git diff-tree --stdin` about each request, starting
/// it if needed. A request is a commit, which git diffs against its first
/// parent, or a commit and then another commit to diff it against. The
/// answer is `None` if the commit isn't there.
///
/// This uses `diff-tree` rather than `git diff`, because it keeps running
/// and answers one request after another. It reads `diff.renameLimit`
/// itself, but not `diff.renames`, which `start_diff_tree` reads for it.
/// Renames and copies then come out as `git show` shows them.
async fn diff_tree(git: &Git, requests: &[String]) -> Result<Vec<Option<Answer>>> {
    let mut process = match git.take_process("diff-tree") {
        Some(p) => p,
        None => start_diff_tree(git).await?,
    };
    let mut input = Vec::new();
    for r in requests {
        input.extend_from_slice(r.as_bytes());
        input.push(b'\n');
        input.extend_from_slice(END);
    }
    let (stdin, stdout) = process.io();
    // There are only a few short requests. They fit in the pipe, so git
    // takes them all before we read anything.
    if stdin.write_all(&input).await.is_err() {
        return Err(process.error("stopped").await);
    }
    let mut answers = Vec::new();
    for _ in requests {
        match read_answer(stdout).await {
            Ok(a) => answers.push(a),
            Err(e) => return Err(process.error(&format!("{e:#}")).await),
        }
    }
    git.put_process(process);
    Ok(answers)
}

/// Read one answer from `git diff-tree --stdin -z`, up to the `END` line
/// after it. The answer is the fields of `FORMAT`, then a NUL. If there
/// are changed files, a newline and the files follow, as in
/// `git diff --raw -z`.
async fn read_answer(out: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Answer>> {
    // Git prints nothing for a commit that isn't there.
    if peek(out).await? == END[0] {
        read_end(out).await?;
        return Ok(None);
    }
    let mut fields: [Vec<u8>; FIELDS] = Default::default();
    for f in &mut fields {
        *f = read_field(out).await?;
    }
    let mut raw = Vec::new();
    loop {
        match peek(out).await? {
            0 | b'\n' => out.consume(1),
            b':' => {
                let info = read_field(out).await?;
                // Renames and copies have two paths.
                let status = info.rsplit(|&b| b == b' ').next().unwrap_or_default();
                let paths = if matches!(status.first(), Some(b'R' | b'C')) {
                    2
                } else {
                    1
                };
                for piece in [info].into_iter().chain(read_fields(out, paths).await?) {
                    raw.extend_from_slice(&piece);
                    raw.push(0);
                }
            }
            b if b == END[0] => {
                read_end(out).await?;
                break;
            }
            b => bail!("unexpected byte {b:#x} after a commit"),
        }
    }
    let [_id, parents, an, ae, ad, cn, ce, cd, message] = &fields;
    let person = |name: &[u8], email: &[u8], date: &[u8]| {
        let (time, offset) = parse_raw_date(date);
        Person {
            name: String::from_utf8_lossy(name).trim().to_string(),
            email: String::from_utf8_lossy(email).trim().to_string(),
            time,
            offset,
        }
    };
    Ok(Some(Answer {
        parents: parents
            .split(|&b| b == b' ')
            .filter(|p| !p.is_empty())
            .map(ObjectId::from_hex)
            .collect::<Result<_>>()?,
        author: person(an, ae, ad),
        committer: person(cn, ce, cd),
        message: String::from_utf8_lossy(message).into_owned(),
        changes: parse_raw(&raw)?,
    }))
}

async fn peek(out: &mut (impl AsyncBufRead + Unpin)) -> Result<u8> {
    let buf = out.fill_buf().await?;
    buf.first().copied().context("stopped")
}

/// Read up to the next NUL, and leave it out.
async fn read_field(out: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
    let mut field = Vec::new();
    out.read_until(0, &mut field).await?;
    if field.pop() != Some(0) {
        bail!("stopped");
    }
    Ok(field)
}

async fn read_fields(out: &mut (impl AsyncBufRead + Unpin), n: usize) -> Result<Vec<Vec<u8>>> {
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        fields.push(read_field(out).await?);
    }
    Ok(fields)
}

async fn read_end(out: &mut (impl AsyncBufRead + Unpin)) -> Result<()> {
    let mut line = Vec::new();
    out.read_until(b'\n', &mut line).await?;
    if line != END {
        bail!("expected the end of an answer, got {:?}", line.as_bstr());
    }
    Ok(())
}

/// Start `git diff-tree --stdin` with the rename setting that `git diff`
/// would use.
async fn start_diff_tree(git: &Git) -> Result<Process> {
    let generation = git.process_generation();
    let renames = rename_flag(git).await?;
    let args = [
        "diff-tree",
        "--stdin",
        "-r",
        "-z",
        // A root commit is diffed against the empty tree. Without
        // `--always`, a commit with no changes would print nothing.
        "--root",
        "--always",
        "--no-abbrev",
        "--no-ext-diff",
        "--ignore-submodules=none",
        // We sort the files ourselves.
        "-O/dev/null",
        "--date=raw",
        renames,
        FORMAT,
    ];
    git.start_process("diff-tree", &args, generation)
}

/// The option that does what the user's `diff.renames` asks for. Like
/// `git diff`, we look for renames when it isn't set.
async fn rename_flag(git: &Git) -> Result<&'static str> {
    let opts = RunOpts {
        // Git exits with 1 when the setting isn't there.
        ok_codes: vec![1],
        ..RunOpts::default()
    };
    let config = |args: &'static [&'static str]| git.read_opts(args, &opts);
    let value = match config(&["config", "--bool", "--get", "diff.renames"]).await {
        Ok(out) => out.text(),
        // "copies" isn't a boolean. Git looks for it before it reads the
        // value as one.
        Err(e) => {
            let out = config(&["config", "--get", "diff.renames"]).await?.text();
            if !matches!(out.trim().to_ascii_lowercase().as_str(), "copies" | "copy") {
                return Err(e);
            }
            return Ok("-C");
        }
    };
    Ok(if value.trim() == "false" {
        "--no-renames"
    } else {
        "-M"
    })
}

/// One side of a changed file in `git diff --raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Side {
    path: BString,
    mode: String,
    id: ObjectId,
}

/// A changed file in `git diff --raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    status: FileStatus,
    similarity: Option<u8>,
    old: Option<Side>,
    new: Option<Side>,
}

/// Read the output of `git diff --raw -z`. Each file is a line such as
/// `:100644 100644 <old id> <new id> M` followed by its path. Renames and
/// copies have a score after their letter, and two paths.
fn parse_raw(out: &[u8]) -> Result<Vec<Change>> {
    let mut fields = out.split(|&b| b == 0);
    let mut changes = Vec::new();
    while let Some(info) = fields.next() {
        if info.is_empty() {
            continue;
        }
        let bad = || format!("can't read git's file list at {:?}", info.as_bstr());
        let info = info.strip_prefix(b":").with_context(bad)?;
        let parts: Vec<&[u8]> = info.split(|&b| b == b' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            bail!(bad());
        };
        let (letter, score) = status.split_first().with_context(bad)?;
        let status = match letter {
            b'A' => FileStatus::Added,
            b'D' => FileStatus::Deleted,
            b'M' => FileStatus::Modified,
            b'T' => FileStatus::TypeChange,
            b'R' => FileStatus::Renamed,
            b'C' => FileStatus::Copied,
            _ => bail!(bad()),
        };
        let similarity = score.to_str().ok().and_then(|s| s.parse().ok());
        let mut path = || fields.next().map(BString::from).with_context(bad);
        let old_path = path()?;
        let new_path = match status {
            FileStatus::Renamed | FileStatus::Copied => path()?,
            _ => old_path.clone(),
        };
        let side = |path: BString, mode: &[u8], id: &[u8]| -> Result<Option<Side>> {
            if mode.iter().all(|&b| b == b'0') {
                return Ok(None);
            }
            Ok(Some(Side {
                path,
                mode: String::from_utf8_lossy(mode).into_owned(),
                id: ObjectId::from_hex(id)?,
            }))
        };
        changes.push(Change {
            status,
            similarity,
            old: side(old_path, old_mode, old_id)?,
            new: side(new_path, new_mode, new_id)?,
        });
    }
    Ok(changes)
}

/// Whether a commit whose parents are `base`, `index` and `untracked` is
/// a stash made with `--include-untracked`.
///
/// Such a stash has three parents: the commit it was made on, a commit
/// with the staged changes on top of that, and a root commit with the
/// untracked files. Git always gives the last one a message that starts
/// with "untracked files on ". We check all of that, so an ordinary merge
/// with three parents isn't mistaken for a stash.
fn is_stash(base: ObjectId, index: &Answer, untracked: &Answer) -> bool {
    index.parents == [base]
        && untracked.parents.is_empty()
        && untracked.message.starts_with("untracked files on ")
}

/// Read the blobs of these changes from the running `git cat-file
/// --batch`, starting it if needed. Submodules have no blobs, so they are
/// left out.
async fn read_blobs(git: &Git, changes: &[Change]) -> Result<HashMap<ObjectId, Vec<u8>>> {
    let mut input = Vec::new();
    let mut wanted = std::collections::HashSet::new();
    for side in changes.iter().flat_map(|c| [&c.old, &c.new]).flatten() {
        if side.mode != "160000" && wanted.insert(side.id) {
            input.extend_from_slice(format!("{}\n", side.id).as_bytes());
        }
    }
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let mut process = match git.take_process("cat-file") {
        Some(p) => p,
        None => git.start_process(
            "cat-file",
            &["cat-file", "--batch"],
            git.process_generation(),
        )?,
    };
    let (stdin, stdout) = process.io();
    // Git starts answering before it has read every id. Writing them all
    // first could fill both pipes, and then we and git would wait for
    // each other forever.
    let write = stdin.write_all(&input);
    let read = async {
        let mut blobs = HashMap::new();
        for _ in 0..wanted.len() {
            let (id, data) = read_object(stdout).await?;
            blobs.insert(id, data);
        }
        anyhow::Ok(blobs)
    };
    match tokio::join!(write, read) {
        (Ok(()), Ok(blobs)) => {
            git.put_process(process);
            Ok(blobs)
        }
        (_, Err(e)) => Err(process.error(&format!("{e:#}")).await),
        (Err(_), Ok(_)) => Err(process.error("stopped").await),
    }
}

/// Read one object from `git cat-file --batch`: a line with its id, type
/// and size, then that many bytes and a newline.
async fn read_object(out: &mut (impl AsyncBufRead + Unpin)) -> Result<(ObjectId, Vec<u8>)> {
    let mut line = Vec::new();
    out.read_until(b'\n', &mut line).await?;
    if line.pop() != Some(b'\n') {
        bail!("stopped");
    }
    let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    // A missing object gives "<id> missing" instead.
    let [id, _kind, size] = parts[..] else {
        bail!("{}", line.as_bstr());
    };
    let size: usize = size
        .to_str()
        .ok()
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("{}", line.as_bstr()))?;
    let mut data = vec![0; size + 1];
    out.read_exact(&mut data).await?;
    if data.pop() != Some(b'\n') {
        bail!("no newline after object {}", id.as_bstr());
    }
    Ok((ObjectId::from_hex(id)?, data))
}

/// Turn the changes into `FileDiff`s with their hunks, and keep the texts
/// for highlighting. Both come back sorted by path.
fn files_of(
    changes: Vec<Change>,
    blobs: &HashMap<ObjectId, Vec<u8>>,
    context: u32,
    line_limit: usize,
) -> (Vec<FileDiff>, Vec<FileTexts>) {
    let mut files = Vec::new();
    let mut texts = Vec::new();
    for c in changes {
        let mut f = FileDiff::new(
            c.status,
            c.old.as_ref().map(|s| s.path.clone()),
            c.new.as_ref().map(|s| s.path.clone()),
        );
        f.similarity = c.similarity;
        f.old_id = c.old.as_ref().map(|s| s.id);
        f.new_id = c.new.as_ref().map(|s| s.id);
        f.old_mode = c.old.as_ref().map(|s| s.mode.clone());
        f.new_mode = c.new.as_ref().map(|s| s.mode.clone());

        let old_text = c.old.as_ref().map(|s| blob_text(s, blobs));
        let new_text = c.new.as_ref().map(|s| blob_text(s, blobs));
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
    sort_by_path(files, texts)
}

/// Sort the files by path, keeping `texts` in the same order as `files`.
fn sort_by_path(files: Vec<FileDiff>, texts: Vec<FileTexts>) -> (Vec<FileDiff>, Vec<FileTexts>) {
    let mut paired: Vec<(FileDiff, FileTexts)> = files.into_iter().zip(texts).collect();
    paired.sort_by(|a, b| a.0.path().cmp(b.0.path()));
    paired.into_iter().unzip()
}

/// The text of one side of a change, or `None` if it looks binary.
/// Submodule entries are shown the way git shows them.
fn blob_text(side: &Side, blobs: &HashMap<ObjectId, Vec<u8>>) -> Option<Arc<str>> {
    if side.mode == "160000" {
        return Some(format!("Subproject commit {}\n", side.id).into());
    }
    let data = blobs.get(&side.id).map_or(&[][..], Vec::as_slice);
    if is_binary(data) {
        return None;
    }
    Some(String::from_utf8_lossy(data).into())
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
