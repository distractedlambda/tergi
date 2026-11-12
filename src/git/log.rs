//! Walking the history with gitoxide, in batches, on a background thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::traverse::commit::topo;

use super::graph::{GraphBuilder, GraphRow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub summary: String,
    pub author: String,
    pub email: String,
    /// Author time in seconds since the epoch.
    pub time: i64,
}

pub struct Batch {
    pub generation: u64,
    pub commits: Vec<CommitInfo>,
    pub rows: Vec<GraphRow>,
    pub done: bool,
}

/// Walk all commits reachable from `tips` in topological order, sending
/// them to `sink` in batches. The walk stops early when `current` no longer
/// matches `generation`, which means a newer walk has replaced this one.
pub fn walk(
    repo: &gix::ThreadSafeRepository,
    tips: Vec<ObjectId>,
    generation: u64,
    current: &Arc<AtomicU64>,
    mut sink: impl FnMut(Batch),
) -> Result<()> {
    let mut repo = repo.to_thread_local();
    repo.object_cache_size_if_unset(64 * 1024 * 1024);
    let mut graph = GraphBuilder::new();
    let mut commits = Vec::new();
    let mut rows = Vec::new();
    // Send a small first batch so the first screen appears quickly.
    let mut batch_size = 200;

    // Tags can point at trees and blobs. git.git has one for a GPG key, for
    // example. The walk refuses to start from those, so leave them out.
    let tips: Vec<ObjectId> = tips
        .into_iter()
        .filter(|id| repo.find_header(*id).is_ok_and(|h| h.kind() == gix::object::Kind::Commit))
        .collect();

    if !tips.is_empty() {
        let walk = topo::Builder::from_iters(&repo.objects, tips, None::<Vec<ObjectId>>)
            .with_commit_graph(repo.commit_graph_if_enabled().ok().flatten())
            .sorting(topo::Sorting::TopoOrder)
            .build()
            .context("could not start the history walk")?;
        for info in walk {
            if current.load(Ordering::Relaxed) != generation {
                return Ok(());
            }
            let info = info.context("history walk failed")?;
            let commit = load_commit(&repo, info.id)?;
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
    }
    sink(Batch {
        generation,
        commits,
        rows,
        done: true,
    });
    Ok(())
}

pub fn load_commit(repo: &gix::Repository, id: ObjectId) -> Result<CommitInfo> {
    let obj = repo.find_commit(id).context("missing commit")?;
    let c = obj.decode().context("bad commit")?;
    let author = c.author().context("bad author")?;
    let time = author.time().map(|t| t.seconds).unwrap_or(0);
    let summary = c
        .message
        .split(|&b| b == b'\n')
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    Ok(CommitInfo {
        id,
        parents: c.parents().collect(),
        summary,
        author: String::from_utf8_lossy(author.name).trim().to_string(),
        email: String::from_utf8_lossy(author.email).trim().to_string(),
        time,
    })
}
