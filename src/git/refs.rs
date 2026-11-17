//! Branches, remotes, tags and stashes.
//!
//! Git allows ref names and remote names that aren't UTF-8, so they are
//! kept as bytes. They only turn into text when they are drawn.

use std::collections::HashMap;

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice as _};

use super::cli::Git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: BString,
    pub target: ObjectId,
    pub upstream: Option<BString>,
    pub ahead: u32,
    pub behind: u32,
    /// The upstream is configured but no longer exists on the remote.
    pub upstream_gone: bool,
    pub is_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: BString,
    /// The branch name without the remote prefix.
    pub name: BString,
    pub target: ObjectId,
}

impl RemoteBranch {
    pub fn full_name(&self) -> BString {
        [self.remote.as_slice(), b"/", self.name.as_slice()]
            .concat()
            .into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: BString,
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
    pub name: BString,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub head: Option<ObjectId>,
    /// The checked out branch, or `None` when detached or unborn.
    pub head_branch: Option<BString>,
    pub branches: Vec<Branch>,
    pub remotes: Vec<BString>,
    pub remote_branches: Vec<RemoteBranch>,
    pub tags: Vec<Tag>,
    pub stashes: Vec<Stash>,
}

impl Refs {
    pub async fn load(git: &Git) -> Result<Refs> {
        let mut refs = Refs::default();
        let out = git.read(&[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ]).await?;
        for line in out.stdout.lines() {
            let f: Vec<&[u8]> = line.split(|&b| b == 0).collect();
            if f.len() < 6 {
                continue;
            }
            let peeled = if f[2].is_empty() { f[1] } else { f[2] };
            let Ok(target) = ObjectId::from_hex(peeled) else {
                continue;
            };
            if let Some(name) = f[0].strip_prefix(b"refs/heads/") {
                let (ahead, behind, gone) = parse_track(&f[4].to_str_lossy());
                let is_head = f[5] == b"*";
                if is_head {
                    refs.head_branch = Some(name.into());
                }
                refs.branches.push(Branch {
                    name: name.into(),
                    target,
                    upstream: (!f[3].is_empty()).then(|| f[3].into()),
                    ahead,
                    behind,
                    upstream_gone: gone,
                    is_head,
                });
            } else if let Some(rest) = f[0].strip_prefix(b"refs/remotes/") {
                let Some((remote, name)) = rest.split_once_str("/") else {
                    continue;
                };
                if name == b"HEAD" {
                    continue;
                }
                refs.remote_branches.push(RemoteBranch {
                    remote: remote.into(),
                    name: name.into(),
                    target,
                });
            } else if let Some(name) = f[0].strip_prefix(b"refs/tags/") {
                refs.tags.push(Tag {
                    name: name.into(),
                    target,
                });
            }
        }
        refs.remotes = git
            .read(&["remote"])
            .await?
            .stdout
            .lines()
            .filter(|s| !s.is_empty())
            .map(BString::from)
            .collect();
        refs.head = git
            .read(&["rev-parse", "--verify", "-q", "HEAD"])
            .await
            .ok()
            .and_then(|o| ObjectId::from_hex(o.text().trim().as_bytes()).ok());
        refs.stashes = load_stashes(git).await.unwrap_or_default();
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
                kind: if b.is_head {
                    LabelKind::Head
                } else {
                    LabelKind::Local
                },
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

    pub fn branch(&self, name: &BStr) -> Option<&Branch> {
        self.branches.iter().find(|b| b.name == name)
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

async fn load_stashes(git: &Git) -> Result<Vec<Stash>> {
    let out = git
        .read_text(&["stash", "list", "--format=%H%x00%gs"])
        .await?;
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
