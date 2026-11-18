//! Property tests for loading refs, run against the real git.
//!
//! Each case builds a small random history with `git commit-tree`. Then it
//! adds random remotes, remote branches, local branches with and without
//! upstreams, tags and stashes. Names come from short lists that hold
//! slashes and bytes that aren't UTF-8. Remote names can hold slashes too,
//! so a remote branch like `a/b/main` can belong to remote `a/b` or to
//! remote `a`. Some remote branches belong to no configured remote, as when
//! someone writes the ref by hand.
//!
//! The test loads the refs with `Refs::load` and checks them against what
//! it built and what git says. Then it checks that the sidebar lists each
//! ref once, in the right group and under the right remote, and that the
//! history labels sit on the right commits.

mod common;

use std::collections::{HashMap, HashSet};

use common::Scratch;
use common::settings::{Settings, settings};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice as _};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use tergi::app::cmd::{SideKey, SideRow, sidebar_rows};
use tergi::git::refs::{LabelKind, Refs};

/// Names for remotes. Two of them are a remote and a folder of another.
const REMOTES: &[&[u8]] = &[b"origin", b"up", b"a", b"a/b", b"caf\xe9", b"x/\xff/y"];

/// The remote that remote branches without a configured remote live under.
/// It isn't in `REMOTES`.
const STRAY: &[u8] = b"stray";

/// Names for branches on a remote. With remote `a`, `b/main` is the same
/// ref as `main` on remote `a/b`.
const REMOTE_BRANCHES: &[&[u8]] = &[
    b"main",
    b"dev",
    b"feature",
    b"feature/x",
    b"b/main",
    b"\xff",
];

/// Names for local branches. `feature` and `feature/x` can't both exist.
const BRANCHES: &[&[u8]] = &[
    b"main",
    b"topic",
    b"feature",
    b"feature/x",
    b"a/b/main",
    b"caf\xe9",
    b"\xff\xfe",
];

const TAGS: &[&[u8]] = &[b"v1", b"v1.0", b"release/2", b"caf\xe9", b"deep/er/tag"];

/// Stash messages, and the message the sidebar shows for each.
const STASHES: &[(&[u8], &str)] = &[
    (b"wip", "On stashing: wip"),
    ("naïve ✓".as_bytes(), "On stashing: naïve ✓"),
    // Unlike the one above, this one fits in Latin-1, so git can convert
    // it to that if the user's log encoding asks for it.
    ("café".as_bytes(), "On stashing: café"),
    (b"bad \xff byte", "On stashing: bad \u{fffd} byte"),
];

#[derive(Debug, Clone)]
enum Upstream {
    None,
    /// A branch on a configured remote.
    Remote(Index),
    /// Another local branch.
    Local(Index),
    /// A configured remote that has no branch of that name.
    Gone(Index),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Light,
    Annotated,
    /// An annotated tag of an annotated tag.
    Nested,
}

#[derive(Debug, Clone)]
enum Head {
    Branch(Index),
    Unborn,
    Detached(Index),
}

#[derive(Debug, Clone)]
struct Case {
    /// The parents of each commit, as indexes of earlier commits.
    commits: Vec<Vec<usize>>,
    remotes: Vec<usize>,
    /// The remote, or `None` for one that isn't configured, the name and
    /// the commit.
    remote_branches: Vec<(Option<Index>, usize, Index)>,
    /// Remote branches that become their remote's `HEAD`.
    remote_heads: Vec<Index>,
    branches: Vec<(usize, Index, Upstream)>,
    tags: Vec<(usize, TagKind, Index)>,
    stashes: Vec<usize>,
    head: Head,
    /// Which headers and remotes to fold in the sidebar, in the order of
    /// the headers and then of `Refs::remotes`.
    collapsed: Vec<bool>,
    settings: Settings,
}

fn case() -> impl Strategy<Value = Case> {
    let parents = prop::collection::vec(any::<Index>(), 0..3);
    let commits = prop::collection::vec(parents, 1..8).prop_map(|raw| {
        raw.into_iter()
            .enumerate()
            .map(|(k, picks)| {
                let mut parents: Vec<usize> = Vec::new();
                if k > 0 {
                    for p in picks {
                        let p = p.index(k);
                        if !parents.contains(&p) {
                            parents.push(p);
                        }
                    }
                }
                parents
            })
            .collect()
    });
    let remote_branch = (
        prop::option::weighted(0.85, any::<Index>()),
        0..REMOTE_BRANCHES.len(),
        any::<Index>(),
    );
    let upstream = prop_oneof![
        2 => Just(Upstream::None),
        3 => any::<Index>().prop_map(Upstream::Remote),
        1 => any::<Index>().prop_map(Upstream::Local),
        1 => any::<Index>().prop_map(Upstream::Gone),
    ];
    let tag_kind = prop_oneof![
        Just(TagKind::Light),
        Just(TagKind::Annotated),
        Just(TagKind::Nested)
    ];
    let head = prop_oneof![
        4 => any::<Index>().prop_map(Head::Branch),
        1 => Just(Head::Unborn),
        2 => any::<Index>().prop_map(Head::Detached),
    ];
    (
        commits,
        prop::collection::vec(0..REMOTES.len(), 0..4),
        prop::collection::vec(remote_branch, 0..10),
        prop::collection::vec(any::<Index>(), 0..3),
        prop::collection::vec((0..BRANCHES.len(), any::<Index>(), upstream), 0..6),
        prop::collection::vec((0..TAGS.len(), tag_kind, any::<Index>()), 0..4),
        prop::collection::vec(0..STASHES.len(), 0..4),
        head,
        prop::collection::vec(prop::bool::weighted(0.2), 16),
        settings(),
    )
        .prop_map(
            |(
                commits,
                remotes,
                remote_branches,
                remote_heads,
                branches,
                tags,
                stashes,
                head,
                collapsed,
                settings,
            )| Case {
                commits,
                remotes,
                remote_branches,
                remote_heads,
                branches,
                tags,
                stashes,
                head,
                collapsed,
                settings,
            },
        )
}

#[derive(Debug)]
enum Up {
    /// An index into `Model::remote_branches`.
    Remote(usize),
    /// An index into `Model::branches`.
    Local(usize),
    /// An index into `Model::remotes`.
    Gone(usize),
}

#[derive(Debug)]
struct RemoteBranch {
    remote: BString,
    name: BString,
    commit: usize,
}

#[derive(Debug)]
struct Branch {
    name: BString,
    commit: usize,
    upstream: Option<Up>,
}

#[derive(Debug)]
enum ModelHead {
    Branch(usize),
    Unborn,
    Detached(usize),
}

/// The refs a case makes, after leaving out the ones that would clash with
/// a ref made before them.
#[derive(Debug)]
struct Model {
    remotes: Vec<BString>,
    remote_branches: Vec<RemoteBranch>,
    /// Remotes with a `HEAD`, and the remote branch it points at.
    remote_heads: Vec<(BString, usize)>,
    branches: Vec<Branch>,
    tags: Vec<(BString, TagKind, usize)>,
    /// Stash messages, oldest first.
    stashes: Vec<usize>,
    head: ModelHead,
}

fn cat(parts: &[&[u8]]) -> BString {
    parts.concat().into()
}

/// Whether a new ref can sit next to the ones already taken. Git can't
/// have both `a` and `a/b`, because one would be a file and the other a
/// folder.
fn take(taken: &mut Vec<BString>, name: BString) -> bool {
    let clash = taken.iter().any(|t| {
        t == &name || name.starts_with(&cat(&[t, b"/"])) || t.starts_with(&cat(&[&name, b"/"]))
    });
    if !clash {
        taken.push(name);
    }
    !clash
}

fn model(case: &Case) -> Model {
    let n = case.commits.len();
    let mut taken: Vec<BString> = vec!["refs/heads/stashing".into(), "refs/heads/unborn".into()];

    let mut remotes: Vec<BString> = Vec::new();
    for &r in &case.remotes {
        let r = BString::from(REMOTES[r]);
        if !remotes.contains(&r) {
            remotes.push(r);
        }
    }

    let mut remote_branches = Vec::new();
    for (remote, name, commit) in &case.remote_branches {
        let remote = match remote {
            Some(r) if !remotes.is_empty() => remotes[r.index(remotes.len())].clone(),
            _ => STRAY.into(),
        };
        let name = BString::from(REMOTE_BRANCHES[*name]);
        // With remotes `a` and `a/b`, `a/b/main` belongs to `a/b`, so
        // branch `b/main` of `a` can't exist.
        let rest = cat(&[&remote, b"/", &name]);
        let ambiguous = remotes
            .iter()
            .any(|r| r.len() > remote.len() && rest.starts_with(&cat(&[r, b"/"])));
        if !ambiguous && take(&mut taken, cat(&[b"refs/remotes/", &rest])) {
            remote_branches.push(RemoteBranch {
                remote,
                name,
                commit: commit.index(n),
            });
        }
    }

    let mut remote_heads: Vec<(BString, usize)> = Vec::new();
    if !remote_branches.is_empty() {
        for i in &case.remote_heads {
            let i = i.index(remote_branches.len());
            let remote = &remote_branches[i].remote;
            if !remote_heads.iter().any(|(r, _)| r == remote)
                && take(&mut taken, cat(&[b"refs/remotes/", remote, b"/HEAD"]))
            {
                remote_heads.push((remote.clone(), i));
            }
        }
    }

    // Only branches on configured remotes can be upstreams.
    let trackable: Vec<usize> = (0..remote_branches.len())
        .filter(|&i| remotes.contains(&remote_branches[i].remote))
        .collect();
    let mut branches: Vec<Branch> = Vec::new();
    for (name, commit, _) in &case.branches {
        let name = BString::from(BRANCHES[*name]);
        if take(&mut taken, cat(&[b"refs/heads/", &name])) {
            branches.push(Branch {
                name,
                commit: commit.index(n),
                upstream: None,
            });
        }
    }
    // Upstreams are picked once all the branches exist, so that a branch
    // can track one made after it.
    let mut seen = HashSet::new();
    for (name, _, upstream) in &case.branches {
        let Some(k) = branches
            .iter()
            .position(|b| b.name.as_slice() == BRANCHES[*name])
        else {
            continue;
        };
        // A name that came up twice made one branch, which gets the
        // upstream of its first appearance.
        if !seen.insert(k) {
            continue;
        }
        branches[k].upstream = match upstream {
            Upstream::None => None,
            Upstream::Remote(i) if !trackable.is_empty() => {
                Some(Up::Remote(trackable[i.index(trackable.len())]))
            }
            Upstream::Local(i) => Some(Up::Local(i.index(branches.len()))),
            Upstream::Gone(i) if !remotes.is_empty() => Some(Up::Gone(i.index(remotes.len()))),
            Upstream::Remote(_) | Upstream::Gone(_) => None,
        };
    }

    let mut tags = Vec::new();
    for (name, kind, commit) in &case.tags {
        let name = BString::from(TAGS[*name]);
        if take(&mut taken, cat(&[b"refs/tags/", &name])) {
            tags.push((name, *kind, commit.index(n)));
        }
    }

    let head = match &case.head {
        Head::Branch(i) if !branches.is_empty() => ModelHead::Branch(i.index(branches.len())),
        Head::Branch(_) | Head::Unborn => ModelHead::Unborn,
        Head::Detached(k) => ModelHead::Detached(k.index(n)),
    };

    Model {
        remotes,
        remote_branches,
        remote_heads,
        branches,
        tags,
        stashes: case.stashes.clone(),
        head,
    }
}

fn trim(out: Vec<u8>) -> String {
    String::from_utf8(out).expect("utf-8").trim().to_string()
}

/// Build the history and the refs, and return the commit ids.
fn build(s: &Scratch, case: &Case, m: &Model) -> Vec<String> {
    s.write("f", "base\n");
    s.sh(&["add", "f"]);
    let tree = s.sh(&["write-tree"]).trim().to_string();
    let mut ids: Vec<String> = Vec::new();
    for (k, parents) in case.commits.iter().enumerate() {
        let mut args = vec!["commit-tree".to_string(), tree.clone()];
        for &p in parents {
            args.push("-p".into());
            args.push(ids[p].clone());
        }
        args.push("-m".into());
        args.push(format!("c{k}"));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        ids.push(s.sh(&args).trim().to_string());
    }

    if !m.stashes.is_empty() {
        s.sh(&["checkout", "-q", "-B", "stashing", &ids[0]]);
        for (k, &msg) in m.stashes.iter().enumerate() {
            s.write("f", &format!("stash {k}\n"));
            s.sh_bytes(&[b"stash", b"push", b"-q", b"-m", STASHES[msg].0]);
        }
    }

    // Write what `git remote add` would. Newer git refuses to add `a` next
    // to `a/b`, but repositories set up by hand or with an older git can
    // still have both.
    for r in &m.remotes {
        let key = |k: &[u8]| cat(&[b"remote.", r, b".", k]);
        let fetch = cat(&[b"+refs/heads/*:refs/remotes/", r, b"/*"]);
        s.sh_bytes(&[b"config", &key(b"url"), b"/nowhere"]);
        s.sh_bytes(&[b"config", &key(b"fetch"), &fetch]);
    }
    for rb in &m.remote_branches {
        let name = cat(&[b"refs/remotes/", &rb.remote, b"/", &rb.name]);
        s.sh_bytes(&[b"update-ref", &name, ids[rb.commit].as_bytes()]);
    }
    for (remote, i) in &m.remote_heads {
        let rb = &m.remote_branches[*i];
        let head = cat(&[b"refs/remotes/", remote, b"/HEAD"]);
        let target = cat(&[b"refs/remotes/", &rb.remote, b"/", &rb.name]);
        s.sh_bytes(&[b"symbolic-ref", &head, &target]);
    }
    for b in &m.branches {
        let name = cat(&[b"refs/heads/", &b.name]);
        s.sh_bytes(&[b"update-ref", &name, ids[b.commit].as_bytes()]);
    }
    for b in &m.branches {
        let (remote, merge) = match &b.upstream {
            None => continue,
            Some(Up::Remote(i)) => {
                let rb = &m.remote_branches[*i];
                (rb.remote.clone(), cat(&[b"refs/heads/", &rb.name]))
            }
            Some(Up::Local(i)) => (".".into(), cat(&[b"refs/heads/", &m.branches[*i].name])),
            Some(Up::Gone(i)) => (m.remotes[*i].clone(), "refs/heads/gone".into()),
        };
        let key = |k: &[u8]| cat(&[b"branch.", &b.name, b".", k]);
        s.sh_bytes(&[b"config", &key(b"remote"), &remote]);
        s.sh_bytes(&[b"config", &key(b"merge"), &merge]);
    }
    for (name, kind, commit) in &m.tags {
        let id = ids[*commit].as_bytes();
        match kind {
            TagKind::Light => {
                s.sh_bytes(&[b"update-ref", &cat(&[b"refs/tags/", name]), id]);
            }
            TagKind::Annotated => {
                s.sh_bytes(&[b"tag", b"-a", b"-m", b"annotated", name, id]);
            }
            TagKind::Nested => {
                let inner = cat(&[name, b"-inner"]);
                s.sh_bytes(&[b"tag", b"-a", b"-m", b"inner", &inner, id]);
                s.sh_bytes(&[b"tag", b"-a", b"-m", b"outer", name, &inner]);
                s.sh_bytes(&[b"tag", b"-d", &inner]);
            }
        }
    }

    match &m.head {
        ModelHead::Branch(i) => {
            let name = cat(&[b"refs/heads/", &m.branches[*i].name]);
            s.sh_bytes(&[b"symbolic-ref", b"HEAD", &name]);
        }
        ModelHead::Unborn => {
            s.sh(&["symbolic-ref", "HEAD", "refs/heads/unborn"]);
        }
        ModelHead::Detached(k) => {
            s.sh(&["update-ref", "--no-deref", "HEAD", &ids[*k]]);
        }
    }
    if !m.stashes.is_empty() {
        s.sh(&["update-ref", "-d", "refs/heads/stashing"]);
    }
    ids
}

fn id(s: &str) -> ObjectId {
    ObjectId::from_hex(s.as_bytes()).expect("id")
}

/// What a sidebar row shows, for comparing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    Branch(BString),
    Remote(BString),
    RemoteBranch(BString, BString),
    Tag(BString),
    Stash(usize, ObjectId),
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let m = model(case);
    let s = Scratch::with(&case.settings);
    if !s.allow_non_utf8_refs() {
        return Ok(());
    }
    let ids = build(&s, case, &m);
    let commit = |k: usize| id(&ids[k]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let refs = rt.block_on(Refs::load(&s.git)).expect("load refs");

    // HEAD.
    let (head, head_branch) = match &m.head {
        ModelHead::Branch(i) => (
            Some(commit(m.branches[*i].commit)),
            Some(m.branches[*i].name.clone()),
        ),
        ModelHead::Unborn => (None, None),
        ModelHead::Detached(k) => (Some(commit(*k)), None),
    };
    prop_assert_eq!(refs.head, head);
    prop_assert_eq!(&refs.head_branch, &head_branch);

    // Local branches, with their upstreams. Git counts ahead and behind
    // with `rev-list`.
    prop_assert_eq!(refs.branches.len(), m.branches.len());
    for (k, b) in m.branches.iter().enumerate() {
        let got = refs.branch(b.name.as_ref());
        prop_assert!(got.is_some(), "branch {} is missing", b.name);
        let got = got.expect("branch");
        prop_assert_eq!(got.target, commit(b.commit), "branch {}", b.name);
        prop_assert_eq!(
            got.is_head,
            matches!(m.head, ModelHead::Branch(i) if i == k),
            "branch {}",
            b.name
        );
        prop_assert_eq!(
            got.upstream.is_some(),
            b.upstream.is_some(),
            "branch {}",
            b.name
        );
        let tracking = match &b.upstream {
            None | Some(Up::Gone(_)) => None,
            Some(Up::Remote(i)) => {
                let rb = &m.remote_branches[*i];
                Some(cat(&[b"refs/remotes/", &rb.remote, b"/", &rb.name]))
            }
            Some(Up::Local(i)) => Some(cat(&[b"refs/heads/", &m.branches[*i].name])),
        };
        prop_assert_eq!(
            got.upstream_gone,
            matches!(b.upstream, Some(Up::Gone(_))),
            "branch {}",
            b.name
        );
        if let Some(tracking) = tracking {
            // The upstream's short name must name the tracking branch.
            let up = got.upstream.as_ref().expect("upstream");
            let full = s.sh_bytes(&[b"rev-parse", b"--symbolic-full-name", up]);
            prop_assert_eq!(full.trim(), tracking.as_slice(), "upstream of {}", b.name);
            let range = cat(&[b"refs/heads/", &b.name, b"...", &tracking]);
            let counts = trim(s.sh_bytes(&[b"rev-list", b"--left-right", b"--count", &range]));
            prop_assert_eq!(
                counts,
                format!("{}\t{}", got.ahead, got.behind),
                "branch {}",
                b.name
            );
        } else {
            prop_assert_eq!((got.ahead, got.behind), (0, 0), "branch {}", b.name);
        }
    }

    // Remotes and their branches. A remote's `HEAD` is left out.
    let mut remotes: Vec<BString> = m.remotes.clone();
    if m.remote_branches.iter().any(|rb| rb.remote == STRAY) {
        remotes.push(STRAY.into());
    }
    let mut got_remotes = refs.remotes.clone();
    got_remotes.sort();
    remotes.sort();
    prop_assert_eq!(&got_remotes, &remotes);
    let mut want: Vec<(BString, BString, ObjectId)> = m
        .remote_branches
        .iter()
        .map(|rb| (rb.remote.clone(), rb.name.clone(), commit(rb.commit)))
        .collect();
    let mut got: Vec<(BString, BString, ObjectId)> = refs
        .remote_branches
        .iter()
        .map(|rb| (rb.remote.clone(), rb.name.clone(), rb.target))
        .collect();
    want.sort();
    got.sort();
    prop_assert_eq!(&got, &want);

    // Tags, peeled to the commit.
    let mut want: Vec<(BString, ObjectId)> = m
        .tags
        .iter()
        .map(|(name, _, k)| (name.clone(), commit(*k)))
        .collect();
    let mut got: Vec<(BString, ObjectId)> = refs
        .tags
        .iter()
        .map(|t| (t.name.clone(), t.target))
        .collect();
    want.sort();
    got.sort();
    prop_assert_eq!(&got, &want);

    // Stashes, newest first, as `stash@{0}`, `stash@{1}` and so on.
    prop_assert_eq!(refs.stashes.len(), m.stashes.len());
    for (i, st) in refs.stashes.iter().enumerate() {
        let want = s.sh(&["rev-parse", &format!("stash@{{{i}}}")]);
        prop_assert_eq!(st.id, id(want.trim()), "stash {}", i);
        prop_assert_eq!(st.index, i);
        let msg = m.stashes[m.stashes.len() - 1 - i];
        prop_assert_eq!(&st.message, STASHES[msg].1, "stash {}", i);
    }

    check_sidebar(case, &refs)?;
    check_labels(&s, &refs)?;
    Ok(())
}

/// Each ref shows up once in the sidebar, in its group and under its
/// remote, unless its group or remote is folded.
fn check_sidebar(case: &Case, refs: &Refs) -> Result<(), TestCaseError> {
    let headers = ["branches", "remotes", "tags", "stashes"];
    let mut collapsed = HashSet::new();
    for (i, h) in headers.iter().enumerate() {
        if case.collapsed[i] {
            collapsed.insert(SideKey::Header(h.to_string()));
        }
    }
    for (i, r) in refs.remotes.iter().enumerate() {
        if case.collapsed.get(headers.len() + i) == Some(&true) {
            collapsed.insert(SideKey::Remote(r.clone()));
        }
    }
    let folded = |key: &str| collapsed.contains(&SideKey::Header(key.into()));

    let mut want = Vec::new();
    if !folded("branches") {
        want.extend(refs.branches.iter().map(|b| Entry::Branch(b.name.clone())));
    }
    if !folded("remotes") {
        for r in &refs.remotes {
            want.push(Entry::Remote(r.clone()));
            if !collapsed.contains(&SideKey::Remote(r.clone())) {
                want.extend(
                    refs.remote_branches
                        .iter()
                        .filter(|rb| &rb.remote == r)
                        .map(|rb| Entry::RemoteBranch(r.clone(), rb.name.clone())),
                );
            }
        }
    }
    if !folded("tags") {
        want.extend(refs.tags.iter().map(|t| Entry::Tag(t.name.clone())));
    }
    if !folded("stashes") {
        want.extend(
            refs.stashes
                .iter()
                .enumerate()
                .map(|(i, st)| Entry::Stash(i, st.id)),
        );
    }

    let rows = sidebar_rows(refs, &collapsed);
    let mut group = String::new();
    let mut remote: Option<&BString> = None;
    let mut got = Vec::new();
    let mut stash_row = 0;
    for row in &rows {
        match row {
            SideRow::Header { key, count, .. } => {
                let want_count = match key.as_str() {
                    "branches" => refs.branches.len(),
                    "remotes" => refs.remotes.len(),
                    "tags" => refs.tags.len(),
                    "stashes" => refs.stashes.len(),
                    other => return Err(TestCaseError::fail(format!("header {other}"))),
                };
                prop_assert_eq!(*count, want_count, "header {}", key);
                group = key.clone();
                remote = None;
            }
            SideRow::Branch(i) => {
                prop_assert_eq!(group.as_str(), "branches");
                got.push(Entry::Branch(refs.branches[*i].name.clone()));
            }
            SideRow::Remote(r) => {
                prop_assert_eq!(group.as_str(), "remotes");
                remote = Some(r);
                got.push(Entry::Remote(r.clone()));
            }
            SideRow::RemoteBranch(i) => {
                prop_assert_eq!(group.as_str(), "remotes");
                let rb = &refs.remote_branches[*i];
                prop_assert_eq!(remote, Some(&rb.remote), "remote branch {}", rb.full_name());
                got.push(Entry::RemoteBranch(rb.remote.clone(), rb.name.clone()));
            }
            SideRow::Tag(i) => {
                prop_assert_eq!(group.as_str(), "tags");
                got.push(Entry::Tag(refs.tags[*i].name.clone()));
            }
            SideRow::Stash(i) => {
                prop_assert_eq!(group.as_str(), "stashes");
                // Stash rows come in the order of `git stash list`.
                prop_assert_eq!(*i, stash_row);
                stash_row += 1;
                got.push(Entry::Stash(*i, refs.stashes[*i].id));
            }
            SideRow::Empty(_) => {
                prop_assert_eq!(group.as_str(), "branches");
                prop_assert!(refs.branches.is_empty());
            }
        }
    }
    for h in headers {
        let shown = rows
            .iter()
            .filter(|r| matches!(r, SideRow::Header { key, .. } if key == h))
            .count();
        prop_assert_eq!(shown, 1, "header {}", h);
    }
    want.sort();
    got.sort();
    prop_assert_eq!(got, want);
    Ok(())
}

/// Each ref has one label, on the commit that git peels it to.
fn check_labels(s: &Scratch, refs: &Refs) -> Result<(), TestCaseError> {
    let peel = |full: &[u8]| {
        let spec = cat(&[full, b"^{commit}"]);
        id(&trim(s.sh_bytes(&[b"rev-parse", b"--verify", &spec])))
    };
    let mut want: Vec<(ObjectId, String, BString)> = Vec::new();
    if let (Some(head), None) = (refs.head, &refs.head_branch) {
        want.push((head, format!("{:?}", LabelKind::Head), "HEAD".into()));
    }
    for b in &refs.branches {
        let kind = if b.is_head {
            LabelKind::Head
        } else {
            LabelKind::Local
        };
        let at = peel(&cat(&[b"refs/heads/", &b.name]));
        want.push((at, format!("{kind:?}"), b.name.clone()));
    }
    for rb in &refs.remote_branches {
        let full = cat(&[b"refs/remotes/", &rb.remote, b"/", &rb.name]);
        want.push((
            peel(&full),
            format!("{:?}", LabelKind::Remote),
            rb.full_name(),
        ));
    }
    for t in &refs.tags {
        let at = peel(&cat(&[b"refs/tags/", &t.name]));
        want.push((at, format!("{:?}", LabelKind::Tag), t.name.clone()));
    }
    let labels: HashMap<ObjectId, Vec<tergi::git::refs::Label>> = refs.labels();
    let mut got: Vec<(ObjectId, String, BString)> = labels
        .iter()
        .flat_map(|(at, ls)| {
            ls.iter()
                .map(move |l| (*at, format!("{:?}", l.kind), l.name.clone()))
        })
        .collect();
    want.sort();
    got.sort();
    prop_assert_eq!(got, want);
    Ok(())
}

/// Run `cases` cases, unless `PROPTEST_CASES` asks for some other number.
/// `ProptestConfig::with_cases` would ignore the variable.
fn config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = cases;
    }
    config
}

proptest! {
    #![proptest_config(config(256))]

    #[test]
    fn refs_match_git(case in case()) {
        check(&case)?;
    }
}
