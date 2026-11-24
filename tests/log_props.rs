//! Property tests for the history walk, run against the real git.
//!
//! Each case builds a random history with `git fast-import`. It has merges,
//! several root commits, and committer dates that go backwards, so a commit
//! can be older than its parents. Branches, remote branches and tags point
//! into it, and some commits aren't reachable from any of them. Some tags
//! are annotated, some point at other tags, and some point at trees or
//! blobs. HEAD is on a branch, detached, or unborn. Some cases write a
//! commit-graph file, and some write one that only covers part of the
//! history. Some cases have stashes, made on any commit, with or without
//! untracked files. A branch can point at a stash, or at the commit that
//! holds its staged changes.
//!
//! The test loads the refs with `Refs::load` and walks from their tips
//! and stashes with `log::walk`, as the app does. It checks the result
//! against `git rev-list`. Each reachable commit must come out once,
//! after all of its children, with the right parents, author, date and
//! summary. A stash must come out with only its first parent, and the
//! commits git makes for its staged changes and untracked files must not
//! come out, unless a branch leads to them.

use crate::common;

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use common::Scratch;
use common::settings::{Settings, settings};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use tergi::git::ObjectId;
use tergi::git::log;
use tergi::git::refs::Refs;
use tergi::util::Generation;

/// Commit messages, and the summary the history shows for each.
const MESSAGES: &[(&[u8], &str)] = &[
    (b"Add the walk\n", "Add the walk"),
    (b"No newline at the end", "No newline at the end"),
    (b"\n\n  Blank lines first  \n\nbody\n", "Blank lines first"),
    (b"Summary\nsame paragraph\n", "Summary"),
    (b"", ""),
    (b"\n \t\n", ""),
    ("naïve café ✓\n".as_bytes(), "naïve café ✓"),
    (b"\tCarriage returns\r\n\r\nbody\r\n", "Carriage returns"),
    (b"bad \xff byte\n", "bad \u{fffd} byte"),
];

/// Authors as fast-import takes them, and the name and email we expect.
const AUTHORS: &[(&str, &str, &str)] = &[
    ("Ann <ann@example.com>", "Ann", "ann@example.com"),
    (
        "Zoë Ünicode <zoe@example.com>",
        "Zoë Ünicode",
        "zoe@example.com",
    ),
    ("<nobody@example.com>", "", "nobody@example.com"),
    ("No Email <>", "No Email", ""),
];

/// Time zones as git writes them, and in seconds east of UTC.
const OFFSETS: &[(&str, i32)] = &[
    ("+0000", 0),
    ("-0500", -18_000),
    ("+0530", 19_800),
    ("+0045", 2_700),
    ("+1400", 50_400),
    ("-1200", -43_200),
];

#[derive(Debug, Clone)]
struct Commit {
    /// Indexes of earlier commits, without duplicates.
    parents: Vec<usize>,
    message: usize,
    author: usize,
    time: i64,
    offset: usize,
    /// The committer date. It is random too, so the dates don't follow
    /// the order of the history.
    committed: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Branch,
    Remote,
    Tag,
    Annotated,
    /// An annotated tag of an annotated tag.
    Nested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Object {
    Commit,
    Tree,
    Blob,
}

#[derive(Debug, Clone)]
struct Ref {
    kind: Kind,
    /// What a tag points at. Branches always point at the commit.
    object: Object,
    commit: Index,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StashBranch {
    None,
    /// A branch points at the stash commit itself.
    Stash,
    /// A branch points at the commit with the stash's staged changes.
    Index,
}

#[derive(Debug, Clone)]
struct Stash {
    /// The commit the stash was made on.
    base: Index,
    untracked: bool,
    branch: StashBranch,
}

#[derive(Debug, Clone)]
enum Head {
    /// On the branch with this index among the branches, or unborn if
    /// there are none.
    Branch(Index),
    Unborn,
    /// Detached at a commit, which may not be reachable from any ref.
    Detached(Index),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitGraph {
    None,
    Full,
    /// Covers the first half of the commits.
    Partial,
}

#[derive(Debug, Clone)]
struct Case {
    commits: Vec<Commit>,
    refs: Vec<Ref>,
    head: Head,
    /// Point `refs/remotes/origin/HEAD` at the first remote branch.
    remote_head: bool,
    graph: CommitGraph,
    stashes: Vec<Stash>,
    settings: Settings,
}

fn commit() -> impl Strategy<Value = (usize, [Index; 4], Commit)> {
    let count = prop_oneof![1 => Just(0usize), 12 => Just(1), 4 => Just(2), 1 => 3usize..5];
    (
        count,
        any::<[Index; 4]>(),
        0..MESSAGES.len(),
        0..AUTHORS.len(),
        0i64..4_000_000_000,
        0..OFFSETS.len(),
        0i64..4_000_000_000,
    )
        .prop_map(
            |(count, parents, message, author, time, offset, committed)| {
                let commit = Commit {
                    parents: Vec::new(),
                    message,
                    author,
                    time,
                    offset,
                    committed,
                };
                (count, parents, commit)
            },
        )
}

fn reference() -> impl Strategy<Value = Ref> {
    let kind = prop_oneof![
        3 => Just(Kind::Branch),
        2 => Just(Kind::Remote),
        2 => Just(Kind::Tag),
        2 => Just(Kind::Annotated),
        1 => Just(Kind::Nested),
    ];
    let object = prop_oneof![
        6 => Just(Object::Commit),
        1 => Just(Object::Tree),
        1 => Just(Object::Blob),
    ];
    (kind, object, any::<Index>()).prop_map(|(kind, object, commit)| Ref {
        kind,
        object,
        commit,
    })
}

fn stash() -> impl Strategy<Value = Stash> {
    let branch = prop_oneof![
        4 => Just(StashBranch::None),
        1 => Just(StashBranch::Stash),
        1 => Just(StashBranch::Index),
    ];
    (any::<Index>(), any::<bool>(), branch).prop_map(|(base, untracked, branch)| Stash {
        base,
        untracked,
        branch,
    })
}

fn case() -> impl Strategy<Value = Case> {
    let head = prop_oneof![
        4 => any::<Index>().prop_map(Head::Branch),
        1 => Just(Head::Unborn),
        2 => any::<Index>().prop_map(Head::Detached),
    ];
    let graph = prop_oneof![
        Just(CommitGraph::None),
        Just(CommitGraph::Full),
        Just(CommitGraph::Partial),
    ];
    (
        prop::collection::vec(commit(), 1..30),
        prop::bool::weighted(0.1),
        prop::collection::vec(reference(), 0..8),
        head,
        any::<bool>(),
        graph,
        prop::collection::vec(stash(), 0..4),
        settings(),
    )
        .prop_map(
            |(raw, long, refs, head, remote_head, graph, stashes, settings)| {
                let mut commits = Vec::new();
                for (k, (count, parents, mut commit)) in raw.into_iter().enumerate() {
                    if k > 0 {
                        for p in &parents[..count] {
                            let p = p.index(k);
                            if !commit.parents.contains(&p) {
                                commit.parents.push(p);
                            }
                        }
                    }
                    commits.push(commit);
                }
                // A long line of commits, so that the walk sends more than one
                // batch.
                if long {
                    for _ in 0..250 {
                        let k = commits.len();
                        commits.push(Commit {
                            parents: vec![k - 1],
                            message: k % MESSAGES.len(),
                            author: k % AUTHORS.len(),
                            time: k as i64,
                            offset: k % OFFSETS.len(),
                            committed: k as i64,
                        });
                    }
                }
                Case {
                    commits,
                    refs,
                    head,
                    remote_head,
                    graph,
                    stashes,
                    settings,
                }
            },
        )
}

fn git_in(s: &Scratch, args: &[&str], stdin: &[u8]) -> String {
    let mut child = Command::new("git")
        .current_dir(s.path())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run git");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin).expect("write");
    drop(input);
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

/// Commit `commits[start..]` with `git fast-import`. Commit `k` gets the
/// mark `k + 1`, and the marks of earlier runs come from `marks`.
fn import(s: &Scratch, commits: &[Commit], start: usize) {
    let mut out = Vec::new();
    for (i, c) in commits.iter().enumerate() {
        let k = start + i;
        let message = MESSAGES[c.message].0;
        // Each commit gets its own tree, so no two commits are the same.
        let content = format!("{k}\n");
        out.extend(b"reset refs/heads/import\ncommit refs/heads/import\n");
        out.extend(format!("mark :{}\n", k + 1).as_bytes());
        let (author, offset) = (AUTHORS[c.author].0, OFFSETS[c.offset].0);
        out.extend(format!("author {author} {} {offset}\n", c.time).as_bytes());
        out.extend(format!("committer C <c@example.com> {} +0000\n", c.committed).as_bytes());
        out.extend(format!("data {}\n", message.len()).as_bytes());
        out.extend(message);
        out.push(b'\n');
        for (n, p) in c.parents.iter().enumerate() {
            let verb = if n == 0 { "from" } else { "merge" };
            out.extend(format!("{verb} :{}\n", p + 1).as_bytes());
        }
        out.extend(format!("M 644 inline f\ndata {}\n{content}\n", content.len()).as_bytes());
    }
    // Each commit resets the branch, so its tip moves around freely.
    let mut args = vec![
        "fast-import",
        "--quiet",
        "--force",
        "--export-marks=.git/marks",
    ];
    if start > 0 {
        args.push("--import-marks=.git/marks");
    }
    git_in(s, &args, &out);
}

/// The ids of the commits, from the marks file.
fn ids(s: &Scratch, n: usize) -> Vec<String> {
    let marks = std::fs::read_to_string(s.file(".git/marks")).expect("read marks");
    let mut ids = vec![String::new(); n];
    for line in marks.lines() {
        let (mark, id) = line.split_once(' ').expect("mark");
        let k: usize = mark.trim_start_matches(':').parse().expect("mark number");
        ids[k - 1] = id.to_string();
    }
    ids
}

fn write_commit_graph(s: &Scratch, ids: &[String]) {
    let input = ids.join("\n") + "\n";
    git_in(
        s,
        &["commit-graph", "write", "--stdin-commits"],
        input.as_bytes(),
    );
}

fn build(s: &Scratch, case: &Case) -> Vec<String> {
    let n = case.commits.len();
    let split = if case.graph == CommitGraph::Partial {
        n / 2
    } else {
        n
    };
    import(s, &case.commits[..split], 0);
    if case.graph == CommitGraph::Partial && split > 0 {
        write_commit_graph(s, &ids(s, split));
    }
    if split < n {
        import(s, &case.commits[split..], split);
    }
    let ids = ids(s, n);
    if case.graph == CommitGraph::Full {
        write_commit_graph(s, &ids);
    }
    s.sh(&["update-ref", "-d", "refs/heads/import"]);

    let mut branches = Vec::new();
    let mut remotes = Vec::new();
    for (i, r) in case.refs.iter().enumerate() {
        let commit = &ids[r.commit.index(n)];
        let object = match r.object {
            Object::Commit => commit.clone(),
            Object::Tree => format!("{commit}^{{tree}}"),
            Object::Blob => format!("{commit}:f"),
        };
        match r.kind {
            Kind::Branch => {
                let name = format!("refs/heads/b{i}");
                s.sh(&["update-ref", &name, commit]);
                branches.push(name);
            }
            Kind::Remote => {
                let remote = if i % 2 == 0 { "origin" } else { "up" };
                let name = format!("refs/remotes/{remote}/r{i}");
                s.sh(&["update-ref", &name, commit]);
                remotes.push(name);
            }
            Kind::Tag => {
                let id = s.sh(&["rev-parse", &object]);
                s.sh(&["update-ref", &format!("refs/tags/t{i}"), id.trim()]);
            }
            Kind::Annotated => {
                s.sh(&["tag", "-a", "-m", "annotated", &format!("t{i}"), &object]);
            }
            Kind::Nested => {
                let inner = format!("t{i}-inner");
                s.sh(&["tag", "-a", "-m", "inner", &inner, &object]);
                s.sh(&["tag", "-a", "-m", "outer", &format!("t{i}"), &inner]);
                s.sh(&["tag", "-d", &inner]);
            }
        }
    }
    if case.remote_head
        && let Some(first) = remotes
            .iter()
            .find(|r| r.starts_with("refs/remotes/origin/"))
    {
        s.sh(&["symbolic-ref", "refs/remotes/origin/HEAD", first]);
    }
    match &case.head {
        Head::Branch(b) if !branches.is_empty() => {
            s.sh(&["symbolic-ref", "HEAD", &branches[b.index(branches.len())]]);
        }
        Head::Branch(_) | Head::Unborn => {
            s.sh(&["symbolic-ref", "HEAD", "refs/heads/unborn"]);
        }
        Head::Detached(k) => {
            s.sh(&["update-ref", "--no-deref", "HEAD", &ids[k.index(n)]]);
        }
    }
    for (k, st) in case.stashes.iter().enumerate() {
        make_stash(s, k, st, &ids[st.base.index(n)]);
    }
    ids
}

/// Make a stash on `base` the way git lays one out, and add it to the
/// list. Its commits hold the same tree as `base`, which doesn't matter
/// to the walk.
fn make_stash(s: &Scratch, k: usize, st: &Stash, base: &str) {
    let tree = format!("{base}^{{tree}}");
    let commit = |parents: &[&str], message: &str| {
        let mut args = vec!["commit-tree", &tree, "-m", message];
        for p in parents {
            args.extend(["-p", p]);
        }
        s.sh(&args).trim().to_string()
    };
    let index = commit(&[base], &format!("index on {k}"));
    let mut parents = vec![base, &index];
    let untracked;
    if st.untracked {
        untracked = commit(&[], &format!("untracked files on {k}"));
        parents.push(&untracked);
    }
    let stash = commit(&parents, &format!("WIP on {k}"));
    s.sh(&["stash", "store", "-m", &format!("stash {k}"), &stash]);
    match st.branch {
        StashBranch::None => {}
        StashBranch::Stash => {
            s.sh(&["update-ref", &format!("refs/heads/s{k}"), &stash]);
        }
        StashBranch::Index => {
            s.sh(&["update-ref", &format!("refs/heads/i{k}"), &index]);
        }
    }
}

/// Each commit that `git rev-list` lists from `args`, with its parents.
fn rev_list(s: &Scratch, args: &[&str]) -> HashMap<ObjectId, Vec<ObjectId>> {
    let mut all = vec!["rev-list", "--parents"];
    all.extend(args);
    s.sh(&all)
        .lines()
        .map(|line| {
            let mut ids = line.split(' ').map(id);
            (ids.next().expect("commit"), ids.collect())
        })
        .collect()
}

fn id(s: &str) -> ObjectId {
    ObjectId::from_hex(s.as_bytes()).expect("id")
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = Scratch::with(&case.settings);
    let ids = build(&s, case);
    let model: HashMap<ObjectId, &Commit> = ids
        .iter()
        .zip(&case.commits)
        .map(|(i, c)| (id(i), c))
        .collect();

    let rt = common::runtime();
    let refs = rt.block_on(Refs::load(&s.git)).expect("load refs");

    // Labels go on the commit a tag points at, so tags must be peeled all
    // the way, through tags of tags.
    for tag in &refs.tags {
        let peeled = s.sh(&["rev-parse", &format!("refs/tags/{}^{{}}", tag.name)]);
        prop_assert_eq!(tag.target, id(peeled.trim()), "tag {}", tag.name);
    }

    let walks = Generation::default();
    let generation = walks.next();
    let mut batches = Vec::new();
    rt.block_on(log::walk(
        &s.git,
        &refs.tips(&Default::default()),
        &refs.stash_tips(&Default::default()),
        generation,
        &walks,
        |b| batches.push(b),
    ))
    .expect("walk");

    let last = batches.len().checked_sub(1);
    prop_assert!(last.is_some(), "no batches");
    let mut commits = Vec::new();
    for (i, b) in batches.into_iter().enumerate() {
        prop_assert_eq!(b.generation, generation);
        prop_assert_eq!(b.done, Some(i) == last);
        prop_assert_eq!(b.rows.len(), b.commits.len());
        commits.extend(b.commits);
    }

    // Everything the refs lead to, and each stash with only its first
    // parent, which brings in that parent's history.
    prop_assert_eq!(refs.stashes.len(), case.stashes.len());
    let mut want = rev_list(&s, &["--exclude=refs/stash", "--all"]);
    for st in &refs.stashes {
        if want.contains_key(&st.id) {
            continue;
        }
        let base = s.sh(&["rev-parse", &format!("{}^1", st.id)]);
        want.insert(st.id, vec![id(base.trim())]);
        want.extend(rev_list(&s, &[base.trim()]));
    }

    let mut position = HashMap::new();
    for (i, c) in commits.iter().enumerate() {
        prop_assert!(position.insert(c.id, i).is_none(), "{} twice", c.id);
    }
    prop_assert_eq!(commits.len(), want.len());
    for c in &commits {
        let parents = want.get(&c.id);
        prop_assert!(parents.is_some(), "{} isn't reachable", c.id);
        prop_assert_eq!(Some(&c.parents), parents);
        for p in &c.parents {
            prop_assert!(position[p] > position[&c.id], "{} before its child", p);
        }
        // Stashes and their commits aren't in the model.
        let Some(m) = model.get(&c.id) else {
            continue;
        };
        prop_assert_eq!(&c.summary, MESSAGES[m.message].1);
        prop_assert_eq!(&c.author, AUTHORS[m.author].1);
        prop_assert_eq!(&c.email, AUTHORS[m.author].2);
        prop_assert_eq!(c.time, m.time);
        prop_assert_eq!(c.offset, OFFSETS[m.offset].1);
    }
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

common::split_proptest! {
    config = config(256);
    parts = [part_1 part_2 part_3 part_4];

    fn walk_matches_rev_list(case in case()) {
        check(&case)?;
    }
}
