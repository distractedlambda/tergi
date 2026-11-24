//! Walking the history with `git log`, in batches, in a background task.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncBufReadExt as _;

use super::ObjectId;
use super::cli::Git;
use super::graph::{GraphBuilder, GraphRow};
use crate::util::Generation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub summary: String,
    pub author: String,
    pub email: String,
    /// Author time in seconds since the epoch.
    pub time: i64,
    /// The author's time zone, in seconds east of UTC.
    pub offset: i32,
}

pub struct Batch {
    pub generation: u64,
    pub commits: Vec<CommitInfo>,
    pub rows: Vec<GraphRow>,
    pub done: bool,
}

/// What `walk` asks git for, one field for each part of a `CommitInfo`.
/// Git ends each field with a NUL, and `-z` ends each commit with one.
/// Names and emails go through `.mailmap`, as `git log` shows them.
const FORMAT: &str = "--format=%H%x00%P%x00%aN%x00%aE%x00%ad%x00%B";
const FIELDS: usize = 6;

/// Walk all commits reachable from `tips` and `stashes` in topological
/// order, sending them to `sink` in batches. The walk stops as soon as
/// `generation` is no longer current in `walks`, which means a newer walk
/// has replaced this one.
///
/// A stash shows up as a commit on top of the commit it was made on. Git
/// keeps the staged changes and the untracked files of a stash in commits
/// of their own, which are its second and third parents. The walk leaves
/// those parents out, and leaves out their commits too, unless something
/// else leads to them.
pub async fn walk(
    git: &Git,
    tips: &[ObjectId],
    stashes: &[ObjectId],
    generation: u64,
    walks: &Generation,
    mut sink: impl FnMut(Batch),
) -> Result<()> {
    // Without a commit-graph file, git reads the whole history before it
    // prints the first commit. So we wait for a newer walk at the same
    // time as for git, and not only check between commits. Dropping the
    // read kills git.
    tokio::select! {
        biased;
        () = walks.replaced(generation) => Ok(()),
        r = read(git, tips, stashes, generation, walks, &mut sink) => r,
    }
}

async fn read(
    git: &Git,
    tips: &[ObjectId],
    stashes: &[ObjectId],
    generation: u64,
    walks: &Generation,
    sink: &mut impl FnMut(Batch),
) -> Result<()> {
    let mut graph = GraphBuilder::new();
    let mut commits = Vec::new();
    let mut rows = Vec::new();
    // Send a small first batch so the first screen appears quickly.
    let mut batch_size = 200;

    let tip_set: HashSet<ObjectId> = tips.iter().copied().collect();
    // A stash that a branch or tag points at is shown as it is.
    let stash_set: HashSet<ObjectId> = stashes
        .iter()
        .copied()
        .filter(|id| !tip_set.contains(id))
        .collect();
    // Commits that git may only list because a stash leads to them through
    // a parent that we leave out.
    let mut left_out: HashSet<ObjectId> = HashSet::new();

    if !tips.is_empty() || !stash_set.is_empty() {
        // The tips go in on stdin, because a repository can have more tags
        // than fit on a command line. Git skips tips that are trees or
        // blobs, which some tags point at.
        let input: String = tips
            .iter()
            .chain(stashes.iter().filter(|id| stash_set.contains(id)))
            .map(|id| format!("{id}\n"))
            .collect();
        let args = [
            "log",
            "--stdin",
            "--topo-order",
            "-z",
            "--date=raw",
            "--no-show-signature",
            "--no-notes",
            "--no-color",
            FORMAT,
        ];
        let mut git_log = git.stream(&args, input.into_bytes())?;
        let mut fields: Vec<Vec<u8>> = Vec::with_capacity(FIELDS);
        let mut field = Vec::new();
        loop {
            field.clear();
            let n = git_log
                .stdout()
                .read_until(0, &mut field)
                .await
                .context("reading the history from git failed")?;
            if n == 0 {
                break;
            }
            if field.pop() != Some(0) {
                bail!("git log ended in the middle of a commit");
            }
            fields.push(std::mem::take(&mut field));
            if fields.len() < FIELDS {
                continue;
            }
            // The read only gives way to `replaced` while it waits for
            // git. When git's output is already here, this check stops
            // the walk instead.
            if !walks.is_current(generation) {
                return Ok(());
            }
            let mut commit = parse_commit(&fields)?;
            fields.clear();
            // Git lists a commit only after all of its children. So if no
            // lane of the graph waits for this one by now, nothing we show
            // leads to it.
            if left_out.remove(&commit.id)
                && !tip_set.contains(&commit.id)
                && !graph.waits_for(commit.id)
            {
                left_out.extend(commit.parents);
                continue;
            }
            if stash_set.contains(&commit.id) && commit.parents.len() > 1 {
                left_out.extend(commit.parents.drain(1..));
            }
            rows.push(graph.push(commit.id, &commit.parents));
            commits.push(commit);
            if commits.len() >= batch_size {
                sink(Batch {
                    generation,
                    commits: std::mem::take(&mut commits),
                    rows: std::mem::take(&mut rows),
                    done: false,
                });
                batch_size = 5000;
            }
        }
        if !fields.is_empty() {
            bail!("git log ended in the middle of a commit");
        }
        git_log.finish().await?;
    }
    sink(Batch {
        generation,
        commits,
        rows,
        done: true,
    });
    Ok(())
}

/// Read one commit from the fields that `FORMAT` asks for.
fn parse_commit(fields: &[Vec<u8>]) -> Result<CommitInfo> {
    let [id, parents, name, email, date, message] = fields else {
        bail!("git log printed a commit with the wrong number of fields");
    };
    let parents = parents
        .split(|&b| b == b' ')
        .filter(|p| !p.is_empty())
        .map(ObjectId::from_hex)
        .collect::<Result<_>>()?;
    let (time, offset) = parse_raw_date(date);
    Ok(CommitInfo {
        id: ObjectId::from_hex(id)?,
        parents,
        summary: summary(message),
        author: String::from_utf8_lossy(name).trim().to_string(),
        email: String::from_utf8_lossy(email).trim().to_string(),
        time,
        offset,
    })
}

/// The first line of a message that isn't blank, without the spaces
/// around it.
fn summary(message: &[u8]) -> String {
    message
        .split(|&b| b == b'\n')
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// Read a date as `--date=raw` prints it, such as `1112911993 -0130`.
/// Gives the seconds since the epoch, and the time zone in seconds east
/// of UTC. A date that doesn't parse counts as the epoch in UTC.
pub fn parse_raw_date(date: &[u8]) -> (i64, i32) {
    let date = String::from_utf8_lossy(date);
    let mut parts = date.split_ascii_whitespace();
    let time = parts.next().and_then(|t| t.parse::<i64>().ok());
    let zone = parts.next().and_then(|z| {
        let (sign, digits) = match z.as_bytes().first()? {
            b'+' => (1, &z[1..]),
            b'-' => (-1, &z[1..]),
            _ => return None,
        };
        if digits.len() != 4 {
            return None;
        }
        let hours: i32 = digits[..2].parse().ok()?;
        let minutes: i32 = digits[2..].parse().ok()?;
        Some(sign * (hours * 3600 + minutes * 60))
    });
    match time {
        Some(t) => (t, zone.unwrap_or(0)),
        None => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_dates() {
        assert_eq!(parse_raw_date(b"1112911993 -0130"), (1112911993, -5400));
        assert_eq!(parse_raw_date(b"0 +0000"), (0, 0));
        assert_eq!(parse_raw_date(b"-5 +1400"), (-5, 50400));
        assert_eq!(parse_raw_date(b"12 bogus"), (12, 0));
        assert_eq!(parse_raw_date(b""), (0, 0));
    }

    #[test]
    fn summaries() {
        assert_eq!(
            summary(b"\n\n  Blank lines first  \n\nbody\n"),
            "Blank lines first"
        );
        assert_eq!(summary(b"Summary\nsame paragraph\n"), "Summary");
        assert_eq!(summary(b"\n \t\n"), "");
    }
}
