//! A lexer for Dockerfiles, which podman and buildah call Containerfiles.
//!
//! It follows BuildKit's Dockerfile parser in `frontend/dockerfile/parser`,
//! and the way BuildKit then reads the arguments of each instruction. A
//! line that ends with the escape character goes on on the next line, and
//! BuildKit skips comment lines and blank lines inside such an
//! instruction. So we gather the lines of an instruction first, and then
//! lex its text without them.
//!
//! The shell commands of `RUN`, `CMD`, `ENTRYPOINT` and `HEALTHCHECK` are
//! lexed with the shell lexer, along with the bodies of the
//! here-documents that a `RUN` reads. When a `RUN` is just one
//! here-document, BuildKit runs its body as a script, so we lex the body as
//! shell code, or in the language that its `#!` line names. Commands in
//! the JSON form, such as `CMD ["nginx", "-g", "daemon off;"]`, are lexed
//! as JSON.
//!
//! BuildKit joins the lines of an instruction with nothing between them,
//! so a word can go on on the next line, and so do we.

use std::ops::Range;

use super::{Kind, Language, Tokens};

/// The colored pieces of a text, in no particular order.
type Out = Vec<(Range<usize>, Kind)>;

/// The instructions that BuildKit knows, in lower case.
const INSTRUCTIONS: &[&str] = &[
    "add",
    "arg",
    "cmd",
    "copy",
    "entrypoint",
    "env",
    "expose",
    "from",
    "healthcheck",
    "label",
    "maintainer",
    "onbuild",
    "run",
    "shell",
    "stopsignal",
    "user",
    "volume",
    "workdir",
];

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lexer = Lexer::new(text);
    lexer.run();
    let mut toks = lexer.out;
    toks.sort_by_key(|(r, _)| r.start);
    for (r, kind) in toks {
        out.push(r, kind);
    }
}

/// A line of the source, without its line break or the carriage returns
/// before it.
#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    end: usize,
}

/// Pieces of the source joined into one text, so that a lexer can read
/// them together. Each piece remembers where it came from, and text
/// between the pieces, such as the line feeds that join lines, comes from
/// nowhere.
#[derive(Default)]
struct Text {
    s: String,
    /// Where each piece starts in `s`, where it starts in the source, and
    /// its length.
    pieces: Vec<(usize, usize, usize)>,
}

impl Text {
    fn push_src(&mut self, src: &str, r: Range<usize>) {
        if !r.is_empty() {
            self.pieces.push((self.s.len(), r.start, r.len()));
            self.s.push_str(&src[r]);
        }
    }

    /// Color the source for a range of the text, one piece at a time.
    fn map(&self, r: Range<usize>, kind: Kind, out: &mut Out) {
        let first = self
            .pieces
            .partition_point(|&(at, _, len)| at + len <= r.start);
        for &(at, src, len) in &self.pieces[first..] {
            if at >= r.end {
                break;
            }
            let s = r.start.max(at);
            let e = r.end.min(at + len);
            if s < e {
                out.push((src + s - at..src + e - at, kind));
            }
        }
    }

    /// Lex the text from `from` on in `lang`, and color the source.
    fn lex(&self, lang: Language, from: usize, out: &mut Out) {
        for (r, kind) in lang.lex(&self.s[from..]).0 {
            self.map(r.start + from..r.end + from, kind, out);
        }
    }
}

/// A here-document, as `heredocsFromLine` and `ParseHeredoc` find it.
#[derive(Debug)]
struct Heredoc {
    /// The whole word, such as `<<-"EOF"` or `<< EOF`.
    word: Range<usize>,
    /// The `<<` or `<<-`, after the number of a file descriptor.
    op: Range<usize>,
    /// Where the delimiter starts, after the operator and any spaces.
    delimiter: usize,
    /// The delimiter without its quotes, which ends the body on a line of
    /// its own.
    name: String,
    /// Whether it came after `<<-`, which removes tabs at the start of
    /// each line.
    strip_tabs: bool,
    /// Whether no part of the delimiter was quoted. Then BuildKit expands
    /// variables in the body.
    expand: bool,
}

/// The lines of a here-document's body, and the line that ends it if
/// there is one.
struct Body {
    lines: Range<usize>,
    end: Option<usize>,
}

struct Lexer<'a> {
    src: &'a str,
    lines: Vec<Line>,
    /// The escape character, `\` or `` ` ``.
    escape: u8,
    /// Whether `RUN` and the others run their commands in a shell of the
    /// sh family.
    sh: bool,
    out: Out,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        let mut lines = Vec::new();
        // BuildKit skips a byte order mark at the start of the file.
        let mut start = if src.starts_with('\u{feff}') { 3 } else { 0 };
        for line in src[start..].split_inclusive('\n') {
            let content = line.strip_suffix('\n').unwrap_or(line);
            // BuildKit removes all carriage returns at the end of a line.
            let content = content.trim_end_matches('\r');
            lines.push(Line {
                start,
                end: start + content.len(),
            });
            start += line.len();
        }
        Lexer {
            src,
            lines,
            escape: b'\\',
            sh: true,
            out: Vec::new(),
        }
    }

    fn run(&mut self) {
        let mut i = 0;
        while i < self.lines.len() && self.directive(self.lines[i]) {
            i += 1;
        }
        self.sh = self.default_sh();
        while i < self.lines.len() {
            let line = self.lines[i];
            let t = self.src[line.start..line.end].trim_start();
            if t.is_empty() {
                i += 1;
            } else if t.starts_with('#') {
                self.comment(line);
                i += 1;
            } else {
                i = self.instruction(i);
            }
        }
    }

    /// Windows images run commands with `cmd` unless a `SHELL`
    /// instruction says otherwise. We can't tell what platform an image is
    /// for, but Dockerfiles for Windows almost always choose `` ` `` as
    /// their escape character, so that `\` can separate the parts of a
    /// path.
    fn default_sh(&self) -> bool {
        self.escape == b'\\'
    }

    /// Color a parser directive such as `# syntax=docker/dockerfile:1`,
    /// and return whether the line is one. BuildKit only reads them at the
    /// top of the file, and stops at the first line that isn't one, even
    /// a blank line or a directive it doesn't know.
    fn directive(&mut self, line: Line) -> bool {
        let s = &self.src[line.start..line.end];
        let at = |t: &str| line.end - t.len();
        let t = s.trim_start();
        let Some(rest) = t.strip_prefix('#') else {
            return false;
        };
        let hash = at(t);
        let rest = rest.trim_start_matches([' ', '\t']);
        let name_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        let name = &rest[..name_len];
        if !["syntax", "escape", "check"]
            .iter()
            .any(|d| d.eq_ignore_ascii_case(name))
        {
            return false;
        }
        let name_at = at(rest);
        let rest = rest[name_len..].trim_start_matches([' ', '\t']);
        let Some(value) = rest.strip_prefix('=') else {
            return false;
        };
        let eq = at(rest);
        let value = value.trim_matches([' ', '\t']);
        if value.is_empty() {
            return false;
        }
        let value_at = at(rest[1..].trim_start_matches([' ', '\t']));
        if name.eq_ignore_ascii_case("escape") && (value == "\\" || value == "`") {
            self.escape = value.as_bytes()[0];
        }
        self.out.push((hash..hash + 1, Kind::Attribute));
        self.out
            .push((name_at..name_at + name_len, Kind::Attribute));
        self.out.push((eq..eq + 1, Kind::Operator));
        self.out
            .push((value_at..value_at + value.len(), Kind::String));
        true
    }

    fn comment(&mut self, line: Line) {
        let s = &self.src[line.start..line.end];
        let start = line.end - s.trim_start().len();
        let end = line.start + s.trim_end().len();
        self.out.push((start..end, Kind::Comment));
    }

    /// Lex the instruction that starts on line `i`, along with the bodies
    /// of its here-documents, and return the line after them.
    fn instruction(&mut self, i: usize) -> usize {
        // The instruction's lines joined with nothing between them, without
        // the escape characters that join them.
        let mut ins = Text::default();
        let mut next = i;
        while next < self.lines.len() {
            let line = self.lines[next];
            let first = next == i;
            next += 1;
            let s = &self.src[line.start..line.end];
            let t = s.trim_start();
            if !first && t.starts_with('#') {
                self.comment(line);
                continue;
            }
            if !first && t.is_empty() {
                continue;
            }
            // The first line starts at its first word. Later lines keep
            // their indentation.
            let start = if first {
                line.end - t.len()
            } else {
                line.start
            };
            let content = self.src[start..line.end].trim_end_matches([' ', '\t']);
            // BuildKit's pattern for this wants something other than the
            // escape character before it, so `a\\` at the end of a line
            // doesn't go on.
            let b = content.as_bytes();
            let more =
                b.last() == Some(&self.escape) && (b.len() == 1 || b[b.len() - 2] != self.escape);
            let end = if more {
                let e = start + content.len() - 1;
                self.out.push((e..e + 1, Kind::Escape));
                e
            } else {
                line.end
            };
            ins.push_src(self.src, start..end);
            if !more {
                break;
            }
        }
        let mut toks = Vec::new();
        let next = self.command(&ins, 0, next, &mut toks);
        for (r, kind) in toks {
            ins.map(r, kind, &mut self.out);
        }
        next
    }

    /// Lex the instruction that starts at `at` in the text. Its
    /// here-documents start on line `next`. Return the line after them.
    fn command(&mut self, ins: &Text, at: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.s;
        let start = skip_blanks(s, at);
        let mut end = word_end(s, start);
        // BuildKit trims all whitespace from the ends of an instruction, so
        // a name can end in a space such as U+00A0 when nothing follows.
        if s[end..].trim().is_empty() {
            end = start + s[start..end].trim_end().len();
        }
        let name = s[start..end].to_ascii_lowercase();
        if !INSTRUCTIONS.contains(&name.as_str()) {
            return next;
        }
        toks.push((start..end, Kind::Keyword));
        let a = flags(s, end, self.escape, toks, |s, r, toks| {
            self.word(s, r, toks);
        });
        // BuildKit trims all whitespace from the start of the arguments,
        // even U+00A0.
        let a = s.len() - s[a..].trim_start().len();
        self.arguments(ins, &name, a, next, toks)
    }

    fn arguments(
        &mut self,
        ins: &Text,
        name: &str,
        a: usize,
        next: usize,
        toks: &mut Out,
    ) -> usize {
        let s = &ins.s;
        match name {
            "run" => return self.run_command(ins, a, next, toks),
            "cmd" | "entrypoint" => self.shell_command(ins, a, &[]),
            "healthcheck" => {
                let start = skip_blanks(s, a);
                let end = word_end(s, start);
                let kind = &s[start..end];
                if kind.eq_ignore_ascii_case("cmd") {
                    toks.push((start..end, Kind::Keyword));
                    self.shell_command(ins, skip_blanks(s, end), &[]);
                } else if kind.eq_ignore_ascii_case("none") {
                    toks.push((start..end, Kind::Keyword));
                }
            }
            // `ONBUILD` holds another instruction, which runs when another
            // image builds on this one.
            "onbuild" => return self.command(ins, a, next, toks),
            "from" => {
                // Each stage starts with the shell of its image, which we
                // take to be the usual one.
                self.sh = self.default_sh();
                let words = blank_words(s, a);
                let stage = words
                    .get(1)
                    .is_some_and(|w| s[w.clone()].eq_ignore_ascii_case("as"));
                for (n, w) in words.into_iter().enumerate() {
                    match n {
                        0 => self.name(s, w, toks),
                        1 if stage => toks.push((w, Kind::Keyword)),
                        2 if stage => self.name(s, w, toks),
                        _ => self.word(s, w, toks),
                    }
                }
            }
            "arg" => {
                for w in quoted_words(s, a, self.escape) {
                    self.key_value(s, w, toks);
                }
            }
            "env" | "label" => {
                let words = quoted_words(s, a, self.escape);
                if let Some(first) = words.first()
                    && !s[first.clone()].contains('=')
                {
                    // The old form, `ENV name value`, sets one variable to
                    // the rest of the line.
                    let key = a..word_end(s, a);
                    let value = skip_blanks(s, key.end);
                    self.name(s, key, toks);
                    self.word(s, value..s.len(), toks);
                } else {
                    for w in words {
                        self.key_value(s, w, toks);
                    }
                }
            }
            "expose" => {
                for w in blank_words(s, a) {
                    if self.plain(&s[w.clone()]) {
                        numbers(s, w, toks);
                    } else {
                        self.word(s, w, toks);
                    }
                }
            }
            "copy" | "add" => return self.copy(ins, a, next, toks),
            "volume" => {
                if json_form(s, a) {
                    ins.lex(Language::Json, a, &mut self.out);
                } else {
                    for w in blank_words(s, a) {
                        self.word(s, w, toks);
                    }
                }
            }
            "shell" => {
                if json_form(s, a) {
                    ins.lex(Language::Json, a, &mut self.out);
                    self.sh = runs_sh(&s[a..]);
                }
            }
            "stopsignal" | "user" | "workdir" => self.word(s, a..s.len(), toks),
            _ => {}
        }
        next
    }

    /// Lex the command of a `RUN`, and the bodies of its here-documents.
    fn run_command(&mut self, ins: &Text, a: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.s;
        if json_form(s, a) {
            ins.lex(Language::Json, a, &mut self.out);
            return next;
        }
        let docs = heredocs(s, a);
        let (bodies, next) = self.bodies(&docs, next);
        // When the command is just one here-document, BuildKit runs its
        // body as a script, with the program its `#!` line names if it has
        // one. Otherwise the shell reads the command and the bodies.
        let script = matches!(&docs[..], [doc] if s[a..].trim_end() == &s[doc.word.clone()]);
        if self.sh && !script {
            self.shell_command(ins, a, &bodies);
        } else {
            for doc in &docs {
                heredoc_word(doc, toks);
            }
        }
        if script {
            let (doc, body) = (&docs[0], &bodies[0]);
            let first = body
                .lines
                .clone()
                .next()
                .map(|l| &self.src[self.body_line(l, doc)]);
            let lang = match first {
                Some(l) if l.starts_with("#!") => Language::for_shebang(l),
                _ => self.sh.then_some(Language::Shell),
            };
            if let Some(lang) = lang {
                self.body_in(body, doc, lang);
            }
            self.body_end(body, doc);
        } else if !self.sh {
            for (doc, body) in docs.iter().zip(&bodies) {
                self.body_end(body, doc);
            }
        }
        next
    }

    /// Lex a command in the shell form, from `a` on, followed by the
    /// bodies of its here-documents, or in the JSON form.
    fn shell_command(&mut self, ins: &Text, a: usize, bodies: &[Body]) {
        let s = &ins.s;
        if a >= s.len() {
            return;
        }
        if json_form(s, a) {
            ins.lex(Language::Json, a, &mut self.out);
            return;
        }
        if !self.sh {
            return;
        }
        // The shell gets the joined lines, and the bodies after them.
        let mut text = Text::default();
        for &(at, src, len) in &ins.pieces {
            let start = a.max(at);
            if start < at + len {
                text.push_src(self.src, src + start - at..src + len);
            }
        }
        text.s.push('\n');
        for body in bodies {
            for l in body.lines.clone().chain(body.end) {
                let line = self.lines[l];
                text.push_src(self.src, line.start..line.end);
                text.s.push('\n');
            }
        }
        text.lex(Language::Shell, 0, &mut self.out);
    }

    /// Lex the arguments of a `COPY` or an `ADD`, and the bodies of its
    /// here-documents, which are the contents of files.
    fn copy(&mut self, ins: &Text, a: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.s;
        if json_form(s, a) {
            ins.lex(Language::Json, a, &mut self.out);
            return next;
        }
        let docs = heredocs(s, a);
        let words = blank_words(s, a);
        // BuildKit splits the arguments at blanks, and only copies a
        // here-document for a word that is one on its own. So in
        // `COPY << EOF /x`, the body is lost, and `<<` and `EOF` are files.
        for w in &words {
            match docs.iter().find(|d| d.word == *w) {
                Some(doc) => heredoc_word(doc, toks),
                None => self.word(s, w.clone(), toks),
            }
        }
        let (bodies, next) = self.bodies(&docs, next);
        // A single file whose name we know the language of, as in
        // `COPY <<EOF /app/main.py`, gets lexed in that language.
        let lang = match (&docs[..], words.last()) {
            ([doc], Some(dest)) if *dest != doc.word && self.plain(&s[dest.clone()]) => {
                Language::for_path(&s[dest.clone()])
            }
            _ => None,
        };
        for (doc, body) in docs.iter().zip(&bodies) {
            match lang {
                Some(lang) => self.body_in(body, doc, lang),
                None => {
                    for l in body.lines.clone() {
                        self.text_body(self.body_line(l, doc), doc.expand);
                    }
                }
            }
            self.body_end(body, doc);
        }
        next
    }

    /// Find the bodies of the here-documents, starting on line `next`, and
    /// return them and the line after them.
    fn bodies(&self, docs: &[Heredoc], mut next: usize) -> (Vec<Body>, usize) {
        let mut bodies = Vec::new();
        for doc in docs {
            let start = next;
            let mut end = None;
            while next < self.lines.len() {
                let mut line = self.line(next);
                next += 1;
                if doc.strip_tabs {
                    line = line.trim_start_matches('\t');
                }
                if line == doc.name {
                    end = Some(next - 1);
                    break;
                }
            }
            bodies.push(Body {
                lines: start..end.unwrap_or(next),
                end,
            });
        }
        (bodies, next)
    }

    fn line(&self, i: usize) -> &'a str {
        let line = self.lines[i];
        &self.src[line.start..line.end]
    }

    /// Line `l` of a here-document's body. After `<<-`, BuildKit removes
    /// the tabs at its start.
    fn body_line(&self, l: usize, doc: &Heredoc) -> Range<usize> {
        let line = self.lines[l];
        let s = self.line(l);
        let tabs = if doc.strip_tabs {
            s.len() - s.trim_start_matches('\t').len()
        } else {
            0
        };
        line.start + tabs..line.end
    }

    /// Lex the lines of a body in `lang`.
    fn body_in(&mut self, body: &Body, doc: &Heredoc, lang: Language) {
        let mut text = Text::default();
        for l in body.lines.clone() {
            text.push_src(self.src, self.body_line(l, doc));
            text.s.push('\n');
        }
        text.lex(lang, 0, &mut self.out);
    }

    /// Color the line that ends a here-document.
    fn body_end(&mut self, body: &Body, doc: &Heredoc) {
        if let Some(l) = body.end {
            let line = self.lines[l];
            let s = self.line(l);
            let start = if doc.strip_tabs {
                line.end - s.trim_start_matches('\t').len()
            } else {
                line.start
            };
            self.out.push((start..line.end, Kind::Attribute));
        }
    }

    /// Color a line of a here-document that holds text. Unless the
    /// delimiter was quoted, BuildKit expands the variables in it and
    /// removes the `\` from escapes, but leaves quotes alone.
    fn text_body(&mut self, r: Range<usize>, expand: bool) {
        if !expand {
            self.out.push((r, Kind::String));
            return;
        }
        let mut toks = Vec::new();
        let mut w = Word {
            s: self.src,
            b: self.src.as_bytes(),
            end: r.end,
            escape: b'\\',
            quotes: false,
            toks: &mut toks,
        };
        w.run(r.start, b"", Some(Kind::String));
        self.out.extend(toks);
    }

    /// Whether a word has nothing in it that BuildKit expands or removes.
    fn plain(&self, w: &str) -> bool {
        !w.bytes()
            .any(|b| matches!(b, b'$' | b'\'' | b'"') || b == self.escape)
    }

    /// Color a word that names something, such as a variable or a stage.
    fn name(&self, s: &str, r: Range<usize>, toks: &mut Out) {
        if self.plain(&s[r.clone()]) {
            toks.push((r, Kind::Identifier));
        } else {
            self.word(s, r, toks);
        }
    }

    /// Color a word such as `name=value` from `ENV`, `LABEL` or `ARG`.
    /// BuildKit splits it at the first `=`, even one in quotes.
    fn key_value(&self, s: &str, w: Range<usize>, toks: &mut Out) {
        match s[w.clone()].find('=') {
            Some(n) => {
                let eq = w.start + n;
                self.name(s, w.start..eq, toks);
                toks.push((eq..eq + 1, Kind::Operator));
                self.word(s, eq + 1..w.end, toks);
            }
            None => self.name(s, w, toks),
        }
    }

    /// Color the quotes, escapes and variables in `s[r]`, as
    /// `ProcessWord` in BuildKit's `shell` package reads them.
    fn word(&self, s: &str, r: Range<usize>, toks: &mut Out) {
        let mut w = Word {
            s,
            b: s.as_bytes(),
            end: r.end,
            escape: self.escape,
            quotes: true,
            toks,
        };
        w.run(r.start, b"", None);
    }
}

/// Reads quotes, escapes and variables in a word, as `ProcessWord` in
/// BuildKit's `shell` package does.
struct Word<'a, 'o> {
    s: &'a str,
    b: &'a [u8],
    end: usize,
    /// The escape character, `\\` or `` ` ``.
    escape: u8,
    /// Whether quotes are special, as they are everywhere but in the body
    /// of a here-document.
    quotes: bool,
    toks: &'o mut Out,
}

impl Word<'_, '_> {
    fn at(&self, i: usize) -> Option<u8> {
        (i < self.end).then(|| self.b[i])
    }

    /// The length of the character at `i`, which may be cut short by the
    /// end.
    fn char_len(&self, i: usize) -> usize {
        let n = self.b[i + 1..self.end]
            .iter()
            .take_while(|&&c| (0x80..0xc0).contains(&c))
            .count();
        n + 1
    }

    /// Read up to the end or a byte in `stops`, and return where we
    /// stopped. Text that isn't quoted, escaped or a variable gets `base`.
    fn run(&mut self, mut i: usize, stops: &[u8], base: Option<Kind>) -> usize {
        while let Some(c) = self.at(i) {
            if stops.contains(&c) {
                break;
            }
            i = match c {
                b'\'' if self.quotes => self.single_quote(i),
                b'"' if self.quotes => self.double_quote(i),
                b'$' => self.dollar(i, base),
                c if c == self.escape => {
                    if i + 1 < self.end {
                        let n = 1 + self.char_len(i + 1);
                        self.toks.push((i..i + n, Kind::Escape));
                        i + n
                    } else {
                        i + 1
                    }
                }
                _ => {
                    let n = self.char_len(i);
                    if let Some(k) = base {
                        self.toks.push((i..i + n, k));
                    }
                    i + n
                }
            };
        }
        i
    }

    fn single_quote(&mut self, i: usize) -> usize {
        let end = self.b[i + 1..self.end]
            .iter()
            .position(|&c| c == b'\'')
            .map_or(self.end, |n| i + n + 2);
        self.toks.push((i..end, Kind::String));
        end
    }

    /// In double quotes, the escape character only escapes `"`, `$` and
    /// itself.
    fn double_quote(&mut self, i: usize) -> usize {
        self.toks.push((i..i + 1, Kind::String));
        let mut i = i + 1;
        while let Some(c) = self.at(i) {
            match c {
                b'"' => {
                    self.toks.push((i..i + 1, Kind::String));
                    return i + 1;
                }
                b'$' => i = self.dollar(i, Some(Kind::String)),
                c if c == self.escape
                    && self
                        .at(i + 1)
                        .is_some_and(|n| n == b'"' || n == b'$' || n == self.escape) =>
                {
                    self.toks.push((i..i + 2, Kind::Escape));
                    i += 2;
                }
                _ => {
                    let n = self.char_len(i);
                    self.toks.push((i..i + n, Kind::String));
                    i += n;
                }
            }
        }
        i
    }

    /// Read what a `$` starts. A `$` that doesn't start a variable gets
    /// `base`.
    fn dollar(&mut self, i: usize, base: Option<Kind>) -> usize {
        if self.at(i + 1) == Some(b'{') {
            return self.braces(i, base);
        }
        let n = name_len(&self.s[i + 1..self.end]);
        if n == 0 {
            if let Some(k) = base {
                self.toks.push((i..i + 1, k));
            }
            return i + 1;
        }
        self.toks.push((i..i + 1, Kind::Punctuation));
        self.toks.push((i + 1..i + 1 + n, Kind::Identifier));
        i + 1 + n
    }

    /// Read `${name}`, or `${name` followed by an operator such as `:-`
    /// and a word.
    fn braces(&mut self, i: usize, base: Option<Kind>) -> usize {
        self.toks.push((i..i + 2, Kind::Punctuation));
        let mut i = i + 2;
        let n = name_len(&self.s[i..self.end]);
        self.toks.push((i..i + n, Kind::Identifier));
        i += n;
        let rest = &self.b[i..self.end];
        let op = match rest {
            [b'}', ..] => {
                self.toks.push((i..i + 1, Kind::Punctuation));
                return i + 1;
            }
            [b':', b'+' | b'-' | b'?', ..]
            | [b'#', b'#', ..]
            | [b'%', b'%', ..]
            | [b'/', b'/', ..] => 2,
            [b'+' | b'-' | b'?' | b'#' | b'%' | b'/', ..] => 1,
            // BuildKit reports anything else as an error.
            _ => return i,
        };
        self.toks.push((i..i + op, Kind::Operator));
        let replace = rest[op - 1] == b'/';
        i += op;
        if replace {
            i = self.run(i, b"/}", base);
            if self.at(i) == Some(b'/') {
                self.toks.push((i..i + 1, Kind::Operator));
                i += 1;
            }
        }
        i = self.run(i, b"}", base);
        if self.at(i) == Some(b'}') {
            self.toks.push((i..i + 1, Kind::Punctuation));
            i += 1;
        }
        i
    }
}

/// The length of the name of a variable at the start of `b`, as
/// `processName` reads it. A name that starts with a digit is all digits,
/// and some special parameters are one character long.
fn name_len(s: &str) -> usize {
    match s.bytes().next() {
        Some(c) if c.is_ascii_digit() => s.bytes().take_while(u8::is_ascii_digit).count(),
        Some(b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!') => 1,
        _ => s
            .chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_')
            .map(char::len_utf8)
            .sum(),
    }
}

/// The whitespace that BuildKit splits on.
fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | 0x0b | 0x0c)
}

fn skip_blanks(s: &str, i: usize) -> usize {
    i + s.as_bytes()[i..]
        .iter()
        .take_while(|&&c| is_blank(c))
        .count()
}

fn word_end(s: &str, i: usize) -> usize {
    i + s.as_bytes()[i..]
        .iter()
        .take_while(|&&c| !is_blank(c))
        .count()
}

/// The words of the arguments from `a` on, split at whitespace even in
/// quotes, as `parseStringsWhitespaceDelimited` splits them.
fn blank_words(s: &str, a: usize) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut i = skip_blanks(s, a);
    while i < s.len() {
        let end = word_end(s, i);
        words.push(i..end);
        i = skip_blanks(s, end);
    }
    words
}

/// The end of the word that starts at `i`, where whitespace in quotes and
/// escaped whitespace don't end it. This is how `parseWords` finds words.
fn quoted_word_end(s: &str, mut i: usize, escape: u8) -> usize {
    let b = s.as_bytes();
    let mut quote = None;
    while i < b.len() {
        let c = b[i];
        match quote {
            None if is_blank(c) => break,
            None if c == b'\'' || c == b'"' => quote = Some(c),
            Some(q) if c == q => quote = None,
            _ if c == escape && quote != Some(b'\'') => i += 1,
            _ => {}
        }
        i += 1;
    }
    i.min(b.len())
}

/// The words of the arguments from `a` on, as `parseWords` splits them for
/// `ENV`, `LABEL` and `ARG`.
fn quoted_words(s: &str, a: usize, escape: u8) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut i = skip_blanks(s, a);
    while i < s.len() {
        let end = quoted_word_end(s, i, escape);
        words.push(i..end);
        i = skip_blanks(s, end);
    }
    words
}

/// Color the flags after an instruction's name, such as
/// `--mount=type=cache,target=/root/.cache`, and return where its
/// arguments start. `extractBuilderFlags` reads them the same way for
/// every instruction.
fn flags(
    s: &str,
    mut i: usize,
    escape: u8,
    toks: &mut Out,
    mut value: impl FnMut(&str, Range<usize>, &mut Out),
) -> usize {
    loop {
        i = skip_blanks(s, i);
        if !s[i..].starts_with("--") {
            return i;
        }
        let end = flag_end(s, i, escape);
        // A `--` on its own ends the flags.
        if end == i + 2 {
            return skip_blanks(s, end);
        }
        let w = &s[i..end];
        match w.find('=') {
            Some(n) => {
                let eq = i + n;
                toks.push((i..eq, Kind::Attribute));
                toks.push((eq..eq + 1, Kind::Operator));
                // `--from` names a stage or an image, as `FROM` does.
                let v = eq + 1..end;
                if &s[i..eq] == "--from"
                    && !s[v.clone()].contains(['$', '\'', '"', char::from(escape)])
                {
                    toks.push((v, Kind::Identifier));
                } else {
                    value(s, v, toks);
                }
            }
            None => toks.push((i..end, Kind::Attribute)),
        }
        i = end;
    }
}

/// The end of the flag that starts at `i`. Unlike in other words, the
/// escape character escapes the next character even in single quotes.
fn flag_end(s: &str, mut i: usize, escape: u8) -> usize {
    let b = s.as_bytes();
    let mut quote = None;
    while i < b.len() {
        let c = b[i];
        match quote {
            None if is_blank(c) => break,
            None if c == b'\'' || c == b'"' => quote = Some(c),
            Some(q) if c == q => quote = None,
            _ if c == escape => i += 1,
            _ => {}
        }
        i += 1;
    }
    i.min(b.len())
}

/// Color the numbers in a word from `EXPOSE`, such as `8080/tcp`.
fn numbers(s: &str, r: Range<usize>, toks: &mut Out) {
    let b = s.as_bytes();
    let mut i = r.start;
    while i < r.end {
        let n = b[i..r.end]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if n > 0 {
            toks.push((i..i + n, Kind::Number));
            i += n;
        } else {
            i += 1;
        }
    }
}

/// Whether the arguments at `a` are in the JSON form, such as
/// `["a", "b"]`. BuildKit tries to read them as a JSON array of strings,
/// and takes them as a shell command when that fails.
fn json_form(s: &str, a: usize) -> bool {
    let a = skip_blanks(s, a);
    let b = s.as_bytes();
    let ws = |mut i: usize| {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r') {
            i += 1;
        }
        i
    };
    // BuildKit trims all whitespace from the end of the arguments, and
    // JSON allows its own whitespace before that.
    let end = |i: usize| s[i..].trim().is_empty();
    if b.get(a) != Some(&b'[') {
        return false;
    }
    let mut i = ws(a + 1);
    if b.get(i) == Some(&b']') {
        return end(i + 1);
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return false;
        }
        i += 1;
        loop {
            let Some(&c) = b.get(i) else { return false };
            match c {
                b'"' => break,
                b'\\' => {
                    i += 1;
                    match b.get(i) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {}
                        Some(b'u') => {
                            if !b
                                .get(i + 1..i + 5)
                                .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
                            {
                                return false;
                            }
                            i += 4;
                        }
                        _ => return false,
                    }
                }
                c if c < 0x20 => return false,
                _ => {}
            }
            i += 1;
        }
        i = ws(i + 1);
        match b.get(i) {
            Some(b',') => i = ws(i + 1),
            Some(b']') => return end(i + 1),
            _ => return false,
        }
    }
}

/// Whether a `SHELL` instruction's JSON array names a shell of the sh
/// family.
fn runs_sh(json: &str) -> bool {
    let mut words = Vec::new();
    let mut rest = json;
    while let Some(start) = rest.find('"') {
        let mut word = String::new();
        let mut chars = rest[start + 1..].char_indices();
        let mut end = rest.len() - start - 1;
        while let Some((n, c)) = chars.next() {
            match c {
                '"' => {
                    end = n + 1;
                    break;
                }
                '\\' => {
                    if let Some((_, c)) = chars.next() {
                        word.push(c);
                    }
                }
                c => word.push(c),
            }
        }
        words.push(word);
        rest = &rest[start + 1 + end..];
    }
    Language::for_shebang(&format!("#!{}", words.join(" "))) == Some(Language::Shell)
}

/// Find the here-documents in the arguments from `a` on. As
/// `heredocsFromLine` does, we split them into words and take each word
/// such as `<<EOF`, `<<-EOF`, `<<"EOF"` or `<< EOF` as one. BuildKit's
/// `shell` package keeps the spaces after `<<` in the word. In `cat<<EOF`
/// there is none, because the word doesn't start with `<<`.
fn heredocs(s: &str, a: usize) -> Vec<Heredoc> {
    let b = s.as_bytes();
    let mut docs = Vec::new();
    // BuildKit reads the instruction with its `shell` package to find the
    // here-documents, and finds none if that fails.
    if !shell_reads(s) {
        return docs;
    }
    for w in heredoc_words(s, a) {
        let op = w.start
            + b[w.clone()]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
        if !s[op..w.end].starts_with("<<") || forgets_start(&s[w.clone()]) {
            continue;
        }
        let strip_tabs = b.get(op + 2) == Some(&b'-');
        let op = op..op + 2 + usize::from(strip_tabs);
        let delimiter = op.end
            + b[op.end..w.end]
                .iter()
                .take_while(|&&c| matches!(c, b' ' | b'\t' | b'\r' | 0x0c))
                .count();
        let rest = &s[delimiter..w.end];
        // A delimiter with a `<` in it isn't one.
        if rest.is_empty() || rest.contains('<') {
            continue;
        }
        // Remove the quotes and escapes, as `ProcessWords` does.
        let mut name = String::new();
        let mut quoted = false;
        let mut quote: Option<char> = None;
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match (quote, c) {
                (None, '\'' | '"') => {
                    quote = Some(c);
                    quoted = true;
                }
                (Some(q), c) if c == q => quote = None,
                (None, '\\') => name.extend(chars.next()),
                (Some('"'), '\\') if matches!(chars.peek(), Some('"' | '$' | '\\')) => {
                    name.extend(chars.next());
                }
                _ => name.push(c),
            }
        }
        // BuildKit can't read a delimiter whose quote isn't closed.
        if quote.is_some() {
            continue;
        }
        docs.push(Heredoc {
            word: w,
            op,
            delimiter,
            name,
            strip_tabs,
            expand: !quoted,
        });
    }
    docs
}

/// Whether BuildKit's `shell` package forgets the start of a word, which
/// it does at a `${` with an operator, as in `<<EOF${x:-y}`. To read the
/// word after the operator, it calls a function that clears the buffer
/// that holds the word so far.
fn forgets_start(w: &str) -> bool {
    let b = w.as_bytes();
    let mut quote = None;
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (None, b'\'' | b'"') => quote = Some(b[i]),
            (Some(q), c) if c == q => quote = None,
            (Some(b'\''), _) => {}
            (_, b'\\') => i += 1,
            (_, b'$') if b.get(i + 1) == Some(&b'{') => {
                let n = name_len(&w[i + 2..]);
                if b.get(i + 2 + n).is_some_and(|&c| c != b'}') {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// The words of the arguments from `a` on, as `ProcessWords` splits them
/// for `heredocsFromLine`. Whitespace in quotes or after a `\\` doesn't
/// end a word, and neither do the spaces and tabs right after `<<`.
fn heredoc_words(s: &str, a: usize) -> Vec<Range<usize>> {
    let b = s.as_bytes();
    // `ProcessWords` splits words at any whitespace, such as U+00A0.
    let skip = |i: usize| s.len() - s[i..].trim_start().len();
    let mut words = Vec::new();
    let mut i = skip(a);
    while i < b.len() {
        let start = i;
        let mut quote = None;
        while let Some(c) = s[i..].chars().next() {
            match quote {
                None if c.is_whitespace() => break,
                None if c == '\'' || c == '"' => quote = Some(c),
                None if b[i..].starts_with(b"<<") => {
                    i += 2;
                    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r') {
                        i += 1;
                    }
                    continue;
                }
                Some(q) if c == q => quote = None,
                _ if c == '\\' && quote != Some('\'') => {
                    i += s[i + 1..].chars().next().map_or(0, char::len_utf8);
                }
                _ => {}
            }
            i += c.len_utf8();
        }
        words.push(start..i);
        i = skip(i);
    }
    words
}

/// Whether BuildKit's `shell` package reads `s` without an error, with `\\`
/// as the escape character, as `heredocsFromLine` has it read the whole
/// instruction. It fails on a quote that isn't closed, and on some forms
/// of `${...}` such as `${}` and `${x`.
fn shell_reads(s: &str) -> bool {
    let mut r = ShellReader {
        c: s.chars().collect(),
        i: 0,
    };
    r.read(None)
}

/// Follows `processStopOn` and the functions it calls in BuildKit's
/// `shell` package, only to see whether they fail.
struct ShellReader {
    c: Vec<char>,
    i: usize,
}

impl ShellReader {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    /// Read up to `stop`, or to the end if it's `None`.
    fn read(&mut self, stop: Option<char>) -> bool {
        while let Some(c) = self.next() {
            let ok = match c {
                c if Some(c) == stop => return true,
                '$' => self.dollar(),
                '\'' => self.single_quote(),
                '"' => self.double_quote(),
                '\\' => {
                    self.next();
                    true
                }
                _ => true,
            };
            if !ok {
                return false;
            }
        }
        stop.is_none()
    }

    fn single_quote(&mut self) -> bool {
        loop {
            match self.next() {
                None => return false,
                Some('\'') => return true,
                Some(_) => {}
            }
        }
    }

    /// In double quotes, `\\` only escapes `"`, `$` and itself.
    fn double_quote(&mut self) -> bool {
        loop {
            match self.next() {
                None => return false,
                Some('"') => return true,
                Some('$') => {
                    if !self.dollar() {
                        return false;
                    }
                }
                Some('\\') if matches!(self.peek(), Some('"' | '$' | '\\')) => self.i += 1,
                Some(_) => {}
            }
        }
    }

    /// Read what follows a `$`.
    fn dollar(&mut self) -> bool {
        if self.peek() != Some('{') {
            self.name();
            return true;
        }
        self.i += 1;
        if matches!(self.peek(), None | Some('{' | '}' | ':')) {
            return false;
        }
        self.name();
        match self.next() {
            Some('}') => true,
            // The character after the `:` can be anything, but `#` and `%`
            // aren't allowed there.
            Some(':') => !matches!(self.next(), Some('#' | '%')) && self.read(Some('}')),
            Some('+' | '-' | '?' | '#' | '%') => self.read(Some('}')),
            Some('/') => {
                if self.peek() == Some('/') {
                    self.i += 1;
                }
                self.read(Some('/')) && self.read(Some('}'))
            }
            _ => false,
        }
    }

    /// Read a name, as `processName` does.
    fn name(&mut self) {
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => self.i += 1,
            _ => {
                while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    self.i += 1;
                }
            }
        }
    }
}

/// Color a word such as `<<EOF` the way the shell lexer colors it.
fn heredoc_word(doc: &Heredoc, toks: &mut Out) {
    toks.push((doc.word.start..doc.op.start, Kind::Number));
    toks.push((doc.op.clone(), Kind::Operator));
    toks.push((doc.delimiter..doc.word.end, Kind::Attribute));
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn lexed(text: &str) -> Tokens {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out
    }

    /// Each token but the identifiers, as its text and kind.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        lexed(text).colored(text)
    }

    fn identifiers(text: &str) -> Vec<&str> {
        lexed(text).identifiers(text)
    }

    fn kinds_of(text: &str, kind: Kind) -> Vec<&str> {
        tokens(text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_a_dockerfile() {
        use Kind::*;
        let text = r#"# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1.80 AS build
# Build it.
RUN --mount=type=cache,target=/root/.cargo \
    cargo build --release
ENV PATH="/app:${PATH}" MODE=prod
COPY --from=build /src/target/x /app/x
EXPOSE 8080/tcp
CMD ["x", "--serve"]
"#;
        assert_eq!(
            tokens(text),
            [
                ("#", Attribute),
                ("syntax", Attribute),
                ("=", Operator),
                ("docker/dockerfile:1", String),
                ("FROM", Keyword),
                ("--platform", Attribute),
                ("=", Operator),
                ("$", Punctuation),
                ("AS", Keyword),
                ("# Build it.", Comment),
                ("RUN", Keyword),
                ("--mount", Attribute),
                ("=", Operator),
                ("\\", Escape),
                ("ENV", Keyword),
                ("=", Operator),
                ("\"/app:", String),
                ("${", Punctuation),
                ("}", Punctuation),
                ("\"", String),
                ("=", Operator),
                ("COPY", Keyword),
                ("--from", Attribute),
                ("=", Operator),
                ("EXPOSE", Keyword),
                ("8080", Number),
                ("CMD", Keyword),
                ("[", Punctuation),
                ("\"x\"", String),
                (",", Punctuation),
                ("\"--serve\"", String),
                ("]", Punctuation),
            ]
        );
        assert_eq!(
            identifiers(text),
            [
                "BUILDPLATFORM",
                "rust:1.80",
                "build",
                "cargo",
                "PATH",
                "PATH",
                "MODE",
                "build"
            ]
        );
    }

    #[test]
    fn instructions_ignore_case() {
        assert_eq!(
            kinds_of("from a as b\nRun x\nworkDir /a\n", Kind::Keyword),
            ["from", "as", "Run", "workDir"]
        );
        // BuildKit doesn't know these, so they're plain.
        assert_eq!(kinds_of("FORM a\nRUNX b\n", Kind::Keyword), [] as [&str; 0]);
        // BuildKit trims all whitespace from the ends of an instruction and
        // its arguments.
        assert_eq!(kinds_of("RUN\u{a0}\n", Kind::Keyword), ["RUN"]);
        assert_eq!(
            kinds_of("HEALTHCHECK \u{a0}NONE\n", Kind::Keyword),
            ["HEALTHCHECK", "NONE"]
        );
        assert_eq!(
            kinds_of("SHELL [\"sh\"]\u{b}\n", Kind::Punctuation),
            ["[", "]"]
        );
    }

    #[test]
    fn directives() {
        use Kind::*;
        assert_eq!(
            tokens("#escape = `\n# check=skip=all\n"),
            [
                ("#escape", Attribute),
                ("=", Operator),
                ("`", String),
                ("#", Attribute),
                ("check", Attribute),
                ("=", Operator),
                ("skip=all", String),
            ]
        );
        // Directives only come first, before any other line, even a blank
        // one or a directive that BuildKit doesn't know.
        assert_eq!(kinds_of("\n# syntax=a\n", Comment), ["# syntax=a"]);
        assert_eq!(
            kinds_of("# foo=a\n# syntax=a\n", Comment),
            ["# foo=a", "# syntax=a"]
        );
        assert_eq!(kinds_of("RUN a\n# syntax=a\n", Comment), ["# syntax=a"]);
    }

    #[test]
    fn line_continuations() {
        use Kind::*;
        // Comment lines and blank lines inside an instruction are skipped,
        // and spaces can follow the escape character.
        let text = "RUN a \\  \n# b \\\n\n  && c\nRUN d\n";
        assert_eq!(
            tokens(text),
            [
                ("RUN", Keyword),
                ("\\", Escape),
                ("# b \\", Comment),
                ("&&", Operator),
                ("RUN", Keyword),
            ]
        );
        assert_eq!(identifiers(text), ["a", "c", "d"]);
        // With `` ` `` as the escape character, a `\` is plain.
        let text = "# escape=`\nRUN a `\n  b\nCOPY a\\b c\\ d\n";
        assert_eq!(kinds_of(text, Escape), ["`"]);
        // A line continuation inside quotes.
        assert_eq!(
            tokens("ENV A=\"b \\\nc\"\n"),
            [
                ("ENV", Keyword),
                ("=", Operator),
                ("\"b ", String),
                ("\\", Escape),
                ("c\"", String),
            ]
        );
        // A line that ends in `a\\` doesn't go on, because the second `\`
        // is escaped.
        let text = "ENV a=b\\\\\nRUN c\n";
        assert_eq!(kinds_of(text, Keyword), ["ENV", "RUN"]);
        assert_eq!(kinds_of(text, Escape), ["\\\\"]);
        // With `` ` `` as the escape character, the `\` in ``a\` `` at the
        // end of a line escapes the space at the start of the next line.
        // So the command is one word, which has no plain name.
        let text = "# escape=`\nSHELL [\"sh\", \"-c\"]\nRUN a\\`\n b\n";
        assert_eq!(kinds_of(text, Escape), ["\\`", " "]);
        assert_eq!(identifiers(text), [] as [&str; 0]);
        // BuildKit joins the lines with nothing between them, so a word
        // can go on on the next line.
        assert_eq!(kinds_of("RU\\\nN a\\\nb c\n", Keyword), ["RU", "N"]);
        assert_eq!(identifiers("RUN a\\\nb c\n"), ["a", "b"]);
        assert_eq!(kinds_of("ENV a\\\n=b\n", Operator), ["="]);
        // CRLF line breaks.
        assert_eq!(
            tokens("RUN a \\\r\n  b\r\nENV x=1\r\n"),
            [
                ("RUN", Keyword),
                ("\\", Escape),
                ("ENV", Keyword),
                ("=", Operator),
            ]
        );
    }

    #[test]
    fn shell_commands() {
        use Kind::*;
        assert_eq!(
            tokens("RUN if [ -f x ]; then echo \"$HOME\"; fi\n"),
            [
                ("RUN", Keyword),
                ("if", Keyword),
                (";", Punctuation),
                ("then", Keyword),
                ("\"", String),
                ("$", Punctuation),
                ("\"", String),
                (";", Punctuation),
                ("fi", Keyword),
            ]
        );
        // A shell command can start with `[`. It's JSON only if it reads as
        // a JSON array of strings.
        let text = "RUN [ -f x ] && y\nCMD [\"a\"] && b\n";
        assert_eq!(identifiers(text), ["y", "b"]);
        assert_eq!(kinds_of(text, Punctuation), [] as [&str; 0]);
        assert_eq!(identifiers("ENTRYPOINT [\"a\", \"b\"]\n"), [] as [&str; 0]);
        // An array that doesn't end on the instruction's last line isn't
        // JSON.
        assert_eq!(
            kinds_of("CMD [\"a\", \"b\"\n", Punctuation),
            [] as [&str; 0]
        );
        // Or doesn't read as JSON on a later line.
        assert_eq!(
            kinds_of("CMD [\"a\", \\\n  b]\n", Punctuation),
            [] as [&str; 0]
        );
        // A JSON array that goes on over several lines.
        assert_eq!(
            tokens("CMD [\"a\", \\\n  \"b\"]\n"),
            [
                ("CMD", Keyword),
                ("[", Punctuation),
                ("\"a\"", String),
                (",", Punctuation),
                ("\\", Escape),
                ("\"b\"", String),
                ("]", Punctuation),
            ]
        );
        // `HEALTHCHECK CMD` and `ONBUILD RUN` hold commands too.
        let text = "HEALTHCHECK --interval=5s CMD curl -f x || exit 1\nONBUILD RUN a\n";
        assert_eq!(
            kinds_of(text, Keyword),
            ["HEALTHCHECK", "CMD", "ONBUILD", "RUN"]
        );
        assert_eq!(identifiers(text), ["curl", "exit", "a"]);
        assert_eq!(
            kinds_of("HEALTHCHECK NONE\n", Keyword),
            ["HEALTHCHECK", "NONE"]
        );
    }

    #[test]
    fn other_shells() {
        // After a `SHELL` that isn't in the sh family, commands are plain
        // until the next stage.
        let text = "FROM a\n\
                    SHELL [\"powershell\", \"-Command\"]\n\
                    RUN Get-Item $env:x\n\
                    FROM b\n\
                    RUN c\n";
        assert_eq!(identifiers(text), ["a", "b", "c"]);
        let text = "SHELL [\"/bin/bash\", \"-o\", \"pipefail\", \"-c\"]\nRUN a\n";
        assert_eq!(identifiers(text), ["a"]);
        // With `` ` `` as the escape character, the image is probably for
        // Windows, where the shell is `cmd`.
        assert_eq!(identifiers("# escape=`\nFROM a\nRUN dir c:\\\n"), ["a"]);
    }

    #[test]
    fn here_documents() {
        use Kind::*;
        // A `RUN` that is just a here-document runs its body as a script.
        let text = "RUN <<EOF\nset -e\napt-get update\nEOF\nRUN a\n";
        assert_eq!(
            tokens(text),
            [
                ("RUN", Keyword),
                ("<<", Operator),
                ("EOF", Attribute),
                ("EOF", Attribute),
                ("RUN", Keyword),
            ]
        );
        assert_eq!(identifiers(text), ["set", "apt-get", "a"]);
        // Its `#!` line can name another language.
        let text = "RUN <<-EOF\n#!/usr/bin/env python3\nprint(\"a\")\n\tEOF\n";
        assert_eq!(
            tokens(text),
            [
                ("RUN", Keyword),
                ("<<-", Operator),
                ("EOF", Attribute),
                ("#!/usr/bin/env python3", Comment),
                ("(", Punctuation),
                ("\"a\"", String),
                (")", Punctuation),
                ("EOF", Attribute),
            ]
        );
        // Otherwise the shell reads the here-documents.
        let text = "RUN --network=none cat <<A > a <<'B' > b\n$x\nA\n$y\nB\n";
        assert_eq!(
            tokens(text),
            [
                ("RUN", Keyword),
                ("--network", Attribute),
                ("=", Operator),
                ("<<", Operator),
                ("A", Attribute),
                (">", Operator),
                ("<<", Operator),
                ("'B'", Attribute),
                (">", Operator),
                ("$", Punctuation),
                ("A", Attribute),
                ("$y", String),
                ("B", Attribute),
            ]
        );
        // BuildKit finds here-documents in words that start with `<<`, and
        // the spaces after `<<` are part of the word.
        assert_eq!(
            kinds_of("RUN cat << EOF\nRUN x\nEOF\n", Attribute),
            ["EOF", "EOF"]
        );
        assert_eq!(
            kinds_of("RUN cat<<EOF\nRUN x\nEOF\n", Keyword),
            ["RUN", "RUN"]
        );
        // BuildKit's shell package forgets the start of a word at a `${`
        // with an operator, so this word doesn't start with `<<` any more.
        assert_eq!(
            kinds_of("COPY <<EOF${x:-y} /x\nRUN y\nEOF\n", Keyword),
            ["COPY", "RUN"]
        );
        // When BuildKit can't read the instruction, as with a quote that
        // isn't closed, it finds no here-documents.
        assert_eq!(
            kinds_of("COPY <<EOF 'a /x\nRUN y\nEOF\n", Keyword),
            ["COPY", "RUN"]
        );
        assert_eq!(
            kinds_of("COPY <<EOF ${x /x\nRUN y\nEOF\n", Keyword),
            ["COPY", "RUN"]
        );
    }

    #[test]
    fn copied_here_documents() {
        use Kind::*;
        let text = "COPY <<EOF <<\"B\" /etc/\na $x\nEOF\n$y\nB\n";
        assert_eq!(
            tokens(text),
            [
                ("COPY", Keyword),
                ("<<", Operator),
                ("EOF", Attribute),
                ("<<", Operator),
                ("\"B\"", Attribute),
                ("a ", String),
                ("$", Punctuation),
                ("EOF", Attribute),
                ("$y", String),
                ("B", Attribute),
            ]
        );
        assert_eq!(identifiers(text), ["x"]);
        // A single file whose name says what it holds is lexed in that
        // language.
        let text = "COPY --chmod=755 <<EOF /app/run.py\nimport os\nEOF\n";
        assert_eq!(
            tokens(text),
            [
                ("COPY", Keyword),
                ("--chmod", Attribute),
                ("=", Operator),
                ("<<", Operator),
                ("EOF", Attribute),
                ("import", Keyword),
                ("EOF", Attribute),
            ]
        );
        // After `<<-`, the tabs at the start of each line aren't part of
        // the file.
        // So in Markdown, `# a` is a heading rather than code.
        assert_eq!(
            tokens("COPY <<-EOF a.md\n\t# a\n\tEOF\n")[3..],
            [("# a", Keyword), ("EOF", Attribute)]
        );
        // And the `#!` line of a script can be indented.
        assert_eq!(
            kinds_of("RUN <<-EOF\n\t#!/usr/bin/python3\n\tx = 1\n\tEOF\n", Number),
            ["1"]
        );
        // A body without its end goes on to the end of the file.
        assert_eq!(kinds_of("ADD <<EOF x\nRUN a\n", String), ["RUN a"]);
        // BuildKit removes the `\` from escapes in a body that it expands,
        // so `\$x` isn't a variable. It leaves quotes alone.
        let text = "COPY <<EOF x\n\\$x a\\n '$y'\nEOF\n";
        assert_eq!(kinds_of(text, Escape), ["\\$", "\\n"]);
        assert_eq!(identifiers(text), ["y"]);
    }

    #[test]
    fn variables() {
        use Kind::*;
        let text = "ARG A B=1\n\
                    ENV C=${A:-x} D='$B' E=\"${B#*/}\\$\"\n\
                    WORKDIR $C/a\\ b\n";
        assert_eq!(
            tokens(text),
            [
                ("ARG", Keyword),
                ("=", Operator),
                ("ENV", Keyword),
                ("=", Operator),
                ("${", Punctuation),
                (":-", Operator),
                ("}", Punctuation),
                ("=", Operator),
                ("'$B'", String),
                ("=", Operator),
                ("\"", String),
                ("${", Punctuation),
                ("#", Operator),
                ("*/", String),
                ("}", Punctuation),
                ("\\$", Escape),
                ("\"", String),
                ("WORKDIR", Keyword),
                ("$", Punctuation),
                ("\\ ", Escape),
            ]
        );
        assert_eq!(identifiers(text), ["A", "B", "C", "A", "D", "E", "B", "C"]);
        // The old form of `ENV` sets one variable to the rest of the line.
        let text = "ENV A b c=$d\n";
        assert_eq!(identifiers(text), ["A", "d"]);
        assert_eq!(kinds_of(text, Operator), [] as [&str; 0]);
        // A quoted key, and a variable in a replacement.
        let text = "LABEL \"a.b\"=c x=${y/a/$z}\n";
        assert_eq!(
            tokens(text),
            [
                ("LABEL", Keyword),
                ("\"a.b\"", String),
                ("=", Operator),
                ("=", Operator),
                ("${", Punctuation),
                ("/", Operator),
                ("/", Operator),
                ("$", Punctuation),
                ("}", Punctuation),
            ]
        );
        // Flags use the file's escape character, which escapes the next
        // character even in single quotes.
        let text = "# escape=`\nCOPY --a='x`' y' --b=1 $c d\n";
        assert_eq!(kinds_of(text, Attribute)[2..], ["--a", "--b"]);
        assert_eq!(identifiers(text), ["c"]);
    }

    #[test]
    fn stages() {
        use Kind::*;
        let text = "FROM ${BASE} as base\nFROM base\nCOPY --from=base a b\n";
        assert_eq!(
            tokens(text),
            [
                ("FROM", Keyword),
                ("${", Punctuation),
                ("}", Punctuation),
                ("as", Keyword),
                ("FROM", Keyword),
                ("COPY", Keyword),
                ("--from", Attribute),
                ("=", Operator),
            ]
        );
        assert_eq!(identifiers(text), ["BASE", "base", "base", "base"]);
    }

    /// A line of a Dockerfile: an instruction, a line that goes on with
    /// one, a comment, a directive or a line of a here-document.
    fn line() -> impl Strategy<Value = String> {
        let start = prop::sample::select(vec![
            "FROM ",
            "from ",
            "RUN ",
            "run ",
            "CMD ",
            "ENTRYPOINT ",
            "COPY ",
            "ADD ",
            "ENV ",
            "ARG ",
            "LABEL ",
            "EXPOSE ",
            "WORKDIR ",
            "USER ",
            "VOLUME ",
            "SHELL ",
            "ONBUILD ",
            "HEALTHCHECK ",
            "STOPSIGNAL ",
            "MAINTAINER ",
            "RUNX ",
            "  ",
            "\t",
            "# ",
            "#",
            "# escape=`",
            "# escape=\\",
            "# syntax=a",
            "",
            "EOF",
            "\tEOF",
            "B",
            "#!/bin/sh",
            "#!/usr/bin/env python3",
            "[\"a\", ",
        ]);
        let piece = prop::sample::select(vec![
            "a",
            "x=1",
            "a.b=\"c d\"",
            "$x",
            "${x}",
            "${x:-a b}",
            "${x/a/b}",
            "\"",
            "'",
            "\\",
            "`",
            "\\\\",
            "--mount=type=cache",
            "--from=b",
            "--",
            "AS",
            "as",
            "<<EOF",
            "<<-EOF",
            "<<'B'",
            "<<\"EOF\"",
            "3<<EOF",
            "<< EOF",
            "<<EOF${x:-y}",
            "<<",
            "${",
            "cat",
            "&&",
            "||",
            ";",
            "|",
            "if",
            "then",
            "fi",
            "case",
            "in",
            "esac",
            ")",
            "(",
            "$(",
            "[",
            "]",
            "[\"a\"",
            ", \"b\"]",
            "\"]",
            "]",
            "{",
            "}",
            "#",
            "# c",
            "/a.py",
            "/a.json",
            "8080/tcp",
            "CMD",
            "NONE",
            "RUN",
            "[\"/bin/sh\", \"-c\"]",
            "[\"cmd\"]",
            "\u{e9}",
            "\u{a0}",
            "\r",
        ]);
        let end = prop::sample::select(vec!["", "", "", " \\", "\\", " \\  ", " `", "`"]);
        (
            start,
            prop::collection::vec((piece, any::<bool>()), 0..6),
            end,
        )
            .prop_map(|(start, pieces, end)| {
                let mut line = start.to_string();
                for (p, space) in pieces {
                    line.push_str(p);
                    if space {
                        line.push(' ');
                    }
                }
                line.push_str(end);
                line
            })
    }

    proptest! {
        /// The lexer doesn't panic, and its tokens are in order, don't
        /// overlap and fall on character boundaries. It joins pieces of
        /// the source in several ways, and lexes them with other lexers.
        #[test]
        fn tokens_are_in_order(lines in prop::collection::vec(line(), 1..12)) {
            let text = lines.join("\n") + "\n";
            let mut pos = 0;
            for (r, _) in lexed(&text).0 {
                prop_assert!(pos <= r.start && r.start < r.end && r.end <= text.len());
                prop_assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
                pos = r.end;
            }
        }
    }
}
