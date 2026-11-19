//! A lexer for gitignore files.
//!
//! It follows git's source. `add_patterns_from_buffer` and
//! `parse_path_pattern` in `dir.c` split the file into lines and find
//! comments and the `!` that negates a pattern. `dowild` in `wildmatch.c`
//! reads the rest of the pattern: wildcards, escapes and bracket
//! expressions such as `[a-z]`.

use std::ops::Range;

use super::{Kind, Tokens};

pub fn lex(text: &str, out: &mut Tokens) {
    // git skips a byte order mark at the start of the file.
    let mut start = if text.starts_with('\u{feff}') { 3 } else { 0 };
    for line in text[start..].split_inclusive('\n') {
        // git removes one carriage return before the line feed. It adds a
        // line feed to a file that doesn't end with one, so this also
        // goes for the last line.
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        lex_line(content, start, out);
        start += line.len();
    }
}

/// Lex one line, without its line ending. `at` is where it starts in the
/// text.
fn lex_line(line: &str, at: usize, out: &mut Tokens) {
    let b = line.as_bytes();
    let mut push = |r: Range<usize>, kind| out.push(at + r.start..at + r.end, kind);
    // Only a `#` at the very start makes a comment. After a space or a
    // `!`, it's part of the pattern.
    if b.first() == Some(&b'#') {
        push(0..b.len(), Kind::Comment);
        return;
    }
    let mut i = 0;
    if b.first() == Some(&b'!') {
        push(0..1, Kind::Keyword);
        i = 1;
    }
    while i < b.len() {
        match b[i] {
            b'\\' => {
                // A backslash at the end of the pattern escapes nothing,
                // and the pattern matches nothing.
                let Some(end) = escape(line, i) else {
                    return;
                };
                push(i..end, Kind::Escape);
                i = end;
            }
            // `**` is special only between slashes, but it gets the same
            // color as `*` everywhere.
            b'*' | b'?' => {
                push(i..i + 1, Kind::Operator);
                i += 1;
            }
            b'/' => {
                push(i..i + 1, Kind::Punctuation);
                i += 1;
            }
            b'[' => {
                // git gives up on the whole pattern when a bracket
                // expression isn't closed or names a class that doesn't
                // exist. The pattern then matches nothing, so we leave the
                // rest of the line plain.
                let mut parts = Vec::new();
                let Some(end) = bracket(line, i, &mut parts) else {
                    return;
                };
                for (r, kind) in parts {
                    push(r, kind);
                }
                i = end;
            }
            _ => i += 1,
        }
    }
}

/// The end of the escape whose backslash is at `i`, or `None` if nothing
/// follows the backslash. The escape covers the whole character after
/// it, though `dowild` only reads its first byte. The other bytes of the
/// character are matched as they are, which comes to the same thing.
fn escape(line: &str, i: usize) -> Option<usize> {
    let c = line[i + 1..].chars().next()?;
    Some(i + 1 + c.len_utf8())
}

/// The names that `[:name:]` can use inside a bracket expression.
const CLASSES: &[&str] = &[
    "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "xdigit",
];

/// Read the bracket expression whose `[` is at `open`, and add its colored
/// parts to `parts`. Return where it ends, or `None` if git would give up
/// on the pattern there.
///
/// `dowild` reads one item before it looks for the closing `]`, so `[]]`
/// and `[!]]` hold a `]`. A `-` makes a range only between two items, as
/// in `a-z`, and is plain at the start or end. A `[` that doesn't start a
/// class such as `[:alpha:]` is plain too.
fn bracket(line: &str, open: usize, parts: &mut Vec<(Range<usize>, Kind)>) -> Option<usize> {
    let b = line.as_bytes();
    parts.push((open..open + 1, Kind::Punctuation));
    let mut i = open + 1;
    if matches!(b.get(i), Some(b'!' | b'^')) {
        parts.push((i..i + 1, Kind::Operator));
        i += 1;
    }
    // The last byte of the item before, which a `-` after it makes the
    // start of a range. `dowild` reads bytes, so for a character such as
    // `é`, that's its last byte. A range or a class leaves nothing for a
    // `-` to use.
    let mut prev: Option<u8> = None;
    loop {
        let c = *b.get(i)?;
        if c == b'\\' {
            let end = escape(line, i)?;
            parts.push((i..end, Kind::Escape));
            prev = Some(b[end - 1]);
            i = end;
        } else if c == b'-' && prev.is_some() && b.get(i + 1).is_some_and(|&c| c != b']') {
            parts.push((i..i + 1, Kind::Operator));
            let start = i + 1;
            // The character that ends the range, which may be escaped.
            let (last, end) = if b[start] == b'\\' {
                let end = escape(line, start)?;
                parts.push((start..end, Kind::Escape));
                (start + 1, end)
            } else {
                (start, start + char_len(line, start))
            };
            // Only the first byte of that character ends the range. Any
            // other bytes are items of their own.
            prev = (end - last > 1).then(|| b[end - 1]);
            i = end;
        } else if c == b'[' && b.get(i + 1) == Some(&b':') {
            let name = i + 2;
            let close = name + b[name..].iter().position(|&c| c == b']')?;
            // A class needs a `:` just before the `]`, and the one in `[:`
            // doesn't count. Without it, the `[` is just a `[`.
            if close > name && b[close - 1] == b':' {
                if !CLASSES.contains(&&line[name..close - 1]) {
                    return None;
                }
                parts.push((i..close + 1, Kind::Constant));
                prev = None;
                i = close + 1;
            } else {
                prev = Some(b'[');
                i += 1;
            }
        } else {
            let end = i + char_len(line, i);
            prev = Some(b[end - 1]);
            i = end;
        }
        if *b.get(i)? == b']' {
            parts.push((i..i + 1, Kind::Punctuation));
            return Some(i + 1);
        }
    }
}

/// The length of the character at `i`.
fn char_len(line: &str, i: usize) -> usize {
    line[i..].chars().next().map_or(1, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use proptest::prelude::*;

    use super::*;

    /// Each token as its text and kind. Tokens of the same kind with
    /// nothing between them come out as one.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out.0.iter().map(|(r, k)| (&text[r.clone()], *k)).collect()
    }

    #[test]
    fn lexes_a_file() {
        use Kind::*;
        let text = "\
# Build output
/target/
**/*.o
!keep.o
doc/**/[!_]*.html
";
        assert_eq!(
            tokens(text),
            [
                ("# Build output", Comment),
                ("/", Punctuation),
                ("/", Punctuation),
                ("**", Operator),
                ("/", Punctuation),
                ("*", Operator),
                ("!", Keyword),
                ("/", Punctuation),
                ("**", Operator),
                ("/[", Punctuation),
                ("!", Operator),
                ("]", Punctuation),
                ("*", Operator),
            ]
        );
    }

    #[test]
    fn comments_and_negation() {
        use Kind::*;
        // Only a `#` at the start of a line starts a comment.
        assert_eq!(tokens("#a\n #b\n!#c"), [("#a", Comment), ("!", Keyword)]);
        // A backslash makes `#` and `!` plain characters.
        assert_eq!(tokens("\\#a\n\\!b"), [("\\#", Escape), ("\\!", Escape)]);
        // The `!` that negates only comes first.
        assert_eq!(tokens("!!a\na!"), [("!", Keyword)]);
        // git skips a byte order mark, and the carriage return of a CRLF.
        assert_eq!(
            tokens("\u{feff}#a\r\n#b\r"),
            [("#a", Comment), ("#b", Comment)]
        );
    }

    #[test]
    fn escapes() {
        use Kind::*;
        assert_eq!(tokens("a\\*\\\\\\ \\é"), [("\\*\\\\\\ \\é", Escape)]);
        // A backslash at the end escapes nothing.
        assert_eq!(tokens("a\\"), []);
        assert_eq!(tokens("a\\\r\n*"), [("*", Operator)]);
    }

    #[test]
    fn brackets() {
        use Kind::*;
        assert_eq!(
            tokens("[a-z]"),
            [("[", Punctuation), ("-", Operator), ("]", Punctuation)]
        );
        assert_eq!(
            tokens("[^[:alpha:]_]"),
            [
                ("[", Punctuation),
                ("^", Operator),
                ("[:alpha:]", Constant),
                ("]", Punctuation)
            ]
        );
        // The first item can be a `]`.
        assert_eq!(
            tokens("[]a][!]]"),
            [
                ("[", Punctuation),
                ("][", Punctuation),
                ("!", Operator),
                ("]", Punctuation)
            ]
        );
        // A `-` at either end is plain, and so is one after a range.
        assert_eq!(
            tokens("[-a-c-e-]"),
            [("[", Punctuation), ("-", Operator), ("]", Punctuation)]
        );
        // Escapes, and a range that ends in one.
        assert_eq!(
            tokens("[\\]-\\a*]"),
            [
                ("[", Punctuation),
                ("\\]", Escape),
                ("-", Operator),
                ("\\a", Escape),
                ("]", Punctuation)
            ]
        );
        // A `[` that doesn't start a class is plain. Here the first
        // expression holds `[` and `:`, and the second holds `a`.
        assert_eq!(
            tokens("[[:][a]"),
            [("[", Punctuation), ("][", Punctuation), ("]", Punctuation)]
        );
        // We checked these two with git. The first holds `:`, `a` and
        // `[`. The second holds a range from `*` to `[`, then the
        // characters of `:alpha:`.
        assert_eq!(tokens("[:aa[:]"), [("[", Punctuation), ("]", Punctuation)]);
        assert_eq!(
            tokens("[\\*-[:alpha:]"),
            [
                ("[", Punctuation),
                ("\\*", Escape),
                ("-", Operator),
                ("]", Punctuation)
            ]
        );
        // Wildcards and slashes in brackets are plain.
        assert_eq!(tokens("[*?/]"), [("[", Punctuation), ("]", Punctuation)]);
    }

    #[test]
    fn broken_brackets() {
        use Kind::*;
        // The pattern matches nothing, so the rest of the line is plain.
        assert_eq!(tokens("*[a\n*"), [("*", Operator), ("*", Operator)]);
        assert_eq!(tokens("*[]\n"), [("*", Operator)]);
        assert_eq!(tokens("*[a\\]"), [("*", Operator)]);
        assert_eq!(tokens("*[[:foo:]]*"), [("*", Operator)]);
        assert_eq!(tokens("*[[::]]*"), [("*", Operator)]);
    }

    /// One thing in a pattern, as the colors describe it.
    #[derive(Debug, Clone)]
    enum Item {
        Byte(u8),
        Any,
        Star,
        Set { negated: bool, members: Vec<Member> },
    }

    #[derive(Debug, Clone)]
    enum Member {
        Byte(u8),
        Range(u8, u8),
        Class(String),
    }

    impl Member {
        fn contains(&self, c: u8) -> bool {
            match self {
                Member::Byte(b) => *b == c,
                Member::Range(lo, hi) => (*lo..=*hi).contains(&c),
                Member::Class(name) => match name.as_str() {
                    "alnum" => c.is_ascii_alphanumeric(),
                    "alpha" => c.is_ascii_alphabetic(),
                    "blank" => matches!(c, b' ' | b'\t'),
                    "cntrl" => c.is_ascii_control(),
                    "digit" => c.is_ascii_digit(),
                    "graph" => c.is_ascii_graphic(),
                    "lower" => c.is_ascii_lowercase(),
                    "print" => (0x20..=0x7e).contains(&c),
                    "punct" => c.is_ascii_punctuation(),
                    "space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r'),
                    "upper" => c.is_ascii_uppercase(),
                    "xdigit" => c.is_ascii_hexdigit(),
                    _ => panic!("no class {name}"),
                },
            }
        }
    }

    impl Item {
        /// Whether the item matches the single byte `c`, or `None` if it
        /// isn't one that matches exactly one byte.
        fn matches(&self, c: u8) -> Option<bool> {
            match self {
                Item::Byte(b) => Some(*b == c),
                Item::Set { negated, members } => {
                    Some(c != b'/' && members.iter().any(|m| m.contains(c)) != *negated)
                }
                Item::Any | Item::Star => None,
            }
        }
    }

    /// A pattern, as its colors describe it.
    #[derive(Debug, Default)]
    struct Pattern {
        items: Vec<Item>,
        /// Whether git gives up on the pattern, which a plain `[` or `\`
        /// shows.
        broken: bool,
    }

    fn read(pat: &[u8], kinds: &[Option<Kind>]) -> Pattern {
        let mut out = Pattern::default();
        let items = &mut out.items;
        let mut i = 0;
        while i < pat.len() {
            match (kinds[i], pat[i]) {
                (Some(Kind::Escape), b'\\') => {
                    items.push(Item::Byte(pat[i + 1]));
                    i += 2;
                    continue;
                }
                (Some(Kind::Operator), b'*') => items.push(Item::Star),
                (Some(Kind::Operator), b'?') => items.push(Item::Any),
                (Some(Kind::Punctuation), b'[') => {
                    let (set, end) = read_set(pat, kinds, i);
                    items.push(set);
                    i = end;
                    continue;
                }
                (None, b'[' | b'\\') => {
                    items.extend(pat[i..].iter().map(|&b| Item::Byte(b)));
                    out.broken = true;
                    return out;
                }
                // The rest of an escaped character, a slash, or plain text.
                (Some(Kind::Escape) | Some(Kind::Punctuation) | None, c) => {
                    items.push(Item::Byte(c));
                }
                (k, c) => panic!("{:?} has {k:?} at {i}", c as char),
            }
            i += 1;
        }
        out
    }

    /// Read the bracket expression whose `[` is at `i`.
    fn read_set(pat: &[u8], kinds: &[Option<Kind>], mut i: usize) -> (Item, usize) {
        i += 1;
        let negated = kinds[i] == Some(Kind::Operator) && matches!(pat[i], b'!' | b'^');
        if negated {
            i += 1;
        }
        let mut members = Vec::new();
        while !(kinds[i] == Some(Kind::Punctuation) && pat[i] == b']') {
            match (kinds[i], pat[i]) {
                (Some(Kind::Escape), b'\\') => {
                    members.push(Member::Byte(pat[i + 1]));
                    i += 2;
                }
                (Some(Kind::Operator), b'-') => {
                    let Some(Member::Byte(lo)) = members.last() else {
                        panic!("a range without a start at {i}");
                    };
                    let (hi, len) = if pat[i + 1] == b'\\' && kinds[i + 1] == Some(Kind::Escape) {
                        (pat[i + 2], 3)
                    } else {
                        (pat[i + 1], 2)
                    };
                    members.push(Member::Range(*lo, hi));
                    i += len;
                }
                (Some(Kind::Constant), b'[') => {
                    let close = i + pat[i..].iter().position(|&c| c == b']').unwrap_or(0);
                    let name = String::from_utf8_lossy(&pat[i + 2..close - 1]);
                    members.push(Member::Class(name.into_owned()));
                    i = close + 1;
                }
                (_, c) => {
                    members.push(Member::Byte(c));
                    i += 1;
                }
            }
        }
        (Item::Set { negated, members }, i + 1)
    }

    /// A printable ASCII character other than `/` that `item` matches,
    /// or doesn't. Paths keep to those so that every file system can
    /// hold them.
    fn pick(item: &Item, matching: bool) -> Option<u8> {
        (b' '..=b'~').find(|&c| c != b'/' && item.matches(c) == Some(matching))
    }

    /// A path that the items match, one byte for each item.
    fn path(items: &[Item]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Item::Byte(b) => out.push(*b),
                Item::Any | Item::Star => out.push(b'x'),
                Item::Set { .. } => out.push(pick(item, true)?),
            }
        }
        Some(out)
    }

    /// Whether git would read `path` as the same path. It drops empty
    /// parts, `.` and `..`, and reads a path that starts with `/` as
    /// absolute.
    fn plain_path(path: &[u8]) -> bool {
        !path.is_empty()
            && path
                .split(|&b| b == b'/')
                .all(|part| !matches!(part, b"" | b"." | b".." | b".git"))
    }

    /// A repository for asking git which paths a pattern matches.
    struct Repo(tempfile::TempDir);

    impl Repo {
        fn new() -> Repo {
            let dir = tempfile::tempdir().expect("tempdir");
            let ok = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .expect("git init")
                .success();
            assert!(ok);
            Repo(dir)
        }

        /// The paths that the gitignore line `line` matches, out of
        /// `paths`, even when the line negates its pattern.
        fn matching(&self, line: &[u8], paths: &[Vec<u8>]) -> Vec<Vec<u8>> {
            let mut file = line.to_vec();
            file.push(b'\n');
            std::fs::write(self.0.path().join(".gitignore"), file).expect("write .gitignore");
            let mut child = Command::new("git")
                .args(["-c", "core.ignoreCase=false", "check-ignore"])
                .args(["--no-index", "--stdin", "-z", "-v"])
                .current_dir(self.0.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("git check-ignore");
            let mut input = Vec::new();
            for p in paths {
                // A path that starts with `:` would be pathspec magic.
                input.extend_from_slice(b"./");
                input.extend_from_slice(p);
                input.push(0);
            }
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(&input)
                .expect("write paths");
            let out = child.wait_with_output().expect("git check-ignore");
            // Each match is the source, the line number, the pattern and
            // the path.
            let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
            fields
                .as_chunks::<4>()
                .0
                .iter()
                .map(|f| f[3].strip_prefix(b"./").unwrap_or(f[3]).to_vec())
                .collect()
        }
    }

    /// A line made of pieces of patterns and bracket expressions. Some
    /// of the bracket expressions aren't closed.
    fn line() -> impl Strategy<Value = String> {
        let piece = prop::sample::select(vec![
            "a", "b", "é", " ", "/", "*", "**", "?", "\\", "\\\\", "\\*", "\\é", "[", "]", "!",
            "-", ":", "#",
        ]);
        let inner = prop::sample::select(vec![
            "a",
            "c",
            "z",
            "é",
            " ",
            "/",
            "*",
            "-",
            "]",
            "[",
            ":",
            "!",
            "^",
            "\\",
            "\\]",
            "\\a",
            "\\-",
            "\\é",
            "a-c",
            "[:alpha:]",
            "[:digit:]",
            "[:space:]",
            "[:foo:]",
            "[:",
            ":]",
        ]);
        let bracket = (
            prop::sample::select(vec!["[", "[!", "[^"]),
            prop::collection::vec(inner, 0..6),
            prop::sample::select(vec!["]", "]", "]", ""]),
        )
            .prop_map(|(open, inner, close)| format!("{open}{}{close}", inner.concat()));
        let piece = prop_oneof![3 => piece.prop_map(String::from), 1 => bracket];
        prop::collection::vec(piece, 0..6).prop_map(|p| p.concat())
    }

    proptest! {
        /// The colors of a pattern describe what git reads. We build a
        /// path from what the colors say each part of the pattern
        /// matches, and check with git that the pattern matches it. Each
        /// part of a pattern without `*` matches exactly one byte. So
        /// changing one byte of the path to one that its part doesn't
        /// match must make the pattern fail.
        #[test]
        fn colors_follow_git(line in line()) {
            // Comments are simple, and git trims spaces at the end,
            // which the colors don't show.
            if line.starts_with('#') || line.ends_with(' ') {
                return Ok(());
            }
            let mut out = Tokens::default();
            lex(&line, &mut out);
            let mut kinds = vec![None; line.len()];
            for (r, k) in out.0 {
                kinds[r].fill(Some(k));
            }
            let start = usize::from(kinds.first() == Some(&Some(Kind::Keyword)));
            let pat = &line.as_bytes()[start..];
            // A pattern for directories, which ends in `/`, would need the
            // path to be one.
            if pat.ends_with(b"/") {
                return Ok(());
            }
            let Pattern { mut items, broken } = read(pat, &kinds[start..]);
            // git matches a pattern that starts with `/` from the
            // directory of the .gitignore, which is what it does anyway
            // when the pattern has a `/` in the middle.
            if pat.first() == Some(&b'/') && kinds[start] == Some(Kind::Punctuation) {
                items.remove(0);
            }
            let Some(p) = path(&items).filter(|p| plain_path(p)) else {
                return Ok(());
            };
            let no_star = !items.iter().any(|i| matches!(i, Item::Star));
            let mut others = Vec::new();
            for (j, item) in items.iter().enumerate() {
                if let Some(c) = pick(item, false).filter(|_| no_star && !broken) {
                    let mut p = p.clone();
                    p[j] = c;
                    if plain_path(&p) {
                        others.push(p);
                    }
                }
            }
            let mut paths = others.clone();
            paths.push(p.clone());
            let matching = Repo::new().matching(line.as_bytes(), &paths);
            let show = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
            prop_assert_eq!(
                matching.contains(&p),
                !broken,
                "{:?} and {:?}",
                line,
                show(&p)
            );
            for o in &others {
                prop_assert!(!matching.contains(o), "{:?} and {:?}", line, show(o));
            }
        }
    }
}
