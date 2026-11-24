//! Syntax highlighting for diffs, in the background.
//!
//! Each language has a lexer of its own, written with Logos, and tokens
//! get their colors from what the lexer alone can tell. There is no
//! parser, so we can't tell which identifiers name functions or types.
//! Identifiers are marked as such instead, so that drawing can give each
//! name a color of its own, unless the user turns that off.
//!
//! We always lex the full text of each side of a diff, then pick out the
//! lines the diff shows. So multi-line strings and comments come out
//! right, and a lexer may look ahead across lines to decide what a token
//! is. Lines of a side whose text we don't have get no colors.

mod c;
mod cmake;
mod dockerfile;
mod fish;
mod gitignore;
mod javascript;
mod json;
mod llvm;
mod markdown;
mod python;
mod racket;
mod rust;
mod sh;
mod shrubbery;
mod toml;
mod yaml;
mod zig;

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
    Dockerfile,
    Fish,
    Gitignore,
    /// HipHop.js, which is JavaScript with synchronous reactive
    /// statements.
    HipHop,
    JavaScript,
    Json,
    LlvmIr,
    Markdown,
    Python,
    Racket,
    Rhombus,
    Rust,
    Scribble,
    Shell,
    Toml,
    Yaml,
    Zig,
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
        if matches!(
            name,
            "Dockerfile" | "Containerfile" | "dockerfile" | "containerfile"
        ) {
            return Some(Language::Dockerfile);
        }
        // Startup files of bash and other shells, and Arch Linux and
        // Alpine package builds, which are bash scripts.
        if matches!(
            name,
            ".bashrc"
                | ".bash_profile"
                | ".bash_login"
                | ".bash_logout"
                | ".bash_aliases"
                | ".profile"
                | "PKGBUILD"
                | "APKBUILD"
        ) {
            return Some(Language::Shell);
        }
        if name.ends_with(".hh.js") || name.ends_with(".hh.mjs") {
            return Some(Language::HipHop);
        }
        match name.rsplit_once('.')?.1 {
            "c" => Some(Language::C),
            "fish" => Some(Language::Fish),
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
            "ll" => Some(Language::LlvmIr),
            "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdwn" => Some(Language::Markdown),
            "py" | "pyi" | "pyw" => Some(Language::Python),
            "rkt" | "rktl" | "rktd" => Some(Language::Racket),
            "rhm" => Some(Language::Rhombus),
            "rs" => Some(Language::Rust),
            "scrbl" => Some(Language::Scribble),
            "sh" | "bash" => Some(Language::Shell),
            "toml" => Some(Language::Toml),
            "yaml" | "yml" => Some(Language::Yaml),
            // `.zon` files, such as `build.zig.zon`, hold Zig values.
            "zig" | "zon" => Some(Language::Zig),
            // Such as `dev.Dockerfile`.
            ext if ext.eq_ignore_ascii_case("dockerfile")
                || ext.eq_ignore_ascii_case("containerfile") =>
            {
                Some(Language::Dockerfile)
            }
            // Variants such as `Dockerfile.dev`. `Dockerfile.dockerignore`
            // lists the files that the build of `Dockerfile` ignores.
            ext if (name.starts_with("Dockerfile.") || name.starts_with("Containerfile."))
                && ext != "dockerignore" =>
            {
                Some(Language::Dockerfile)
            }
            _ => None,
        }
    }

    /// The language of a script from its `#!` line, such as
    /// `#!/usr/bin/env python3`, `#!/bin/sh` or `#!/usr/bin/env racket`.
    pub fn for_shebang(line: &str) -> Option<Language> {
        fn base(path: &str) -> &str {
            path.rsplit('/').next().unwrap_or(path)
        }
        let mut words = line.strip_prefix("#!")?.split_whitespace();
        let mut program = base(words.next()?);
        // `env` finds the program on the PATH. Skip its options, such as
        // `-S`, and the variables it sets.
        if program == "env" {
            program = base(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
        }
        match program {
            // The lexer follows bash, but the other shells in the sh
            // family read scripts in much the same way.
            "sh" | "bash" | "dash" | "ash" | "ksh" | "mksh" | "zsh" => {
                return Some(Language::Shell);
            }
            "fish" => return Some(Language::Fish),
            "node" | "nodejs" => return Some(Language::JavaScript),
            // A `#lang` line after the `#!` line can still pick Rhombus or
            // Scribble, as the Racket lexer reads it.
            "racket" | "racketcs" | "racketbc" | "gracket" | "gracket-text" => {
                return Some(Language::Racket);
            }
            // `uv run --script` runs a Python script with the packages
            // it lists at the top.
            "uv" => {
                let script = words.next() == Some("run") && words.any(|w| w == "--script");
                return script.then_some(Language::Python);
            }
            _ => {}
        }
        // Names such as `python3.12`, `pypy3` and the free-threaded
        // `python3.13t`.
        let version = program
            .strip_prefix("python")
            .or_else(|| program.strip_prefix("pypy"))?;
        let version = version.strip_suffix('t').unwrap_or(version);
        version
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.')
            .then_some(Language::Python)
    }

    pub fn lex(self, text: &str) -> Tokens {
        let mut out = Tokens::default();
        match self {
            Language::C | Language::Cpp => c::lex(text, self, &mut out),
            Language::Cmake => cmake::lex(text, &mut out),
            Language::Dockerfile => dockerfile::lex(text, &mut out),
            Language::Fish => fish::lex(text, &mut out),
            Language::Gitignore => gitignore::lex(text, &mut out),
            Language::HipHop | Language::JavaScript => javascript::lex(text, self, &mut out),
            Language::Json => json::lex(text, &mut out),
            Language::LlvmIr => llvm::lex(text, &mut out),
            Language::Markdown => markdown::lex(text, &mut out),
            Language::Python => python::lex(text, &mut out),
            Language::Racket | Language::Rhombus | Language::Scribble => {
                racket::lex(text, self, &mut out);
            }
            Language::Rust => rust::lex(text, &mut out),
            Language::Shell => sh::lex(text, &mut out),
            Language::Toml => toml::lex(text, &mut out),
            Language::Yaml => yaml::lex(text, &mut out),
            Language::Zig => zig::lex(text, &mut out),
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
        // Without the text of either side, no line gets colors.
        if job.old.is_none() && job.new.is_none() {
            continue;
        }
        let Some(lang) = language(&job) else {
            continue;
        };
        let done = done.clone();
        crate::util::blocking(move || done(job.key, highlight_job(lang, &job))).await;
    }
}

/// The language of a job's file, from its name, or else from a `#!`
/// line at the top of either side.
fn language(job: &Job) -> Option<Language> {
    Language::for_path(&job.path).or_else(|| {
        [&job.new, &job.old]
            .into_iter()
            .flatten()
            .find_map(|t| Language::for_shebang(t.lines().next()?))
    })
}

/// The full text of one side of a diff, lexed.
struct Lexed<'a> {
    text: &'a str,
    tokens: Vec<(Range<usize>, Kind)>,
    /// Where each line starts, and then the end of the text.
    starts: Vec<usize>,
}

impl<'a> Lexed<'a> {
    fn new(lang: Language, text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        if starts.last() != Some(&text.len()) {
            starts.push(text.len());
        }
        Lexed {
            text,
            tokens: lang.lex(text).0,
            starts,
        }
    }

    /// The line at `i`, counting from 0, without its newline, and the
    /// byte where it starts.
    fn line(&self, i: usize) -> Option<(usize, &'a str)> {
        let (&start, &next) = (self.starts.get(i)?, self.starts.get(i + 1)?);
        let line = &self.text[start..next];
        Some((start, line.strip_suffix('\n').unwrap_or(line)))
    }

    /// The spans of the tokens in `start..end`, from `start` on.
    fn spans(&self, start: usize, end: usize) -> LineSpans {
        // Tokens don't overlap, so their ends are in order too. A token
        // can cover several lines, so the first one here can start
        // before the line.
        let first = self.tokens.partition_point(|(r, _)| r.end <= start);
        let mut spans = Vec::new();
        for (r, kind) in &self.tokens[first..] {
            if r.start >= end {
                break;
            }
            let (s, e) = (r.start.max(start), r.end.min(end));
            if s < e {
                spans.push(((s - start) as u32, (e - start) as u32, *kind));
            }
        }
        spans
    }

    /// The colors of the line numbered `no`, counting from 1, which the
    /// diff shows as `shown`. The text we read needn't be what git
    /// diffed. The file can change on disk in between, and git compares
    /// the working tree after its filters, such as `core.autocrlf`. So a
    /// line that reads differently gets no colors, apart from a carriage
    /// return at its end that the diff doesn't have.
    fn colors(&self, no: Option<u32>, shown: &str) -> Option<LineSpans> {
        let (start, line) = self.line((no? as usize).checked_sub(1)?)?;
        if line != shown && line.strip_suffix('\r') != Some(shown) {
            return None;
        }
        Some(self.spans(start, start + shown.len()))
    }
}

/// Lex `text` and split its tokens up by line, for tests. Each line's
/// spans leave out its newline.
#[cfg(test)]
fn highlight_text(lang: Language, text: &str) -> Vec<LineSpans> {
    let lexed = Lexed::new(lang, text);
    (0..lexed.starts.len() - 1)
        .filter_map(|i| lexed.line(i))
        .map(|(start, line)| lexed.spans(start, start + line.len()))
        .collect()
}

/// Lex the full text of each side, and pick out the lines the diff shows
/// by their line numbers. Lines of a side whose text we don't have get no
/// colors.
fn highlight_job(lang: Language, job: &Job) -> FileHighlight {
    let old = job.old.as_deref().map(|t| Lexed::new(lang, t));
    let new = job.new.as_deref().map(|t| Lexed::new(lang, t));
    let from_old = |l: &DiffLine| old.as_ref()?.colors(l.old_no, &l.text);
    let from_new = |l: &DiffLine| new.as_ref()?.colors(l.new_no, &l.text);
    let lines = job.hunks.iter().map(|hunk| {
        hunk.lines
            .iter()
            .map(|l| {
                match l.kind {
                    LineKind::Del => from_old(l),
                    LineKind::Add => from_new(l),
                    // Prefer the new side for context, which is what's on
                    // disk.
                    LineKind::Context => from_new(l).or_else(|| from_old(l)),
                }
                .unwrap_or_default()
            })
            .collect()
    });
    FileHighlight {
        lines: lines.collect(),
    }
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
        "Dockerfile",
        "a.fish",
        ".gitignore",
        "a.js",
        "a.hh.js",
        "a.json",
        "a.ll",
        "a.md",
        "a.py",
        "a.rkt",
        "a.rhm",
        "a.rs",
        "a.scrbl",
        "a.sh",
        "a.toml",
        "a.yaml",
        "a.zig",
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
        // Pieces of HipHop.js, such as statements, their JavaScript
        // blocks and `${`.
        pieces.extend([
            "hiphop ",
            "module() ",
            "fork ",
            "} par {",
            "emit S(",
            "await (",
            "S.now",
            "pragma {",
            "async (S) {",
            "} kill {",
            "${",
            "do {",
            "} every (",
            "L: ",
            "break L",
        ]);
        // Pieces of JSON, such as keys and escapes.
        pieces.extend([
            "\"a\": ", "\"a\"", "\\u00e9", "-1.5e3", "01", "null", "[1, 2]",
        ]);
        // Pieces of Zig, such as multiline strings and builtins.
        pieces.extend(["\\\\", "@import", "@\"", "///", ".*", ".?", "0x1p-2"]);
        // Pieces of LLVM IR, such as names, labels and fields.
        pieces.extend([
            "%x", "@\"a", "!0", "!dbg", "entry:", "\"a\":", "line: ", "i32", "cc10", "-0x1.p2",
            "+nan(0x", "#0", "\\0A",
        ]);
        // Pieces of Python, such as f-strings and soft keywords.
        pieces.extend([
            "f\"",
            "rb'",
            "'''",
            "{x!r:>{w}}",
            "{{",
            "}}",
            "\\N{DASH}",
            "match ",
            "case ",
            "type X",
            ":",
            "@",
            "lazy import",
            "...",
            "1j",
            "\\",
        ]);
        // Pieces of shell scripts, such as here-documents, expansions and
        // the words that only mean something in some places.
        pieces.extend([
            "<<EOF",
            "<<-'E'",
            "EOF",
            "\tE",
            "$(",
            "${x:-",
            "$((",
            "((",
            "))",
            "$'",
            "$\"",
            "`",
            "case x in",
            "a)",
            ";;",
            "esac",
            "[[",
            "]]",
            "=~",
            "for i in",
            "do",
            "done",
            "if",
            "then",
            "fi",
            "a=",
            "a[1]=",
            "=(",
            "2>&1",
            "<(",
            "@(",
            "{",
            "}",
            "${ ",
            "\\",
        ]);
        // Pieces of fish scripts, such as blocks, slices, command
        // substitutions and the words that only mean something in some
        // places.
        pieces.extend([
            "begin",
            "end",
            "else if",
            "switch x",
            "case",
            "and ",
            "or ",
            "not ",
            "function f",
            "set -l x",
            "--help",
            "$x[1..-1]",
            "$$x",
            "(",
            ")",
            "$(",
            "{a,b}",
            "{",
            "}",
            "&|",
            ">?",
            "&>",
            "2>&1",
            ">&-",
            "a=b",
            "\\x41",
            "time ",
            "command ",
        ]);
        // Pieces of Dockerfiles, such as instructions, flags, the JSON form
        // of commands and here-documents.
        pieces.extend([
            "FROM ",
            "RUN ",
            "CMD ",
            "COPY ",
            "ENV ",
            "ARG ",
            "LABEL ",
            "ONBUILD ",
            "HEALTHCHECK CMD ",
            "SHELL [\"cmd\"]",
            " AS ",
            "--mount=",
            "--from=",
            "[\"a\", ",
            "\"]",
            "<<EOF",
            "<<-\"EOF\"",
            "EOF",
            "#!/usr/bin/env python3",
            "a=b",
            "${a:-b}",
            "# escape=`",
            "# syntax=x",
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
        assert_eq!(Language::for_path("abro.hh.js"), Some(Language::HipHop));
        assert_eq!(Language::for_path("x/prog.hh.mjs"), Some(Language::HipHop));
        assert_eq!(Language::for_path("hh.js"), Some(Language::JavaScript));
        assert_eq!(Language::for_path("a.hh"), Some(Language::Cpp));
        assert_eq!(Language::for_path(".clang-format"), Some(Language::Yaml));
        assert_eq!(Language::for_path("a/b.json"), Some(Language::Json));
        assert_eq!(Language::for_path("tsconfig.jsonc"), Some(Language::Json));
        assert_eq!(Language::for_path("x/flake.lock"), Some(Language::Json));
        assert_eq!(Language::for_path("src/a.zig"), Some(Language::Zig));
        assert_eq!(Language::for_path("build.zig.zon"), Some(Language::Zig));
        assert_eq!(Language::for_path("test/a.ll"), Some(Language::LlvmIr));
        assert_eq!(Language::for_path("a/b.py"), Some(Language::Python));
        assert_eq!(Language::for_path("a.pyi"), Some(Language::Python));
        assert_eq!(Language::for_path("x/a.sh"), Some(Language::Shell));
        assert_eq!(Language::for_path("a.bash"), Some(Language::Shell));
        assert_eq!(Language::for_path("~/.bashrc"), Some(Language::Shell));
        assert_eq!(Language::for_path("x/PKGBUILD"), Some(Language::Shell));
        assert_eq!(
            Language::for_path("~/.config/fish/config.fish"),
            Some(Language::Fish)
        );
        assert_eq!(
            Language::for_path("x/Dockerfile"),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            Language::for_path("Containerfile"),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            Language::for_path("Dockerfile.alpine"),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            Language::for_path("dev.Dockerfile"),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            Language::for_path("a.containerfile"),
            Some(Language::Dockerfile)
        );
        assert_eq!(Language::for_path("Dockerfile.dockerignore"), None);
        assert_eq!(Language::for_path("dockerfile.rs"), Some(Language::Rust));
        assert_eq!(Language::for_path("a.lock"), None);
        assert_eq!(Language::for_path("a.b/c"), None);
        assert_eq!(Language::for_path("rs"), None);
    }

    #[test]
    fn finds_languages_by_shebang() {
        let cases = [
            ("#!/usr/bin/python", Some(Language::Python)),
            ("#!/usr/bin/python3", Some(Language::Python)),
            ("#!/usr/local/bin/python3.12", Some(Language::Python)),
            ("#! /usr/bin/env python", Some(Language::Python)),
            ("#!/usr/bin/env python3\r", Some(Language::Python)),
            ("#!/usr/bin/env -S python3 -u", Some(Language::Python)),
            (
                "#!/usr/bin/env -S PYTHONPATH=lib python3",
                Some(Language::Python),
            ),
            ("#!/usr/bin/env pypy3", Some(Language::Python)),
            ("#!/usr/bin/python3.13t", Some(Language::Python)),
            ("#!python", Some(Language::Python)),
            ("#!/usr/bin/env -S uv run --script", Some(Language::Python)),
            (
                "#!/usr/bin/env -S uv run --quiet --script",
                Some(Language::Python),
            ),
            ("#!/bin/sh", Some(Language::Shell)),
            ("#!/bin/sh -e", Some(Language::Shell)),
            ("#!/usr/bin/env bash", Some(Language::Shell)),
            ("#!/bin/bash\r", Some(Language::Shell)),
            (
                "#!/usr/bin/env -S bash -euo pipefail",
                Some(Language::Shell),
            ),
            ("#!/bin/dash", Some(Language::Shell)),
            ("#!/bin/ksh", Some(Language::Shell)),
            ("#!/usr/bin/env zsh", Some(Language::Shell)),
            ("#!/usr/bin/env fish", Some(Language::Fish)),
            ("#!/usr/local/bin/fish -n", Some(Language::Fish)),
            ("#!/usr/bin/env node", Some(Language::JavaScript)),
            (
                "#!/usr/bin/env -S node --no-warnings",
                Some(Language::JavaScript),
            ),
            ("#!/usr/bin/nodejs", Some(Language::JavaScript)),
            ("#!/usr/bin/env racket", Some(Language::Racket)),
            ("#! /usr/local/bin/racket -u", Some(Language::Racket)),
            ("#!/usr/bin/racketcs", Some(Language::Racket)),
            ("#!/usr/bin/env gracket", Some(Language::Racket)),
            ("#!/usr/bin/python-config", None),
            ("#!/usr/bin/env -S uv run", None),
            ("#!/usr/bin/env perl", None),
            ("#!/bin/bash5", None),
            ("#!/usr/bin/env", None),
            ("#!", None),
            ("# /usr/bin/python", None),
            (" #!/bin/sh", None),
            ("python", None),
        ];
        for (line, lang) in cases {
            assert_eq!(Language::for_shebang(line), lang, "{line}");
        }
    }

    #[test]
    fn finds_scripts_by_first_line() {
        let script = "#!/usr/bin/env python3\nprint(1)\n";
        let edited = "#!/usr/bin/env python3\nprint(2)\n";
        let lang = |path, old: Option<&str>, new: Option<&str>| {
            let hunks = diff_texts(
                old.unwrap_or("").as_bytes(),
                new.unwrap_or("").as_bytes(),
                3,
            );
            language(&job(path, &hunks, old, new))
        };
        let python = Some(Language::Python);
        assert_eq!(lang("bin/x", Some(script), Some(edited)), python);
        // Only one side has to be a Python script.
        assert_eq!(lang("bin/x", Some(""), Some(script)), python);
        assert_eq!(lang("bin/x", Some(script), Some("")), python);
        assert_eq!(lang("bin/x", Some("x\n"), Some("y\n")), None);
        // The name comes first.
        assert_eq!(
            lang("x.sh", Some(script), Some(edited)),
            Some(Language::Shell)
        );
        // Only the texts we have count, even when a hunk shows the first
        // line of another.
        assert_eq!(lang("bin/x", None, Some(edited)), python);
        assert_eq!(lang("bin/x", Some("x\n"), None), None);
        assert_eq!(lang("bin/x", None, None), None);
        assert_eq!(lang("x.py", None, None), python);
    }

    #[test]
    fn checks_lines_against_the_diff() {
        let old = "/* a\nb */\nc\n";
        let new = "/* a\nb */\nd\n";
        let hunks = diff_texts(old.as_bytes(), new.as_bytes(), 3);
        let comment = |n| vec![(0, n, Kind::Comment)];
        let plain = || vec![(0, 1, Kind::Identifier)];
        let colors = |old: Option<&str>, new: Option<&str>| {
            highlight_job(Language::C, &job("a.c", &hunks, old, new)).lines
        };
        // The lines are "/* a", "b */", "-c" and "+d".
        assert_eq!(
            colors(Some(old), Some(new)),
            [[comment(4), comment(4), plain(), plain()]]
        );
        // The file changed on disk after git diffed it. Its first line
        // reads differently, so it gets no colors, but its second line is
        // the same and still gets the colors from the text we have.
        let changed = "/* x\nb */\nd\n";
        assert_eq!(
            colors(None, Some(changed)),
            [[vec![], comment(4), vec![], plain()]]
        );
        // A context line takes the old side's colors when the new side
        // reads differently.
        assert_eq!(
            colors(Some(old), Some(changed)),
            [[comment(4), comment(4), plain(), plain()]]
        );
        // Lines that the text doesn't have get no colors.
        assert_eq!(
            colors(None, Some("/* a\nb */\n")),
            [[comment(4), comment(4), vec![], vec![]]]
        );
        // The text has carriage returns that `core.autocrlf` took out
        // before git diffed it.
        let crlf = "/* a\r\nb */\r\nd\r\n";
        assert_eq!(
            colors(None, Some(crlf)),
            [[comment(4), comment(4), vec![], plain()]]
        );
    }

    #[test]
    fn splits_tokens_by_line() {
        let text = "let x = /* a\nb */ 1;\r\n\"s\nt\" y\n";
        let k = |s, e, k| (s, e, k);
        assert_eq!(
            highlight_text(Language::Rust, text),
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
            let out = highlight_text(lang, &text);
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
            let old_lines = highlight_text(lang, &old);
            let new_lines = highlight_text(lang, &new);
            for (has_old, has_new) in [(true, true), (true, false), (false, true), (false, false)] {
                let j = job(path, &hunks, has_old.then_some(&*old), has_new.then_some(&*new));
                let hl = highlight_job(lang, &j);
                prop_assert_eq!(hl.lines.len(), hunks.len());
                for (hunk, spans) in hunks.iter().zip(&hl.lines) {
                    prop_assert_eq!(spans.len(), hunk.lines.len());
                    for (line, spans) in hunk.lines.iter().zip(spans) {
                        check_spans(&line.text, spans)?;
                        // A line gets the colors of its line in its side's
                        // full text, and none without that text. Context
                        // lines use the new side when they can.
                        let from_old = || {
                            has_old.then_some(())?;
                            Some(&old_lines[line.old_no? as usize - 1])
                        };
                        let from_new = || {
                            has_new.then_some(())?;
                            Some(&new_lines[line.new_no? as usize - 1])
                        };
                        let want = match line.kind {
                            LineKind::Del => from_old(),
                            LineKind::Add => from_new(),
                            LineKind::Context => from_new().or_else(from_old),
                        };
                        prop_assert_eq!(spans, &want.cloned().unwrap_or_default());
                    }
                }
            }
        }

        /// The texts we read can differ from the ones git diffed, such
        /// as when a file changes on disk in between. Then a line only
        /// gets colors where its side's text reads the same, maybe with
        /// a carriage return at the end, and the colors stay inside the
        /// line.
        #[test]
        fn texts_that_differ_from_the_diff(
            path in prop::sample::select(PATHS),
            (old, new, other) in prop::collection::vec(line(), 1..6)
                .prop_flat_map(|pool| (text(pool.clone()), text(pool.clone()), text(pool))),
            context in 0u32..4,
        ) {
            let lang = Language::for_path(path).expect("a language for each test path");
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let lines: Vec<&str> = other
                .split_inclusive('\n')
                .map(|l| l.strip_suffix('\n').unwrap_or(l))
                .collect();
            let colors = highlight_text(lang, &other);
            let hl = highlight_job(lang, &job(path, &hunks, None, Some(&other)));
            for (hunk, spans) in hunks.iter().zip(&hl.lines) {
                for (line, spans) in hunk.lines.iter().zip(spans) {
                    check_spans(&line.text, spans)?;
                    let i = line.new_no.map(|n| n as usize - 1);
                    let want = match i.and_then(|i| Some((lines.get(i)?, &colors[i]))) {
                        Some((l, c)) if *l == line.text => c.clone(),
                        Some((l, c)) if l.strip_suffix('\r') == Some(line.text.as_str()) => c
                            .iter()
                            .filter(|s| (s.0 as usize) < line.text.len())
                            .map(|&(s, e, k)| (s, e.min(line.text.len() as u32), k))
                            .collect(),
                        _ => vec![],
                    };
                    prop_assert_eq!(spans, &want);
                }
            }
        }
    }
}

/// Writes out the tokens of a list of files, for the scripts in
/// `dev/reference` that compare the lexers with the real ones. It is
/// ignored, so `cargo test` skips it.
///
/// `LEX_FILES` names a file with one path on each line. A line can start
/// with the name of a language and a tab, such as `Cpp\tfoo.h`, to lex
/// the file as that language. Otherwise the language comes from the
/// file's name. The tokens of the file on line `n`, counting from 0, go
/// to `n.tok` in the directory `LEX_OUT`, one token on each line as its
/// start, its end and its kind, such as `4 9 Keyword`. The offsets are in
/// bytes. A file that isn't UTF-8 gets no `.tok` file.
#[cfg(test)]
mod reference {
    use super::Language;

    const LANGUAGES: [Language; 20] = [
        Language::C,
        Language::Cmake,
        Language::Cpp,
        Language::Dockerfile,
        Language::Fish,
        Language::Gitignore,
        Language::HipHop,
        Language::JavaScript,
        Language::Json,
        Language::LlvmIr,
        Language::Markdown,
        Language::Python,
        Language::Racket,
        Language::Rhombus,
        Language::Rust,
        Language::Scribble,
        Language::Shell,
        Language::Toml,
        Language::Yaml,
        Language::Zig,
    ];

    #[test]
    #[ignore]
    fn dump_tokens() {
        use std::fmt::Write;
        let var = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let list = std::fs::read_to_string(var("LEX_FILES")).expect("reading LEX_FILES");
        let out = std::path::PathBuf::from(var("LEX_OUT"));
        std::fs::create_dir_all(&out).expect("creating LEX_OUT");
        for (n, line) in list.lines().enumerate() {
            let (lang, path) = match line.split_once('\t') {
                Some((name, path)) => {
                    let lang = LANGUAGES
                        .into_iter()
                        .find(|l| format!("{l:?}") == name)
                        .unwrap_or_else(|| panic!("no language named {name}"));
                    (lang, path)
                }
                None => {
                    let lang = Language::for_path(line)
                        .unwrap_or_else(|| panic!("no language for {line}"));
                    (lang, line)
                }
            };
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let mut s = String::new();
            for (r, k) in &lang.lex(&text).0 {
                writeln!(s, "{} {} {k:?}", r.start, r.end).expect("writing to a string");
            }
            std::fs::write(out.join(format!("{n}.tok")), s).expect("writing a .tok file");
        }
    }
}
