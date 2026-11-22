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
//! The colors of a line never depend on the lines after it. BuildKit
//! joins the lines of an instruction with nothing between them, so a word
//! can go on on the next line. We end words at the end of a line instead,
//! as the shell lexer does.

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

/// A line of the source, without its line break or the carriage return
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

/// An instruction's lines joined with line feeds, without the escape
/// characters that join them.
struct Instruction {
    text: Text,
    /// The range of each line in `text`, and whether it ends with an
    /// escape character. Each line that does has a line feed after it,
    /// even the last one.
    lines: Vec<(Range<usize>, bool)>,
}

/// A here-document, as `heredocsFromLine` and `ParseHeredoc` find it.
#[derive(Debug)]
struct Heredoc {
    /// The whole word, such as `<<-"EOF"`.
    word: Range<usize>,
    /// Where the `<<` starts, after the number of a file descriptor.
    op: usize,
    /// Where the delimiter starts, after `<<` or `<<-`.
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
            let content = content.strip_suffix('\r').unwrap_or(content);
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
        let mut ins = Instruction {
            text: Text::default(),
            lines: Vec::new(),
        };
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
            let at = ins.text.s.len();
            let content = self.src[start..line.end].trim_end_matches([' ', '\t']);
            let more = content.as_bytes().last() == Some(&self.escape);
            let end = if more {
                let e = start + content.len() - 1;
                self.out.push((e..e + 1, Kind::Escape));
                e
            } else {
                line.end
            };
            ins.text.push_src(self.src, start..end);
            ins.lines.push((at..ins.text.s.len(), more));
            if !more {
                break;
            }
            ins.text.s.push('\n');
        }
        let mut toks = Vec::new();
        let next = self.command(&ins, 0, next, &mut toks);
        for (r, kind) in toks {
            ins.text.map(r, kind, &mut self.out);
        }
        next
    }

    /// Lex the instruction that starts at `at` in the text. Its
    /// here-documents start on line `next`. Return the line after them.
    fn command(&mut self, ins: &Instruction, at: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.text.s;
        let start = skip_blanks(s, at);
        let end = word_end(s, start);
        let name = s[start..end].to_ascii_lowercase();
        if !INSTRUCTIONS.contains(&name.as_str()) {
            return next;
        }
        toks.push((start..end, Kind::Keyword));
        let a = flags(s, end, toks, |s, r, toks| self.word(s, r, toks));
        self.arguments(ins, &name, a, next, toks)
    }

    fn arguments(
        &mut self,
        ins: &Instruction,
        name: &str,
        a: usize,
        next: usize,
        toks: &mut Out,
    ) -> usize {
        let s = &ins.text.s;
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
                // A word in quotes can go on to the next line. We only look
                // for the `=` on the first line, so that its colors don't
                // depend on the next.
                let line_end = s[a..].find('\n').map_or(s.len(), |n| a + n);
                if let Some(first) = words.first()
                    && !s[first.start..first.end.min(line_end)].contains('=')
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
                    ins.text.lex(Language::Json, a, &mut self.out);
                } else {
                    for w in blank_words(s, a) {
                        self.word(s, w, toks);
                    }
                }
            }
            "shell" => {
                if json_form(s, a) {
                    ins.text.lex(Language::Json, a, &mut self.out);
                    self.sh = runs_sh(&s[a..]);
                }
            }
            "stopsignal" | "user" | "workdir" => self.word(s, a..s.len(), toks),
            _ => {}
        }
        next
    }

    /// Lex the command of a `RUN`, and the bodies of its here-documents.
    fn run_command(&mut self, ins: &Instruction, a: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.text.s;
        if json_form(s, a) {
            ins.text.lex(Language::Json, a, &mut self.out);
            return next;
        }
        let docs = heredocs(s, a);
        let (bodies, next) = self.bodies(&docs, next);
        // When the command is just one here-document, BuildKit runs its
        // body as a script, with the program its `#!` line names if it has
        // one. Otherwise the shell reads the bodies. Either way the command
        // gets the same colors, because whether it's just a here-document
        // can depend on the lines after the first.
        let script = matches!(&docs[..], [doc] if s[a..].trim_end() == &s[doc.word.clone()]);
        if self.sh {
            self.shell_command(ins, a, if script { &[] } else { &bodies });
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
    fn shell_command(&mut self, ins: &Instruction, a: usize, bodies: &[Body]) {
        let s = &ins.text.s;
        if a >= s.len() {
            return;
        }
        if json_form(s, a) {
            ins.text.lex(Language::Json, a, &mut self.out);
            return;
        }
        if !self.sh {
            return;
        }
        // The shell lexer reads a line continuation as such, so we put
        // the escape characters back. Their colors are already set.
        let mut text = Text::default();
        for (r, more) in &ins.lines {
            if r.end < a || (r.end == a && *more) {
                continue;
            }
            let start = r.start.max(a);
            let mut end = r.end;
            // In `a\\` at the end of a line, BuildKit removes the second
            // `\`, and the first escapes the start of the next line. With
            // the `\` we put back, the shell would read an escaped `\`
            // and end the command. So we leave the first one out too.
            let backslashes = s[start..end]
                .bytes()
                .rev()
                .take_while(|&c| c == b'\\')
                .count();
            if *more && backslashes % 2 == 1 {
                end -= 1;
                ins.text.map(end..end + 1, Kind::Escape, &mut self.out);
            }
            let src = ins
                .text
                .pieces
                .iter()
                .find(|p| p.0 <= start && start < p.0 + p.2);
            if let Some(&(at, src, _)) = src {
                text.push_src(self.src, src + start - at..src + end - at);
            }
            text.s.push_str(if *more { "\\\n" } else { "\n" });
        }
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
    fn copy(&mut self, ins: &Instruction, a: usize, next: usize, toks: &mut Out) -> usize {
        let s = &ins.text.s;
        if json_form(s, a) {
            ins.text.lex(Language::Json, a, &mut self.out);
            return next;
        }
        let docs = heredocs(s, a);
        let words = blank_words(s, a);
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

    /// Find the bodies of here-documents, starting on line `next`, and
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

    /// Color a line of a here-document that holds text. Variables in it
    /// are expanded unless the delimiter was quoted.
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
            escape: None,
            toks: &mut toks,
        };
        let mut i = r.start;
        while i < r.end {
            if self.src.as_bytes()[i] == b'$' {
                i = w.dollar(i, Some(Kind::String));
            } else {
                w.toks.push((i..i + 1, Kind::String));
                i += 1;
            }
        }
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
            escape: Some(self.escape),
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
    /// The escape character, or none in the body of a here-document.
    escape: Option<u8>,
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
                b'\'' => self.single_quote(i),
                b'"' => self.double_quote(i),
                b'$' => self.dollar(i, base),
                c if Some(c) == self.escape => {
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
                c if Some(c) == self.escape
                    && self
                        .at(i + 1)
                        .is_some_and(|n| n == b'"' || n == b'$' || Some(n) == self.escape) =>
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

/// The whitespace that BuildKit splits on. The line feeds that join the
/// lines of an instruction count too.
fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
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
/// escaped whitespace don't end it. This is how `parseWords` and
/// `extractBuilderFlags` find words.
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
/// every instruction, with `\` as the escape character.
fn flags(
    s: &str,
    mut i: usize,
    toks: &mut Out,
    mut value: impl FnMut(&str, Range<usize>, &mut Out),
) -> usize {
    loop {
        i = skip_blanks(s, i);
        if !s[i..].starts_with("--") {
            return i;
        }
        let end = quoted_word_end(s, i, b'\\');
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
                if &s[i..eq] == "--from" && !s[v.clone()].contains(['$', '\'', '"', '\\']) {
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
/// and takes them as a shell command when that fails. We only look at the
/// first line, so that its colors don't depend on the lines after it. An
/// array that is still fine where that line ends counts as JSON.
fn json_form(s: &str, a: usize) -> bool {
    let a = skip_blanks(s, a);
    let b = s.as_bytes();
    let stop = s[a..].find('\n').map_or(s.len(), |n| a + n);
    let ws = |mut i: usize| {
        while i < stop && matches!(b[i], b' ' | b'\t' | b'\r') {
            i += 1;
        }
        i
    };
    if b.get(a) != Some(&b'[') {
        return false;
    }
    let mut i = ws(a + 1);
    if i < stop && b[i] == b']' {
        return ws(i + 1) == stop;
    }
    loop {
        if i == stop {
            return true;
        }
        if b[i] != b'"' {
            return false;
        }
        i += 1;
        loop {
            if i == stop {
                return true;
            }
            match b[i] {
                b'"' => break,
                b'\\' => {
                    i += 1;
                    if i == stop {
                        return true;
                    }
                    match b[i] {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            for _ in 0..4 {
                                i += 1;
                                if i == stop {
                                    return true;
                                }
                                if !b[i].is_ascii_hexdigit() {
                                    return false;
                                }
                            }
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
        if i == stop {
            return true;
        }
        match b[i] {
            b',' => i = ws(i + 1),
            b']' => return ws(i + 1) == stop,
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
/// such as `<<EOF`, `<<-EOF` or `<<"EOF"` as one. A word such as `<<`
/// with the delimiter after a space, or `cat<<EOF`, isn't one.
fn heredocs(s: &str, a: usize) -> Vec<Heredoc> {
    let b = s.as_bytes();
    let mut docs = Vec::new();
    for w in quoted_words(s, a, b'\\') {
        let op = w.start
            + b[w.clone()]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
        if !s[op..w.end].starts_with("<<") {
            continue;
        }
        let strip_tabs = b.get(op + 2) == Some(&b'-');
        let delimiter = op + 2 + usize::from(strip_tabs);
        let rest = &s[delimiter..w.end];
        // A delimiter with a quote that goes on to the next line could
        // have a `<` there, which would make it not a delimiter. We don't
        // look, so that the colors of a line don't depend on the next.
        if rest.is_empty() || rest.contains(['<', '\n']) {
            continue;
        }
        // Remove the quotes and escapes, as `ProcessWords` does.
        let mut name = String::new();
        let mut quoted = false;
        let mut quote = None;
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

/// Color a word such as `<<EOF` the way the shell lexer colors it.
fn heredoc_word(doc: &Heredoc, toks: &mut Out) {
    toks.push((doc.word.start..doc.op, Kind::Number));
    toks.push((doc.op..doc.delimiter, Kind::Operator));
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
        // In `a\\` at the end of a line, the first `\` escapes the start
        // of the next line, and the command goes on.
        let text = "RUN a\\\\\n b\n";
        assert_eq!(kinds_of(text, Escape), ["\\\\"]);
        assert_eq!(identifiers(text), ["a"]);
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
        // BuildKit only finds here-documents in words of their own.
        assert_eq!(kinds_of("RUN cat << EOF\nEOF\n", Keyword), ["RUN"]);
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
            "<<",
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
                // The shell lexer joins a line of a here-document that ends
                // in `\` to the next one, and whether the result ends the
                // here-document then depends on the next line. That's a
                // bug in the shell lexer, listed in `todo/bugs.md`, so we
                // don't end lines that could do that with a `\`.
                let t = line.trim_start();
                if !(end.ends_with('\\') && (t.is_empty() || t.starts_with(['E', 'B']))) {
                    line.push_str(end);
                }
                line
            })
    }

    proptest! {
        /// Cutting a Dockerfile after a line doesn't change the colors of
        /// the lines up to there. A hunk that we lex on its own ends where
        /// the hunk ends, and its lines must get the colors they get in
        /// the whole file.
        #[test]
        fn colors_only_depend_on_earlier_lines(
            lines in prop::collection::vec(line(), 1..12),
        ) {
            let text = lines.join("\n") + "\n";
            let full = super::super::highlight_text(Language::Dockerfile, &text, usize::MAX);
            let mut end = 0;
            for (n, line) in lines.iter().enumerate() {
                end += line.len() + 1;
                let cut = super::super::highlight_text(Language::Dockerfile, &text[..end], usize::MAX);
                prop_assert_eq!(&cut[..], &full[..=n], "cut after line {}", n);
            }
        }
    }
}
