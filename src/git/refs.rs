//! Branches, remotes, tags and stashes.

use std::collections::HashMap;

use anyhow::Result;
use gix::ObjectId;

use super::cli::Git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub target: ObjectId,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// The upstream is configured but no longer exists on the remote.
    pub upstream_gone: bool,
    pub is_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    /// The branch name without the remote prefix.
    pub name: String,
    pub target: ObjectId,
}

impl RemoteBranch {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.remote, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    /// The commit the tag points at, after peeling annotated tags.
    pub target: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub id: ObjectId,
    pub message: String,
}

impl Stash {
    pub fn refname(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Head,
    Local,
    Remote,
    Tag,
}

/// A ref name drawn next to a commit in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub kind: LabelKind,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub head: Option<ObjectId>,
    /// The checked out branch, or `None` when detached or unborn.
    pub head_branch: Option<String>,
    pub branches: Vec<Branch>,
    pub remotes: Vec<String>,
    pub remote_branches: Vec<RemoteBranch>,
    pub tags: Vec<Tag>,
    pub stashes: Vec<Stash>,
}

impl Refs {
    pub fn load(git: &Git) -> Result<Refs> {
        let mut refs = Refs::default();
        let out = git.read_text(&[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])?;
        for line in out.lines() {
            let f: Vec<&str> = line.split('\0').collect();
            if f.len() < 6 {
                continue;
            }
            let peeled = if f[2].is_empty() { f[1] } else { f[2] };
            let Ok(target) = ObjectId::from_hex(peeled.as_bytes()) else {
                continue;
            };
            if let Some(name) = f[0].strip_prefix("refs/heads/") {
                let (ahead, behind, gone) = parse_track(f[4]);
                let is_head = f[5] == "*";
                if is_head {
                    refs.head_branch = Some(name.to_string());
                }
                refs.branches.push(Branch {
                    name: name.to_string(),
                    target,
                    upstream: (!f[3].is_empty()).then(|| f[3].to_string()),
                    ahead,
                    behind,
                    upstream_gone: gone,
                    is_head,
                });
            } else if let Some(rest) = f[0].strip_prefix("refs/remotes/") {
                let Some((remote, name)) = rest.split_once('/') else {
                    continue;
                };
                if name == "HEAD" {
                    continue;
                }
                refs.remote_branches.push(RemoteBranch {
                    remote: remote.to_string(),
                    name: name.to_string(),
                    target,
                });
            } else if let Some(name) = f[0].strip_prefix("refs/tags/") {
                refs.tags.push(Tag {
                    name: name.to_string(),
                    target,
                });
            }
        }
        refs.remotes = git
            .read_text(&["remote"])?
            .lines()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect();
        refs.head = git
            .read(&["rev-parse", "--verify", "-q", "HEAD"])
            .ok()
            .and_then(|o| ObjectId::from_hex(o.text().trim().as_bytes()).ok());
        refs.stashes = load_stashes(git).unwrap_or_default();
        Ok(refs)
    }

    /// Labels to draw next to each commit in the history.
    pub fn labels(&self) -> HashMap<ObjectId, Vec<Label>> {
        let mut map: HashMap<ObjectId, Vec<Label>> = HashMap::new();
        if let (Some(head), None) = (self.head, &self.head_branch) {
            map.entry(head).or_default().push(Label {
                kind: LabelKind::Head,
                name: "HEAD".into(),
            });
        }
        for b in &self.branches {
            map.entry(b.target).or_default().push(Label {
                kind: if b.is_head { LabelKind::Head } else { LabelKind::Local },
                name: b.name.clone(),
            });
        }
        for r in &self.remote_branches {
            map.entry(r.target).or_default().push(Label {
                kind: LabelKind::Remote,
                name: r.full_name(),
            });
        }
        for t in &self.tags {
            map.entry(t.target).or_default().push(Label {
                kind: LabelKind::Tag,
                name: t.name.clone(),
            });
        }
        map
    }

    /// Commits the history walk starts from.
    pub fn tips(&self) -> Vec<ObjectId> {
        let mut v: Vec<ObjectId> = Vec::new();
        v.extend(self.head);
        v.extend(self.branches.iter().map(|b| b.target));
        v.extend(self.remote_branches.iter().map(|b| b.target));
        v.extend(self.tags.iter().map(|t| t.target));
        let mut seen = std::collections::HashSet::new();
        v.retain(|id| seen.insert(*id));
        v
    }

    pub fn current_branch(&self) -> Option<&Branch> {
        self.branches.iter().find(|b| b.is_head)
    }
}

fn parse_track(s: &str) -> (u32, u32, bool) {
    if s == "gone" {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in s.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

fn load_stashes(git: &Git) -> Result<Vec<Stash>> {
    let out = git.read_text(&["stash", "list", "--format=%H%x00%gs"])?;
    Ok(out
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let (id, msg) = line.split_once('\0')?;
            Some(Stash {
                index,
                id: ObjectId::from_hex(id.as_bytes()).ok()?,
                message: msg.to_string(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track() {
        assert_eq!(parse_track(""), (0, 0, false));
        assert_eq!(parse_track("ahead 3"), (3, 0, false));
        assert_eq!(parse_track("ahead 3, behind 2"), (3, 2, false));
        assert_eq!(parse_track("gone"), (0, 0, true));
    }
}
