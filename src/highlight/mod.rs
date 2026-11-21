//! Syntax highlighting for diffs, in the background.
//!
//! Each language has a lexer of its own, written with Logos, and tokens
//! get their colors from what the lexer alone can tell. There is no
//! parser, so we can't tell which identifiers name functions or types.
//! Identifiers are marked as such instead, so that drawing can give each
//! name a color of its own, unless the user turns that off.
//!
//! When we have the full text of a file, we lex the whole file so that
//! multi-line strings and comments come out right, then pick out the lines
//! the diff shows. When we only have the diff, we lex each hunk's old and
//! new side on its own.

mod c;
mod cmake;
mod gitignore;
mod javascript;
mod json;
mod markdown;
mod racket;
mod rust;
mod shrubbery;
mod toml;
mod yaml;

use std::ops::Range;
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::git::diff::{DiffLine, Hunk, LineKind};
use crate::util::Generation;

/// What a token is, as far as its color goes. Themes give each kind a
/// color, or none to leave it plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Comment,
    String,
    /// An escape inside a string, such as `\n`.
    Escape,
    Number,
    /// Named constants and literals such as `true`.
    Constant,
    Keyword,
    Type,
    Attribute,
    Lifetime,
    Operator,
    Punctuation,
    /// An identifier that gets no other color. Its color depends on its
    /// name, or it is plain text if the user turns that off.
    Identifier,
}

/// The tokens of a text that get a color, in order, as byte ranges. Plain
/// text has none.
#[derive(Debug, Default)]
pub struct Tokens(Vec<(Range<usize>, Kind)>);

impl Tokens {
    /// Add a token after the last one. One that follows a token of the
    /// same kind with nothing between them is joined to it, except for
    /// identifiers. Each of those gets a color from its own name.
    fn push(&mut self, r: Range<usize>, kind: Kind) {
        if r.is_empty() {
            return;
        }
        if let Some((last, k)) = self.0.last_mut()
            && *k == kind
            && kind != Kind::Identifier
            && last.end == r.start
        {
            last.end = r.end;
            return;
        }
        self.0.push((r, kind));
    }

    /// Color the range `r` of `text` as a comment, but not the whitespace
    /// at the end of each of its lines. Whether a comment that covers
    /// several lines covers the whitespace at the end of one can depend
    /// on what comes on the next line, and colors shouldn't.
    fn push_comment(&mut self, text: &str, r: Range<usize>) {
        let mut start = r.start;
        for line in text[r].split_inclusive('\n') {
            let end = start + line.trim_end().len();
            self.push(start..end, Kind::Comment);
            start += line.len();
        }
    }
}

#[cfg(test)]
impl Tokens {
    /// Each token but the identifiers, as its text and kind. Most lexer
    /// tests are about the other tokens, and every identifier in them
    /// would make them hard to read.
    fn colored<'a>(&self, text: &'a str) -> Vec<(&'a str, Kind)> {
        self.0
            .iter()
            .filter(|(_, k)| *k != Kind::Identifier)
            .map(|(r, k)| (&text[r.clone()], *k))
            .collect()
    }

    /// The text of each identifier.
    fn identifiers<'a>(&self, text: &'a str) -> Vec<&'a str> {
        self.0
            .iter()
            .filter(|(_, k)| *k == Kind::Identifier)
            .map(|(r, _)| &text[r.clone()])
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cmake,
    Cpp,
    Gitignore,
    JavaScript,
    Json,
    Markdown,
    Racket,
    Rhombus,
    Rust,
    Scribble,
    Toml,
    Yaml,
}

impl Language {
    /// The language of a file, from its name.
    pub fn for_path(path: &str) -> Option<Language> {
        let name = path.rsplit('/').next().unwrap_or(path);
        // Some tools keep TOML in files with names of their own.
        if matches!(name, "Cargo.lock" | "Pipfile" | "poetry.lock" | "uv.lock") {
            return Some(Language::Toml);
        }
        // configure_file templates such as `FooConfig.cmake.in` are
        // mostly CMake too.
        if name == "CMakeLists.txt" || name.ends_with(".cmake.in") {
            return Some(Language::Cmake);
        }
        if matches!(name, ".clang-format" | ".clang-tidy") {
            return Some(Language::Yaml);
        }
        if matches!(name, "composer.lock" | "flake.lock" | "Pipfile.lock") {
            return Some(Language::Json);
        }
        match name.rsplit_once('.')?.1 {
            "c" => Some(Language::C),
            "cmake" => Some(Language::Cmake),
            // Headers ending in `.h` can be C or C++. We read them as C++,
            // as Sublime Text does, so that `class` and `template` get
            // colors. In a C header, a name such as `new` then does too.
            "h" | "C" | "H" | "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++"
            | "ipp" | "tpp" | "inl" | "cppm" | "ixx" | "ccm" | "cxxm" | "c++m" => {
                Some(Language::Cpp)
            }
            // `.gitignore`, and templates such as `Rust.gitignore`.
            "gitignore" => Some(Language::Gitignore),
            "js" | "mjs" | "cjs" => Some(Language::JavaScript),
            "json" | "jsonc" | "jsonl" | "ndjson" | "geojson" | "ipynb" | "webmanifest" => {
                Some(Language::Json)
            }
            "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdwn" => Some(Language::Markdown),
            "rkt" | "rktl" | "rktd" => Some(Language::Racket),
            "rhm" => Some(Language::Rhombus),
            "rs" => Some(Language::Rust),
            "scrbl" => Some(Language::Scribble),
            "toml" => Some(Language::Toml),
            "yaml" | "yml" => Some(Language::Yaml),
            _ => None,
        }
    }

    pub fn lex(self, text: &str) -> Tokens {
        let mut out = Tokens::default();
        match self {
            Language::C | Language::Cpp => c::lex(text, self, &mut out),
            Language::Cmake => cmake::lex(text, &mut out),
            Language::Gitignore => gitignore::lex(text, &mut out),
            Language::JavaScript => javascript::lex(text, &mut out),
            Language::Json => json::lex(text, &mut out),
            Language::Markdown => markdown::lex(text, &mut out),
            Language::Racket | Language::Rhombus | Language::Scribble => {
                racket::lex(text, self, &mut out);
            }
            Language::Rust => rust::lex(text, &mut out),
            Language::Toml => toml::lex(text, &mut out),
            Language::Yaml => yaml::lex(text, &mut out),
        }
        out
    }
}

/// Colored spans for one line, as byte ranges into the line text, in
/// order and without overlaps. Text outside them is plain.
pub type LineSpans = Vec<(u32, u32, Kind)>;

#[derive(Debug, Clone, Default)]
pub struct FileHighlight {
    /// Indexed by hunk, then by line within the hunk.
    pub lines: Vec<Vec<LineSpans>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub generation: u64,
    pub section: u8,
    pub file: usize,
}

pub struct Job {
    pub key: Key,
    pub path: String,
    pub hunks: Vec<Hunk>,
    pub old: Option<Arc<str>>,
    pub new: Option<Arc<str>>,
}

pub struct Highlighter {
    tx: UnboundedSender<Job>,
}

impl Highlighter {
    /// The returned future does the work. The caller should spawn it. It
    /// ends when the `Highlighter` is dropped. Jobs whose key has a
    /// generation that is no longer current in `files` are skipped.
    pub fn new(
        files: Generation,
        done: impl Fn(Key, FileHighlight) + Send + Sync + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (tx, rx) = unbounded_channel();
        let worker = worker(rx, files, Arc::new(done));
        (Highlighter { tx }, worker)
    }

    pub fn request(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

async fn worker(
    mut rx: UnboundedReceiver<Job>,
    files: Generation,
    done: Arc<dyn Fn(Key, FileHighlight) + Send + Sync>,
) {
    while let Some(job) = rx.recv().await {
        if !files.is_current(job.key.generation) {
            continue;
        }
        let Some(lang) = Language::for_path(&job.path) else {
            continue;
        };
        let done = done.clone();
        crate::util::blocking(move || done(job.key, highlight_job(lang, &job))).await;
    }
}

/// Lex `text` and split its tokens up by line, for the first `max` lines.
/// Each line's spans leave out its newline.
fn highlight_text(lang: Language, text: &str, max: usize) -> Vec<LineSpans> {
    let end = text.split_inclusive('\n').take(max).map(str::len).sum();
    let text = &text[..end];
    let tokens = lang.lex(text).0;
    let mut out = Vec::new();
    let mut first = 0;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.strip_suffix('\n').unwrap_or(line).len();
        // A token can cover several lines, so keep it until it ends.
        while tokens.get(first).is_some_and(|(r, _)| r.end <= start) {
            first += 1;
        }
        let mut spans = Vec::new();
        for (r, kind) in &tokens[first..] {
            if r.start >= end {
                break;
            }
            let (s, e) = (r.start.max(start), r.end.min(end));
            if s < e {
                spans.push(((s - start) as u32, (e - start) as u32, *kind));
            }
        }
        out.push(spans);
        start += line.len();
    }
    out
}

/// Highlight one side of a hunk on its own.
fn highlight_side<'a>(lang: Language, lines: impl Iterator<Item = &'a DiffLine>) -> Vec<LineSpans> {
    let mut text = String::new();
    for l in lines {
        text.push_str(&l.text);
        if !l.no_eol {
            text.push('\n');
        }
    }
    highlight_text(lang, &text, usize::MAX)
}

fn highlight_job(lang: Language, job: &Job) -> FileHighlight {
    let max_line = |no: fn(&DiffLine) -> Option<u32>| {
        job.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(no)
            .max()
            .unwrap_or(0) as usize
    };
    let old_full = job
        .old
        .as_ref()
        .map(|t| highlight_text(lang, t, max_line(|l| l.old_no)));
    let new_full = job
        .new
        .as_ref()
        .map(|t| highlight_text(lang, t, max_line(|l| l.new_no)));

    let mut result = FileHighlight::default();
    for hunk in &job.hunks {
        // Lex each side of the hunk on its own when we don't have the full
        // text for that side.
        let old_side = old_full
            .is_none()
            .then(|| highlight_side(lang, hunk.lines.iter().filter(|l| l.kind != LineKind::Add)));
        let new_side = new_full
            .is_none()
            .then(|| highlight_side(lang, hunk.lines.iter().filter(|l| l.kind != LineKind::Del)));
        let mut oi = 0;
        let mut ni = 0;
        let mut out = Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            let pick = |full: &Option<Vec<LineSpans>>,
                        side: &Option<Vec<LineSpans>>,
                        no: Option<u32>,
                        i: usize|
             -> LineSpans {
                let spans = match (full, side) {
                    (Some(f), _) => no.and_then(|n| f.get(n as usize - 1)),
                    (None, Some(s)) => s.get(i),
                    _ => None,
                };
                spans.cloned().unwrap_or_default()
            };
            let from_old = || pick(&old_full, &old_side, line.old_no, oi);
            let from_new = || pick(&new_full, &new_side, line.new_no, ni);
            let spans = match line.kind {
                LineKind::Del => from_old(),
                LineKind::Add => from_new(),
                // Prefer the new side for context, which is what's on disk.
                LineKind::Context => {
                    if new_full.is_some() || old_full.is_none() {
                        from_new()
                    } else {
                        from_old()
                    }
                }
            };
            if line.kind != LineKind::Add {
                oi += 1;
            }
            if line.kind != LineKind::Del {
                ni += 1;
            }
            out.push(spans);
        }
        result.lines.push(out);
    }
    result
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::git::commit::diff_texts;

    const PATHS: &[&str] = &[
        "a.c",
        "CMakeLists.txt",
        "a.cpp",
        ".gitignore",
        "a.js",
        "a.json",
        "a.md",
        "a.rkt",
        "a.rhm",
        "a.rs",
        "a.scrbl",
        "a.toml",
        "a.yaml",
    ];

    /// A line made of pieces that start or end strings, comments and
    /// @-forms in some language, along with tabs, wide characters and a
    /// stray carriage return.
    fn line() -> impl Strategy<Value = String> {
        let mut pieces = vec![
            "x", "fn", " ", "\t", "\"", "'", "`", "```", "\"\"\"", "//", "/*", "*/", "#", "<!--",
            "-->", "<a>", "{", "}", "(", ")", "\\", "$x", "=", ";", "0x1F", "r#\"", "\"#", "- ",
            "b'", "'a'", "中", "👍", "e\u{301}", "\r", "#|", "|#", "#;", ";", "|", "#\\", "#<<",
            "1/2", "#:", "@", "@;", "@foo", "[", "]", "|{", "}|", "|<{", "}>|", "|<@",
        ];
        // Pieces of shrubbery notation.
        pieces.extend([
            "#//", "~kw", "#{", "~#{", "«", "»", ":", "-1", "1.", "x.y", ",", "  ", "@//", "a:",
        ]);
        // Pieces of C and C++, such as directives and raw strings.
        pieces.extend([
            "#if 0", "#if", "#else", "#endif", "#include", "<a.h>", "#error", "%:", "R\"x(",
            ")x\"", "R\"(", ")\"", "1'0", "[[", "\\ ", "u8'", "defined",
        ]);
        // Pieces of TOML, such as table headers, multi-line strings and
        // dates.
        pieces.extend([
            "[[", "]]", "'''", "\"\"\"\"", "a.b", "1979", "-05-27", "T", "07:32", "Z", "+inf",
            "true =", "\\e",
        ]);
        // Pieces of Markdown, such as containers, fences, links and HTML.
        pieces.extend([
            "> ",
            "- ",
            "* ",
            "1. ",
            "2) ",
            "    ",
            "   ",
            "~~~",
            "---",
            "===",
            "***",
            "# ",
            "|",
            "|-|",
            "| --- |",
            "[a]",
            "(b)",
            "](",
            "[a]: ",
            "![",
            "<div>",
            "</div>",
            "<pre>",
            "<a href=\"x\">",
            "<http://x>",
            "&amp;",
            "[ ] ",
            "```rust",
            "```md",
            "<?",
            "?>",
        ]);
        // Pieces of CMake, such as brackets, variable references and
        // generator expressions.
        pieces.extend([
            "if(", "endif()", "AND", "#[[", "]]", "[=[", "]=]", "${", "$ENV{", "$<", ">", "\\;",
            "$(x)", "a\"b c\"",
        ]);
        // Pieces of YAML, such as indicators, block scalar headers and
        // document markers.
        pieces.extend([
            "a: ", "- ", "? ", ": ", "|", ">-", "|2+", "---", "...", "&a", "*a", "!!str", "!<x>",
            "%YAML", "''", "{a: b}", "[a, b]", ".inf", "~", "  ", "    ",
        ]);
        // Pieces of JavaScript, such as template literals and regular
        // expressions.
        pieces.extend([
            "`a${", "}b`", "/a/g", "[/]", "x /", "?.", "=>", "if (", ")", "@x", "#!", "#p",
            "\\u{41}", "of", "import", "from", "'a'",
        ]);
        // Pieces of JSON, such as keys and escapes.
        pieces.extend([
            "\"a\": ", "\"a\"", "\\u00e9", "-1.5e3", "01", "null", "[1, 2]",
        ]);
        // Pieces of gitignore patterns.
        pieces.extend(["!", "**/", "?", "[!a-]", "[[:alpha:]]", "\\#"]);
        // Lines that choose how the rest of a Racket file is read.
        pieces.extend([
            "#lang at-exp racket",
            "#lang scribble/manual",
            "#lang rhombus",
            "#lang rhombus/scribble/manual",
            "#lang my/lang",
        ]);
        let piece = prop::sample::select(pieces);
        prop::collection::vec(piece, 0..8).prop_map(|p| p.concat())
    }

    /// Lines picked from a small pool, so that two texts share some, each
    /// ending in LF or CRLF, and maybe without a final newline.
    fn text(pool: Vec<String>) -> impl Strategy<Value = String> {
        let n = pool.len();
        (
            prop::collection::vec((0..n, any::<bool>()), 0..20),
            any::<bool>(),
        )
            .prop_map(move |(lines, eol)| {
                let mut s = String::new();
                for (i, &(k, crlf)) in lines.iter().enumerate() {
                    s.push_str(&pool[k]);
                    if i + 1 < lines.len() || eol {
                        s.push_str(if crlf { "\r\n" } else { "\n" });
                    }
                }
                s
            })
    }

    fn texts() -> impl Strategy<Value = (String, String)> {
        prop::collection::vec(line(), 1..6).prop_flat_map(|pool| (text(pool.clone()), text(pool)))
    }

    /// The spans of a line must be in order, not overlap, not be empty,
    /// and stay inside the line. Every span must start and end on a
    /// character boundary.
    fn check_spans(line: &str, spans: &LineSpans) -> Result<(), TestCaseError> {
        let mut pos = 0;
        for &(s, e, _) in spans {
            prop_assert!(pos <= s, "overlap in {line:?}: {spans:?}");
            prop_assert!(s < e, "empty span in {line:?}: {spans:?}");
            prop_assert!(e as usize <= line.len(), "{line:?}: {spans:?}");
            prop_assert!(
                line.is_char_boundary(s as usize) && line.is_char_boundary(e as usize),
                "span inside a character of {line:?}: {spans:?}"
            );
            pos = e;
        }
        Ok(())
    }

    fn job(path: &str, hunks: &[Hunk], old: Option<&str>, new: Option<&str>) -> Job {
        Job {
            key: Key {
                generation: 0,
                section: 0,
                file: 0,
            },
            path: path.to_string(),
            hunks: hunks.to_vec(),
            old: old.map(Arc::from),
            new: new.map(Arc::from),
        }
    }

    #[test]
    fn finds_languages_by_extension() {
        assert_eq!(Language::for_path("src/main.rs"), Some(Language::Rust));
        assert_eq!(Language::for_path("info.rkt"), Some(Language::Racket));
        assert_eq!(Language::for_path("doc.scrbl"), Some(Language::Scribble));
        assert_eq!(Language::for_path("main.rhm"), Some(Language::Rhombus));
        assert_eq!(Language::for_path("lib/a.c"), Some(Language::C));
        assert_eq!(Language::for_path("a.h"), Some(Language::Cpp));
        assert_eq!(Language::for_path("a.cpp"), Some(Language::Cpp));
        assert_eq!(Language::for_path("Cargo.toml"), Some(Language::Toml));
        assert_eq!(Language::for_path("x/Cargo.lock"), Some(Language::Toml));
        assert_eq!(Language::for_path(".gitignore"), Some(Language::Gitignore));
        assert_eq!(
            Language::for_path("t/Rust.gitignore"),
            Some(Language::Gitignore)
        );
        assert_eq!(
            Language::for_path("x/CMakeLists.txt"),
            Some(Language::Cmake)
        );
        assert_eq!(Language::for_path("a.cmake"), Some(Language::Cmake));
        assert_eq!(
            Language::for_path("FooConfig.cmake.in"),
            Some(Language::Cmake)
        );
        assert_eq!(Language::for_path("a/b.yml"), Some(Language::Yaml));
        assert_eq!(Language::for_path("a.js"), Some(Language::JavaScript));
        assert_eq!(Language::for_path("a.mjs"), Some(Language::JavaScript));
        assert_eq!(Language::for_path(".clang-format"), Some(Language::Yaml));
        assert_eq!(Language::for_path("a/b.json"), Some(Language::Json));
        assert_eq!(Language::for_path("tsconfig.jsonc"), Some(Language::Json));
        assert_eq!(Language::for_path("x/flake.lock"), Some(Language::Json));
        assert_eq!(Language::for_path("a.lock"), None);
        assert_eq!(Language::for_path("a.b/c"), None);
        assert_eq!(Language::for_path("rs"), None);
    }

    #[test]
    fn splits_tokens_by_line() {
        let text = "let x = /* a\nb */ 1;\r\n\"s\nt\" y\n";
        let k = |s, e, k| (s, e, k);
        assert_eq!(
            highlight_text(Language::Rust, text, usize::MAX),
            [
                vec![
                    k(0, 3, Kind::Keyword),
                    k(4, 5, Kind::Identifier),
                    k(6, 7, Kind::Operator),
                    k(8, 12, Kind::Comment)
                ],
                vec![
                    k(0, 4, Kind::Comment),
                    k(5, 6, Kind::Number),
                    k(6, 7, Kind::Punctuation)
                ],
                vec![k(0, 2, Kind::String)],
                vec![k(0, 2, Kind::String), k(3, 4, Kind::Identifier)],
            ]
        );
        assert_eq!(highlight_text(Language::Rust, text, 1).len(), 1);
    }

    proptest! {
        /// Lexers must not panic on any text, and their tokens must be in
        /// order and fall on character boundaries.
        #[test]
        fn tokens_are_in_order(
            path in prop::sample::select(PATHS),
            text in prop_oneof![any::<String>(), texts().prop_map(|(t, _)| t)],
        ) {
            let lang = Language::for_path(path).expect("a language for each test path");
            let mut pos = 0;
            for (r, _) in lang.lex(&text).0 {
                prop_assert!(pos <= r.start && r.start < r.end && r.end <= text.len());
                prop_assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
                pos = r.end;
            }
        }

        #[test]
        fn lines_are_covered(
            path in prop::sample::select(PATHS),
            (text, _) in texts(),
        ) {
            let lang = Language::for_path(path).expect("a language for each test path");
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let out = highlight_text(lang, &text, usize::MAX);
            prop_assert_eq!(out.len(), lines.len());
            for (l, spans) in lines.iter().zip(&out) {
                check_spans(l.strip_suffix('\n').unwrap_or(l), spans)?;
            }
        }

        #[test]
        fn every_hunk_line_is_covered(
            path in prop::sample::select(PATHS),
            (old, new) in texts(),
            context in 0u32..4,
        ) {
            let lang = Language::for_path(path).expect("a language for each test path");
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let run = |o: bool, n: bool| {
                let j = job(path, &hunks, o.then_some(&*old), n.then_some(&*new));
                highlight_job(lang, &j)
            };
            let full = run(true, true);
            let parts = run(false, false);
            for (has_old, has_new) in [(true, true), (true, false), (false, true), (false, false)] {
                let hl = run(has_old, has_new);
                prop_assert_eq!(hl.lines.len(), hunks.len());
                for (h, (hunk, spans)) in hunks.iter().zip(&hl.lines).enumerate() {
                    prop_assert_eq!(spans.len(), hunk.lines.len());
                    for (i, (line, spans)) in hunk.lines.iter().zip(spans).enumerate() {
                        check_spans(&line.text, spans)?;
                        // A line with the full text of its side gets the colors
                        // that it gets when both sides have it, and one without
                        // gets the colors that it gets when neither side has it.
                        // Context lines use the new side when they can.
                        let side = match line.kind {
                            LineKind::Del => Some(has_old),
                            LineKind::Add => Some(has_new),
                            LineKind::Context if has_new || !has_old => Some(has_new),
                            LineKind::Context => None,
                        };
                        match side {
                            Some(true) => prop_assert_eq!(spans, &full.lines[h][i]),
                            Some(false) => prop_assert_eq!(spans, &parts.lines[h][i]),
                            None => {}
                        }
                    }
                }
            }
            // A hunk that starts at the top of both files lexes the same
            // lines from the top on its own, so it gets the same colors.
            for (h, hunk) in hunks.iter().enumerate() {
                if hunk.old_start <= 1 && hunk.new_start <= 1 {
                    prop_assert_eq!(&full.lines[h], &parts.lines[h]);
                }
            }
        }
    }
}
