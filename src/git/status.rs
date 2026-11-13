//! Parsing `git status --porcelain=v2 --branch -z`.

use anyhow::Result;

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
        path: String,
        orig_path: Option<String>,
        staged: Change,
        unstaged: Change,
        submodule: bool,
    },
    Unmerged {
        path: String,
        /// The two-letter code, such as "UU" or "AA".
        code: String,
    },
    Untracked {
        path: String,
    },
}

impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Entry::Changed { path, .. } | Entry::Unmerged { path, .. } | Entry::Untracked { path } => {
                path
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head_oid: Option<String>,
    /// The branch name, or `None` when HEAD is detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<Entry>,
    /// Paths git ignores. Folders end in a slash and stand for everything in
    /// them. The file watcher uses these.
    pub ignored: Vec<String>,
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
        Ok(parse(&out.text()))
    }

    pub fn staged(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| {
            matches!(e, Entry::Changed { staged, .. } if *staged != Change::Unmodified)
        })
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| {
            matches!(e, Entry::Changed { unstaged, .. } if *unstaged != Change::Unmodified)
        })
    }

    pub fn untracked(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| matches!(e, Entry::Untracked { .. }))
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| matches!(e, Entry::Unmerged { .. }))
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn has_staged(&self) -> bool {
        self.staged().next().is_some()
    }
}

pub fn parse(text: &str) -> Status {
    let mut st = Status::default();
    let mut fields = text.split('\0');
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                st.head_oid = (v != "(initial)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                st.branch = (v != "(detached)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                let mut it = v.split(' ');
                st.ahead = it.next().and_then(|s| s.trim_start_matches('+').parse().ok()).unwrap_or(0);
                st.behind = it.next().and_then(|s| s.trim_start_matches('-').parse().ok()).unwrap_or(0);
            }
            continue;
        }
        let kind = rec.as_bytes()[0];
        match kind {
            b'1' | b'2' => {
                // 1 XY sub mH mI mW hH hI path
                // 2 XY sub mH mI mW hH hI Xscore path\0origPath
                let n = if kind == b'1' { 9 } else { 10 };
                let parts: Vec<&str> = rec.splitn(n, ' ').collect();
                if parts.len() < n {
                    continue;
                }
                let xy = parts[1].as_bytes();
                let orig_path = if kind == b'2' {
                    fields.next().map(str::to_string)
                } else {
                    None
                };
                st.entries.push(Entry::Changed {
                    path: parts[n - 1].to_string(),
                    orig_path,
                    staged: Change::from_byte(xy[0]),
                    unstaged: Change::from_byte(xy[1]),
                    submodule: parts[2].starts_with('S'),
                });
            }
            b'u' => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let parts: Vec<&str> = rec.splitn(11, ' ').collect();
                if parts.len() < 11 {
                    continue;
                }
                st.entries.push(Entry::Unmerged {
                    path: parts[10].to_string(),
                    code: parts[1].to_string(),
                });
            }
            b'?' => st.entries.push(Entry::Untracked {
                path: rec[2..].to_string(),
            }),
            b'!' => st.ignored.push(rec[2..].to_string()),
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
        let text = "# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\0\
1 A. N... 000000 100644 100644 000 bbb new.rs\0\
2 R. N... 100644 100644 100644 ccc ccc R100 to.rs\0from.rs\0\
u UU N... 100644 100644 100644 100644 d1 d2 d3 conflict.rs\0\
? untracked file\0! target/\0";
        let st = parse(text);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.entries.len(), 5);
        assert_eq!(st.entries[0].path(), "src/a b.rs");
        assert_eq!(st.unstaged().count(), 1);
        assert_eq!(st.staged().count(), 2);
        assert!(matches!(&st.entries[2], Entry::Changed { orig_path: Some(p), .. } if p == "from.rs"));
        assert_eq!(st.conflicted().count(), 1);
        assert_eq!(st.untracked().next().map(Entry::path), Some("untracked file"));
        assert_eq!(st.ignored, ["target/"]);
    }

    #[test]
    fn detached_and_initial() {
        let st = parse("# branch.oid (initial)\0# branch.head (detached)\0");
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
    fn render(st: &Status) -> String {
        let mut out = String::new();
        let oid = st.head_oid.as_deref().unwrap_or("(initial)");
        out.push_str(&format!("# branch.oid {oid}\0"));
        out.push_str(&format!("# branch.head {}\0", st.branch.as_deref().unwrap_or("(detached)")));
        if let Some(u) = &st.upstream {
            out.push_str(&format!("# branch.upstream {u}\0# branch.ab +{} -{}\0", st.ahead, st.behind));
        }
        for e in &st.entries {
            match e {
                Entry::Changed { path, orig_path, staged, unstaged, submodule } => {
                    let xy = format!("{}{}", letter(*staged), letter(*unstaged));
                    let sub = if *submodule { "SC.." } else { "N..." };
                    let ids = "100644 100644 100644 1111111 2222222";
                    match orig_path {
                        Some(o) => out.push_str(&format!("2 {xy} {sub} {ids} R100 {path}\0{o}\0")),
                        None => out.push_str(&format!("1 {xy} {sub} {ids} {path}\0")),
                    }
                }
                Entry::Unmerged { path, code } => {
                    out.push_str(&format!("u {code} N... 100644 100644 100644 100644 1 2 3 {path}\0"));
                }
                Entry::Untracked { path } => out.push_str(&format!("? {path}\0")),
            }
        }
        for p in &st.ignored {
            out.push_str(&format!("! {p}\0"));
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

    /// With `-z`, git doesn't quote paths, so they can hold spaces and
    /// newlines.
    const PATH: &str = "[a-z \n\té/]{1,12}";

    fn entry() -> impl Strategy<Value = Entry> {
        prop_oneof![
            (PATH, prop::option::of(PATH), change(), change(), any::<bool>()).prop_map(
                |(path, orig_path, staged, unstaged, submodule)| Entry::Changed {
                    path,
                    orig_path,
                    staged,
                    unstaged,
                    submodule,
                }
            ),
            (PATH, prop::sample::select(vec!["UU", "AA", "DD", "AU", "UD"]))
                .prop_map(|(path, code)| Entry::Unmerged { path, code: code.to_string() }),
            PATH.prop_map(|path| Entry::Untracked { path }),
        ]
    }

    fn status() -> impl Strategy<Value = Status> {
        let branch = "[a-z][a-z/._-]{0,10}";
        (
            prop::option::of("[0-9a-f]{40}"),
            prop::option::of(branch),
            prop::option::of((branch, any::<u32>(), any::<u32>())),
            prop::collection::vec(entry(), 0..8),
            prop::collection::vec(PATH, 0..3),
        )
            .prop_map(|(head_oid, branch, upstream, entries, ignored)| {
                let (upstream, ahead, behind) = match upstream {
                    Some((u, a, b)) => (Some(u), a, b),
                    None => (None, 0, 0),
                };
                Status { head_oid, branch, upstream, ahead, behind, entries, ignored }
            })
    }

    proptest! {
        #[test]
        fn parse_reads_back_what_git_prints(st in status()) {
            prop_assert_eq!(parse(&render(&st)), st);
        }
    }
}
