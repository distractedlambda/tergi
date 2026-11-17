//! The diff model shared by commit diffs and working tree diffs, plus a parser
//! for the unified diffs that `git diff` prints.

use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line for display. If the file isn't valid UTF-8, the bad bytes
    /// are replaced here and the originals are kept in `raw`.
    pub text: String,
    /// The line's bytes, only when they differ from `text`. A patch has to
    /// use these, or it won't match the file.
    pub raw: Option<Box<[u8]>>,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// The line is the last one in its file and has no trailing newline.
    pub no_eol: bool,
}

impl DiffLine {
    pub fn new(kind: LineKind, bytes: &[u8], old_no: Option<u32>, new_no: Option<u32>) -> Self {
        let (text, raw) = match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), None),
            Err(_) => (
                String::from_utf8_lossy(bytes).into_owned(),
                Some(bytes.into()),
            ),
        };
        DiffLine {
            kind,
            text,
            raw,
            old_no,
            new_no,
            no_eol: false,
        }
    }

    /// The line as it is in the file.
    pub fn bytes(&self) -> &[u8] {
        self.raw.as_deref().unwrap_or(self.text.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// The text after the second `@@`, usually the enclosing function.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    pub fn header(&self) -> String {
        let mut s = format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        );
        if !self.section.is_empty() {
            s.push(' ');
            s.push_str(&self.section);
        }
        s
    }

    pub fn changed_lines(&self) -> impl Iterator<Item = usize> + '_ {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind != LineKind::Context)
            .map(|(i, _)| i)
    }
}

fn range(start: u32, len: u32) -> String {
    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChange,
    Untracked,
    Conflicted,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Modified => 'M',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::TypeChange => 'T',
            FileStatus::Untracked => '?',
            FileStatus::Conflicted => 'U',
        }
    }
}

/// A file in a diff. Paths are bytes, because git stores file names as
/// they are, and they needn't be UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// `None` when the file didn't exist before.
    pub old_path: Option<BString>,
    /// `None` when the file was deleted.
    pub new_path: Option<BString>,
    pub status: FileStatus,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub old_id: Option<ObjectId>,
    pub new_id: Option<ObjectId>,
    pub binary: bool,
    /// The diff was too big to show, so `hunks` is empty.
    pub too_large: bool,
    pub similarity: Option<u8>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn new(status: FileStatus, old_path: Option<BString>, new_path: Option<BString>) -> Self {
        Self {
            old_path,
            new_path,
            status,
            old_mode: None,
            new_mode: None,
            old_id: None,
            new_id: None,
            binary: false,
            too_large: false,
            similarity: None,
            hunks: Vec::new(),
        }
    }

    /// The path that names this file.
    pub fn path(&self) -> &BStr {
        self.new_path
            .as_ref()
            .or(self.old_path.as_ref())
            .map_or(b"".as_bstr(), |p| p.as_bstr())
    }

    /// The path to show for this file. Bytes that aren't UTF-8 show up as
    /// replacement characters.
    pub fn display_path(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(o), Some(n)) if o != n => format!("{o} → {n}"),
            _ => self.path().to_string(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }

    pub fn added_removed(&self) -> (usize, usize) {
        let mut a = 0;
        let mut r = 0;
        for l in self.hunks.iter().flat_map(|h| &h.lines) {
            match l.kind {
                LineKind::Add => a += 1,
                LineKind::Del => r += 1,
                LineKind::Context => {}
            }
        }
        (a, r)
    }

    pub fn is_new(&self) -> bool {
        self.old_path.is_none()
    }

    pub fn is_deleted(&self) -> bool {
        self.new_path.is_none()
    }
}

/// Files with more diff lines than this are marked `too_large` and their
/// hunks are dropped, unless the caller asks for everything.
pub const DEFAULT_LINE_LIMIT: usize = 20_000;

/// Parse the output of `git diff` (without color, with `--no-ext-diff`).
///
/// The output is bytes, because git prints file contents as they are, and
/// they need not be UTF-8.
pub fn parse_unified(text: impl AsRef<[u8]>, line_limit: usize) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let lines = text.as_ref().split(|&b| b == b'\n');
    // Track where we are in the current hunk so we know when it ends. A hunk
    // body line can start with "diff --git" if a file contains that text, so
    // we rely on the counts in the hunk header instead of guessing.
    let mut old_left = 0u32;
    let mut new_left = 0u32;
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut file_lines = 0usize;
    // Inside a combined diff, which we don't show. Its lines start with one
    // column per parent, so a removed "- item" reads "--- item" and must not
    // be taken for a file header.
    let mut combined = false;

    for line in lines {
        if old_left > 0 || new_left > 0 {
            let Some(file) = files.last_mut() else { break };
            let (kind, body): (_, &[u8]) = match line.first() {
                Some(b'+') => (LineKind::Add, &line[1..]),
                Some(b'-') => (LineKind::Del, &line[1..]),
                Some(b' ') => (LineKind::Context, &line[1..]),
                // Some tools strip the space from empty context lines.
                None => (LineKind::Context, b""),
                Some(b'\\') => {
                    mark_no_eol(file);
                    continue;
                }
                _ => {
                    // Malformed. Stop the hunk and treat the line normally.
                    old_left = 0;
                    new_left = 0;
                    continue;
                }
            };
            let (o, n) = match kind {
                LineKind::Context => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    let nos = (Some(old_no), Some(new_no));
                    old_no = old_no.saturating_add(1);
                    new_no = new_no.saturating_add(1);
                    nos
                }
                LineKind::Del => {
                    old_left = old_left.saturating_sub(1);
                    let nos = (Some(old_no), None);
                    old_no = old_no.saturating_add(1);
                    nos
                }
                LineKind::Add => {
                    new_left = new_left.saturating_sub(1);
                    let nos = (None, Some(new_no));
                    new_no = new_no.saturating_add(1);
                    nos
                }
            };
            if !file.too_large
                && let Some(h) = file.hunks.last_mut()
            {
                h.lines.push(DiffLine::new(kind, body, o, n));
                file_lines += 1;
                if file_lines > line_limit {
                    file.too_large = true;
                    file.hunks.clear();
                }
            }
            continue;
        }

        // Headers hold paths, which are bytes. The rest of a header is
        // ASCII, apart from the section text after a hunk header.
        let text = |b: &[u8]| b.to_str_lossy().into_owned();
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            let (a, b) = split_git_paths(rest);
            // The paths are refined later by the ---/+++ and rename lines.
            files.push(FileDiff::new(FileStatus::Modified, a, b));
            file_lines = 0;
            combined = false;
        } else if let Some(rest) = line
            .strip_prefix(b"diff --cc ")
            .or(line.strip_prefix(b"diff --combined "))
        {
            let p = unquote(rest);
            let mut f = FileDiff::new(FileStatus::Conflicted, Some(p.clone()), Some(p));
            f.too_large = true;
            files.push(f);
            file_lines = 0;
            combined = true;
        } else if combined {
            continue;
        } else if let Some(file) = files.last_mut() {
            if let Some(rest) = line.strip_prefix(b"@@ ") {
                if let Some(h) = parse_hunk_header(&rest.to_str_lossy()) {
                    old_left = h.old_len;
                    new_left = h.new_len;
                    old_no = h.old_start;
                    new_no = h.new_start;
                    // For empty ranges git prints the line before the change.
                    if h.old_len == 0 {
                        old_no = old_no.saturating_add(1);
                    }
                    if h.new_len == 0 {
                        new_no = new_no.saturating_add(1);
                    }
                    if !file.too_large {
                        file.hunks.push(h);
                    }
                }
            } else if line.starts_with(b"\\ ") {
                mark_no_eol(file);
            } else if let Some(rest) = line.strip_prefix(b"--- ") {
                file.old_path = header_path(rest, b"a/");
            } else if let Some(rest) = line.strip_prefix(b"+++ ") {
                file.new_path = header_path(rest, b"b/");
            } else if let Some(m) = line.strip_prefix(b"new file mode ") {
                file.status = FileStatus::Added;
                file.old_path = None;
                file.new_mode = Some(text(m));
            } else if let Some(m) = line.strip_prefix(b"deleted file mode ") {
                file.status = FileStatus::Deleted;
                file.new_path = None;
                file.old_mode = Some(text(m));
            } else if let Some(m) = line.strip_prefix(b"old mode ") {
                file.old_mode = Some(text(m));
            } else if let Some(m) = line.strip_prefix(b"new mode ") {
                let m = text(m);
                if file.status == FileStatus::Modified
                    && file.old_mode.as_deref().map(mode_kind) != Some(mode_kind(&m))
                {
                    file.status = FileStatus::TypeChange;
                }
                file.new_mode = Some(m);
            } else if let Some(p) = line.strip_prefix(b"rename from ") {
                file.status = FileStatus::Renamed;
                file.old_path = Some(unquote(p));
            } else if let Some(p) = line.strip_prefix(b"rename to ") {
                file.new_path = Some(unquote(p));
            } else if let Some(p) = line.strip_prefix(b"copy from ") {
                file.status = FileStatus::Copied;
                file.old_path = Some(unquote(p));
            } else if let Some(p) = line.strip_prefix(b"copy to ") {
                file.new_path = Some(unquote(p));
            } else if let Some(s) = line
                .strip_prefix(b"similarity index ")
                .and_then(|s| s.strip_suffix(b"%"))
            {
                file.similarity = s.to_str().ok().and_then(|s| s.parse().ok());
            } else if let Some(rest) = line.strip_prefix(b"index ") {
                let mut fields = rest.split(|&b| b == b' ');
                let ids = fields.next().unwrap_or_default();
                if let Some((a, b)) = ids.split_once_str("..") {
                    file.old_id = ObjectId::from_hex(a).ok();
                    file.new_id = ObjectId::from_hex(b).ok();
                }
                if let Some(mode) = fields.next() {
                    file.old_mode.get_or_insert_with(|| text(mode));
                    file.new_mode.get_or_insert_with(|| text(mode));
                }
            } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
                file.binary = true;
            }
        }
    }
    files
}

fn mark_no_eol(file: &mut FileDiff) {
    if let Some(l) = file.hunks.last_mut().and_then(|h| h.lines.last_mut()) {
        l.no_eol = true;
    }
}

fn mode_kind(mode: &str) -> &str {
    // Regular files are 100644/100755. Symlinks, submodules and so on differ
    // in the first three digits.
    if mode.starts_with("100") { "100" } else { mode }
}

pub fn parse_hunk_header(rest: &str) -> Option<Hunk> {
    // rest looks like "-1,3 +1,4 @@ fn foo()"
    let (ranges, section) = match rest.split_once(" @@") {
        Some((r, s)) => (r, s.strip_prefix(' ').unwrap_or(s)),
        None => return None,
    };
    let mut parts = ranges.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = parse(old)?;
    let (new_start, new_len) = parse(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section: section.to_string(),
        lines: Vec::new(),
    })
}

fn header_path(rest: &[u8], prefix: &[u8]) -> Option<BString> {
    // Paths with spaces may be followed by a tab.
    let rest = rest.split(|&b| b == b'\t').next().unwrap_or(rest);
    if rest == b"/dev/null" {
        return None;
    }
    let p = unquote(rest);
    Some(p.strip_prefix(prefix).map_or(p.clone(), BString::from))
}

/// Split the `a/x b/y` part of a `diff --git` line. Unquoted paths with spaces
/// are ambiguous, so we assume both sides are the same length when they are
/// not renamed, which is what git does too.
fn split_git_paths(rest: &[u8]) -> (Option<BString>, Option<BString>) {
    let strip = |p: &[u8], prefix: &[u8]| BString::from(p.strip_prefix(prefix).unwrap_or(p));
    if rest.starts_with(b"\"") {
        let (a, tail) = take_quoted(rest);
        let b = unquote(tail.trim_ascii_start());
        return (Some(strip(&a, b"a/")), Some(strip(&b, b"b/")));
    }
    if let Some(idx) = rest.find(" \"b/") {
        let b = unquote(&rest[idx + 1..]);
        return (Some(strip(&rest[..idx], b"a/")), Some(strip(&b, b"b/")));
    }
    let n = rest.len();
    if n % 2 == 1 {
        let half = n / 2;
        let (a, b) = (&rest[..half], &rest[half + 1..]);
        if let (Some(a2), Some(b2)) = (a.strip_prefix(b"a/"), b.strip_prefix(b"b/"))
            && a2 == b2
        {
            return (Some(a2.into()), Some(b2.into()));
        }
    }
    match rest.split_once_str(" b/") {
        Some((a, b)) => (Some(strip(a, b"a/")), Some(b.into())),
        None => (Some(rest.into()), Some(rest.into())),
    }
}

fn take_quoted(s: &[u8]) -> (BString, &[u8]) {
    let mut i = 1;
    while i < s.len() {
        if s[i] == b'\\' {
            i += 2;
            continue;
        }
        if s[i] == b'"' {
            return (unquote(&s[..=i]), &s[i + 1..]);
        }
        i += 1;
    }
    (unquote(s), b"")
}

/// Undo git's C-style path quoting (used for unusual characters). The
/// result is bytes, since an escape can stand for any byte.
pub fn unquote(s: &[u8]) -> BString {
    let Some(b) = s.strip_prefix(b"\"").and_then(|s| s.strip_suffix(b"\"")) else {
        return s.into();
    };
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        i += 2;
        match c {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = u32::from(c - b'0');
                let mut k = 0;
                while k < 2 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    k += 1;
                }
                out.push((v & 0xff) as u8);
            }
            other => out.push(other),
        }
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::git::patch::quote_path;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@ fn main() {
 one
-two
+TWO
+three
 four
 five
@@ -10 +11,0 @@
-gone
diff --git a/new file.txt b/new file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new file.txt
@@ -0,0 +1,2 @@
+hello
+world
\\ No newline at end of file
diff --git a/old.txt b/renamed.txt
similarity index 90%
rename from old.txt
rename to renamed.txt
diff --git a/img.png b/img.png
index 4444444..5555555 100644
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_sample() {
        let files = parse_unified(SAMPLE, DEFAULT_LINE_LIMIT);
        assert_eq!(files.len(), 4);

        let a = &files[0];
        assert_eq!(a.path(), "src/a.rs");
        assert_eq!(a.status, FileStatus::Modified);
        assert_eq!(a.hunks.len(), 2);
        assert_eq!(a.hunks[0].section, "fn main() {");
        assert_eq!(a.hunks[0].lines.len(), 6);
        assert_eq!(a.hunks[0].lines[1].kind, LineKind::Del);
        assert_eq!(a.hunks[0].lines[1].old_no, Some(2));
        assert_eq!(a.hunks[0].lines[2].new_no, Some(2));
        assert_eq!(a.hunks[0].lines[4].old_no, Some(3));
        assert_eq!(a.hunks[0].lines[4].new_no, Some(4));
        assert_eq!(a.hunks[1].lines[0].old_no, Some(10));
        assert_eq!(a.added_removed(), (2, 2));

        let n = &files[1];
        assert_eq!(n.status, FileStatus::Added);
        assert_eq!(n.path(), "new file.txt");
        assert!(n.is_new());
        assert!(n.hunks[0].lines[1].no_eol);
        assert!(!n.hunks[0].lines[0].no_eol);

        let r = &files[2];
        assert_eq!(r.status, FileStatus::Renamed);
        assert_eq!(r.display_path(), "old.txt → renamed.txt");
        assert_eq!(r.similarity, Some(90));

        assert!(files[3].binary);
    }

    #[test]
    fn content_that_looks_like_a_header() {
        let text = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,2 +1,2 @@
-diff --git a/y b/y
+--- a/z
 @@ -1 +1 @@
";
        let files = parse_unified(text, DEFAULT_LINE_LIMIT);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 3);
        assert_eq!(files[0].hunks[0].lines[0].text, "diff --git a/y b/y");
    }

    #[test]
    fn combined_diff_content_is_skipped() {
        let text = "\
diff --cc list.md
index 1111111,2222222..0000000
--- a/list.md
+++ b/list.md
@@@ -1,3 -1,3 +1,2 @@@
  - kept
--- dropped by the merge
+++ added by the merge
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1 +1 @@
-old
+new
";
        let files = parse_unified(text, DEFAULT_LINE_LIMIT);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, FileStatus::Conflicted);
        assert_eq!(files[0].old_path, Some("list.md".into()));
        assert_eq!(files[0].new_path, Some("list.md".into()));
        assert_eq!(files[1].path(), "x");
        assert_eq!(files[1].hunks[0].lines.len(), 2);
    }

    #[test]
    fn line_limit() {
        let files = parse_unified(SAMPLE, 3);
        assert!(files[0].too_large);
        assert!(files[0].hunks.is_empty());
        assert!(!files[1].too_large);
    }

    #[test]
    fn quoted_paths() {
        assert_eq!(unquote(br#""a\tb\303\251""#), "a\tbé");
        assert_eq!(unquote(br#""caf\351""#), b"caf\xe9".as_bstr());
        let (a, b) = split_git_paths(br#""a/x y\t" "b/x y\t""#);
        assert_eq!(a, Some("x y\t".into()));
        assert_eq!(b, Some("x y\t".into()));
        let (a, b) = split_git_paths(b"a/a b b/a b");
        assert_eq!(a, Some("a b".into()));
        assert_eq!(b, Some("a b".into()));
    }

    #[test]
    fn paths_that_are_not_utf8() {
        // With `core.quotepath=false`, git prints bytes over 0x7f as they
        // are. Here 0xe9 is "é" in Latin-1.
        let diff = b"diff --git a/caf\xe9 b/caf\xe9\nnew file mode 100644\n\
--- /dev/null\n+++ b/caf\xe9\n@@ -0,0 +1 @@\n+x\n\
diff --git a/\xff b/\xfe\nsimilarity index 100%\nrename from \xff\nrename to \xfe\n";
        let files = parse_unified(diff, DEFAULT_LINE_LIMIT);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, Some(BString::from(&b"caf\xe9"[..])));
        assert_eq!(files[0].display_path(), "caf\u{fffd}");
        assert_eq!(files[1].old_path, Some(BString::from(&b"\xff"[..])));
        assert_eq!(files[1].new_path, Some(BString::from(&b"\xfe"[..])));
    }

    /// A file name made of pieces that git has to quote, and bytes that
    /// aren't UTF-8.
    fn path_component() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop::sample::select(vec![
            &b"a"[..],
            b"z",
            b" ",
            b"\"",
            b"\\",
            b"\t",
            "\u{e9}".as_bytes(),
            b"\x80",
            b"\xff",
        ]);
        prop::collection::vec(piece, 1..10).prop_map(|v| v.concat())
    }

    /// Text made of pieces of diff syntax, in any order, with some made-up
    /// lines mixed in.
    fn diffish() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            prop::sample::select(vec![
                "diff --git a/f b/f",
                "diff --git \"a/x\\ty\" \"b/x\\ty\"",
                "diff --cc f",
                "--- a/f",
                "+++ b/f",
                "--- /dev/null",
                "@@ -1,2 +1,3 @@ fn",
                "@@ -0,0 +1 @@",
                "@@ -4294967295,0 +4294967295,0 @@",
                "@@@ -1 -1 +1 @@@",
                "+x",
                "-x",
                " x",
                "",
                "\\ No newline at end of file",
                "new file mode 100644",
                "deleted file mode 100755",
                "old mode 100644",
                "new mode 120000",
                "rename from \"\\303\"",
                "rename to b",
                "similarity index 100%",
                "index 0000000..1111111 100644",
                "Binary files a/f and b/f differ",
            ])
            .prop_map(str::to_string),
            "[ -~é\t]{0,12}",
        ];
        prop::collection::vec(piece, 0..40).prop_map(|v| v.join("\n"))
    }

    proptest! {
        #[test]
        fn parsing_never_panics(text in diffish(), limit in 0usize..20) {
            for f in parse_unified(&text, limit) {
                if f.too_large {
                    prop_assert!(f.hunks.is_empty());
                }
            }
        }

        #[test]
        fn quoting_round_trips(path in any::<Vec<u8>>()) {
            prop_assert_eq!(unquote(&quote_path(&path)), BString::from(path));
        }

        #[test]
        fn git_paths_split(path in prop::collection::vec(path_component(), 1..4)) {
            let path = path.join(&b'/');
            let mut line = quote_path(&[b"a/", &path[..]].concat());
            line.push(b' ');
            line.extend(quote_path(&[b"b/", &path[..]].concat()));
            let (a, b) = split_git_paths(&line);
            prop_assert_eq!(a, Some(BString::from(path.clone())));
            prop_assert_eq!(b, Some(BString::from(path)));
        }
    }
}
