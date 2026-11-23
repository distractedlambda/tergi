//! Parsing `git status --porcelain=v2 --branch -z`.

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice as _};

use super::cli::Git;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Unmodified,
    Modified,
    TypeChange,
    Added,
    Deleted,
    Renamed,
    Copied,
}

impl Change {
    fn from_byte(b: u8) -> Self {
        match b {
            b'M' => Change::Modified,
            b'T' => Change::TypeChange,
            b'A' => Change::Added,
            b'D' => Change::Deleted,
            b'R' => Change::Renamed,
            b'C' => Change::Copied,
            _ => Change::Unmodified,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Changed {
        path: BString,
        orig_path: Option<BString>,
        staged: Change,
        unstaged: Change,
        submodule: bool,
    },
    Unmerged {
        path: BString,
        /// The two-letter code, such as "UU" or "AA".
        code: String,
    },
    Untracked {
        path: BString,
    },
}

impl Entry {
    pub fn path(&self) -> &BStr {
        match self {
            Entry::Changed { path, .. }
            | Entry::Unmerged { path, .. }
            | Entry::Untracked { path } => path.as_bstr(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head_oid: Option<String>,
    /// The branch name, or `None` when HEAD is detached. Like paths,
    /// branch names needn't be UTF-8.
    pub branch: Option<BString>,
    pub upstream: Option<BString>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<Entry>,
    /// Paths git ignores. Folders end in a slash and stand for everything in
    /// them. The file watcher uses these.
    pub ignored: Vec<BString>,
}

impl Status {
    /// The arguments to `git` that `load` uses. With `--ignored=matching`,
    /// git lists an ignored folder once instead of every file in it.
    pub const ARGS: &[&str] = &[
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=dirty",
        "--ignored=matching",
    ];

    pub async fn load(git: &Git) -> Result<Status> {
        let out = git.read(Self::ARGS).await?;
        Ok(parse(&out.stdout))
    }

    pub fn staged(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Changed { staged, .. } if *staged != Change::Unmodified))
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(
            |e| matches!(e, Entry::Changed { unstaged, .. } if *unstaged != Change::Unmodified),
        )
    }

    pub fn untracked(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Untracked { .. }))
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Unmerged { .. }))
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn has_staged(&self) -> bool {
        self.staged().next().is_some()
    }
}

/// Parse the output of `git status`. Paths are kept as bytes, because git
/// prints them as they are, and they needn't be UTF-8.
pub fn parse(out: &[u8]) -> Status {
    let mut st = Status::default();
    let mut fields = out.split(|&b| b == 0);
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix(b"# ") {
            if let Some(v) = h.strip_prefix(b"branch.head ") {
                st.branch = (v != b"(detached)").then(|| v.into());
                continue;
            } else if let Some(v) = h.strip_prefix(b"branch.upstream ") {
                st.upstream = Some(v.into());
                continue;
            }
            let h = h.to_str_lossy();
            if let Some(v) = h.strip_prefix("branch.oid ") {
                st.head_oid = (v != "(initial)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                let mut it = v.split(' ');
                st.ahead = it
                    .next()
                    .and_then(|s| s.trim_start_matches('+').parse().ok())
                    .unwrap_or(0);
                st.behind = it
                    .next()
                    .and_then(|s| s.trim_start_matches('-').parse().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        let kind = rec[0];
        match kind {
            b'1' | b'2' => {
                // 1 XY sub mH mI mW hH hI path
                // 2 XY sub mH mI mW hH hI Xscore path\0origPath
                let n = if kind == b'1' { 9 } else { 10 };
                let parts: Vec<&[u8]> = rec.splitn(n, |&b| b == b' ').collect();
                if parts.len() < n || parts[1].len() < 2 {
                    continue;
                }
                let xy = parts[1];
                let orig_path = if kind == b'2' {
                    fields.next().map(BString::from)
                } else {
                    None
                };
                st.entries.push(Entry::Changed {
                    path: parts[n - 1].into(),
                    orig_path,
                    staged: Change::from_byte(xy[0]),
                    unstaged: Change::from_byte(xy[1]),
                    submodule: parts[2].starts_with(b"S"),
                });
            }
            b'u' => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let parts: Vec<&[u8]> = rec.splitn(11, |&b| b == b' ').collect();
                if parts.len() < 11 {
                    continue;
                }
                st.entries.push(Entry::Unmerged {
                    path: parts[10].into(),
                    code: parts[1].to_str_lossy().into_owned(),
                });
            }
            b'?' => st.entries.push(Entry::Untracked {
                path: rec.get(2..).unwrap_or_default().into(),
            }),
            b'!' => st.ignored.push(rec.get(2..).unwrap_or_default().into()),
            _ => {}
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses() {
        let text = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\0\
1 A. N... 000000 100644 100644 000 bbb new.rs\0\
2 R. N... 100644 100644 100644 ccc ccc R100 to.rs\0from.rs\0\
u UU N... 100644 100644 100644 100644 d1 d2 d3 conflict.rs\0\
? untracked file\0! target/\0? caf\xe9\0";
        let st = parse(text);
        assert_eq!(st.branch, Some("main".into()));
        assert_eq!(st.upstream, Some("origin/main".into()));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.entries.len(), 6);
        assert_eq!(st.entries[0].path(), "src/a b.rs");
        assert_eq!(st.unstaged().count(), 1);
        assert_eq!(st.staged().count(), 2);
        assert!(
            matches!(&st.entries[2], Entry::Changed { orig_path: Some(p), .. } if p == "from.rs")
        );
        assert_eq!(st.entries[5].path(), b"caf\xe9".as_bstr());
        assert_eq!(st.conflicted().count(), 1);
        assert_eq!(
            st.untracked().next().map(Entry::path),
            Some(b"untracked file".as_bstr())
        );
        assert_eq!(st.ignored, [BString::from("target/")]);
    }

    #[test]
    fn detached_and_initial() {
        let st = parse(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(st.head_oid, None);
        assert_eq!(st.branch, None);
    }

    fn letter(c: Change) -> char {
        match c {
            Change::Unmodified => '.',
            Change::Modified => 'M',
            Change::TypeChange => 'T',
            Change::Added => 'A',
            Change::Deleted => 'D',
            Change::Renamed => 'R',
            Change::Copied => 'C',
        }
    }

    /// Print a status the way `git status --porcelain=v2 --branch -z` does.
    fn render(st: &Status) -> Vec<u8> {
        use std::io::Write as _;
        let mut out = Vec::new();
        let oid = st.head_oid.as_deref().unwrap_or("(initial)");
        let head = st.branch.as_ref().map_or(&b"(detached)"[..], |b| b);
        write!(out, "# branch.oid {oid}\0").expect("write");
        out.extend_from_slice(&[b"# branch.head ", head, b"\0"].concat());
        if let Some(u) = &st.upstream {
            let (a, b) = (st.ahead, st.behind);
            out.extend_from_slice(&[b"# branch.upstream ", u.as_slice(), b"\0"].concat());
            write!(out, "# branch.ab +{a} -{b}\0").expect("write");
        }
        for e in &st.entries {
            match e {
                Entry::Changed {
                    orig_path,
                    staged,
                    unstaged,
                    submodule,
                    ..
                } => {
                    let xy = format!("{}{}", letter(*staged), letter(*unstaged));
                    let sub = if *submodule { "SC.." } else { "N..." };
                    let ids = "100644 100644 100644 1111111 2222222";
                    match orig_path {
                        Some(_) => write!(out, "2 {xy} {sub} {ids} R100 "),
                        None => write!(out, "1 {xy} {sub} {ids} "),
                    }
                }
                Entry::Unmerged { code, .. } => {
                    write!(out, "u {code} N... 100644 100644 100644 100644 1 2 3 ")
                }
                Entry::Untracked { .. } => write!(out, "? "),
            }
            .expect("write");
            out.extend_from_slice(e.path());
            out.push(0);
            if let Entry::Changed {
                orig_path: Some(o), ..
            } = e
            {
                out.extend_from_slice(o);
                out.push(0);
            }
        }
        for p in &st.ignored {
            out.extend_from_slice(b"! ");
            out.extend_from_slice(p);
            out.push(0);
        }
        out
    }

    fn change() -> impl Strategy<Value = Change> {
        prop::sample::select(vec![
            Change::Unmodified,
            Change::Modified,
            Change::TypeChange,
            Change::Added,
            Change::Deleted,
            Change::Renamed,
            Change::Copied,
        ])
    }

    /// With `-z`, git doesn't quote paths, so they can hold spaces,
    /// newlines and bytes that aren't UTF-8.
    fn path() -> impl Strategy<Value = BString> {
        let piece = prop::sample::select(vec![
            &b"a"[..],
            b"z",
            b" ",
            b"\n",
            b"\t",
            b"/",
            "é".as_bytes(),
            b"\xe9",
            b"\xff",
            b"\x80",
        ]);
        prop::collection::vec(piece, 1..12).prop_map(|v| BString::from(v.concat()))
    }

    fn entry() -> impl Strategy<Value = Entry> {
        prop_oneof![
            (
                path(),
                prop::option::of(path()),
                change(),
                change(),
                any::<bool>()
            )
                .prop_map(|(path, orig_path, staged, unstaged, submodule)| {
                    Entry::Changed {
                        path,
                        orig_path,
                        staged,
                        unstaged,
                        submodule,
                    }
                }),
            (
                path(),
                prop::sample::select(vec!["UU", "AA", "DD", "AU", "UD"])
            )
                .prop_map(|(path, code)| Entry::Unmerged {
                    path,
                    code: code.to_string()
                }),
            path().prop_map(|path| Entry::Untracked { path }),
        ]
    }

    /// Branch names can't hold spaces or control characters, but they
    /// can hold bytes that aren't UTF-8.
    fn branch() -> impl Strategy<Value = BString> {
        let piece = prop::sample::select(vec![
            &b"a"[..],
            b"z",
            b"/",
            b".",
            b"-",
            "é".as_bytes(),
            b"\xe9",
            b"\xff",
        ]);
        prop::collection::vec(piece, 0..10)
            .prop_map(|v| BString::from([&b"b"[..], &v.concat()].concat()))
    }

    fn status() -> impl Strategy<Value = Status> {
        (
            prop::option::of("[0-9a-f]{40}"),
            prop::option::of(branch()),
            prop::option::of((branch(), any::<u32>(), any::<u32>())),
            prop::collection::vec(entry(), 0..8),
            prop::collection::vec(path(), 0..3),
        )
            .prop_map(|(head_oid, branch, upstream, entries, ignored)| {
                let (upstream, ahead, behind) = match upstream {
                    Some((u, a, b)) => (Some(u), a, b),
                    None => (None, 0, 0),
                };
                Status {
                    head_oid,
                    branch,
                    upstream,
                    ahead,
                    behind,
                    entries,
                    ignored,
                }
            })
    }

    proptest! {
        #[test]
        fn parse_reads_back_what_git_prints(st in status()) {
            prop_assert_eq!(parse(&render(&st)), st);
        }
    }
}
