//! A lexer for YAML.
//!
//! It follows YAML 1.2.2, and reads a file the way the scanner of
//! saphyr-parser does. That scanner is a port of libyaml's, fixed to pass
//! the YAML test suite. Like them, the lexer keeps track of the block
//! collections and flow collections it's inside of. It needs the block
//! indentation to know where a block scalar or a plain scalar that goes on
//! over several lines ends. And a word followed by `:` on the same line is
//! a key, so the lexer remembers where each possible key starts.
//!
//! The names in keys are identifiers, so they get colors from their text.
//! The quotes of a quoted key are plain. Values get the color of the
//! type that YAML's core schema gives them, so `1.5` is a number, `true`
//! and `null` are constants, and other plain scalars are strings.
//!
//! A plain scalar that goes on over several lines is always a string,
//! since its lines are joined with spaces.

use std::cell::Cell;
use std::ops::Range;

use logos::Logos;

use super::{Kind, Tokens};

/// The plain scalars that YAML 1.2's core schema reads as something other
/// than a string, from section 10.3.2 of the spec.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Schema {
    #[token("null")]
    #[token("Null")]
    #[token("NULL")]
    #[token("~")]
    #[token("true")]
    #[token("True")]
    #[token("TRUE")]
    #[token("false")]
    #[token("False")]
    #[token("FALSE")]
    Constant,

    // Integers, and floats, whose pattern takes in decimal integers too.
    #[regex(r"0o[0-7]+|0x[0-9a-fA-F]+")]
    #[regex(r"[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?")]
    #[regex(r"[-+]?\.(inf|Inf|INF)|\.(nan|NaN|NAN)")]
    Number,
}

/// The color of a plain scalar that isn't a key.
fn resolve(s: &str) -> Kind {
    let mut lex = Schema::lexer(s);
    match lex.next() {
        Some(Ok(t)) if lex.span().end == s.len() => match t {
            Schema::Constant => Kind::Constant,
            Schema::Number => Kind::Number,
        },
        _ => Kind::String,
    }
}

fn is_break(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// A space, a line break, or the end of the text, which `at` reads as 0.
fn is_blankz(b: u8) -> bool {
    is_blank(b) || is_break(b) || b == 0
}

fn is_flow(b: u8) -> bool {
    matches!(b, b',' | b'[' | b']' | b'{' | b'}')
}

/// A place where a `:` would make the scalar before it a key.
#[derive(Debug, Clone, Copy)]
struct SimpleKey {
    /// The first of the key's tokens in `Lexer::out`.
    tok: usize,
    line: usize,
    pos: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Seq,
    Map,
}

#[derive(Debug)]
struct Tok {
    r: Range<usize>,
    /// None for a token that has no color, such as the quotes of a key.
    kind: Option<Kind>,
    /// For a plain or quoted scalar, the flow level it's at, so that a
    /// `:` after it can make it a key.
    scalar: Option<usize>,
}

struct Lexer<'a> {
    s: &'a [u8],
    pos: usize,
    line: usize,
    line_start: usize,
    /// A position on the current line and its column, so that finding the
    /// column of each token on a long line doesn't take time that grows
    /// with the square of its length.
    seen: Cell<(usize, usize)>,
    /// The columns of the block collections we're inside of, innermost
    /// last.
    indents: Vec<usize>,
    flows: Vec<Flow>,
    /// The key that a `:` could end, for the block level and then each
    /// flow level.
    keys: Vec<Option<SimpleKey>>,
    /// Whether a simple key can start here, as in the scanner.
    allowed: bool,
    /// The next node is a key even without a `:` after it, because it
    /// follows a `?` or starts an entry in a flow mapping.
    key_next: bool,
    /// The last token was a quoted scalar or the end of a flow
    /// collection. Inside a flow collection, a `:` right after one is a
    /// value indicator even without a space after it, as in `{"a":1}`.
    adjacent: bool,
    out: Vec<Tok>,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Lexer {
        s: text.as_bytes(),
        pos: 0,
        line: 0,
        line_start: 0,
        seen: Cell::new((0, 0)),
        indents: Vec::new(),
        flows: Vec::new(),
        keys: vec![None],
        allowed: true,
        key_next: false,
        adjacent: false,
        out: Vec::new(),
    };
    lex.run();
    for t in lex.out {
        if let Some(kind) = t.kind {
            out.push(t.r, kind);
        }
    }
}

impl Lexer<'_> {
    /// The byte `i` bytes ahead, or 0 at the end of the text.
    fn at(&self, i: usize) -> u8 {
        self.s.get(self.pos + i).copied().unwrap_or(0)
    }

    /// The column, in characters, as the scanner counts it.
    fn column(&self) -> usize {
        let (mut from, mut column) = self.seen.get();
        if from < self.line_start || from > self.pos {
            (from, column) = (self.line_start, 0);
        }
        column += self.s[from..self.pos]
            .iter()
            .filter(|&&b| b & 0xc0 != 0x80)
            .count();
        self.seen.set((self.pos, column));
        column
    }

    fn indent(&self) -> isize {
        self.indents.last().map_or(-1, |&i| i as isize)
    }

    fn level(&self) -> usize {
        self.flows.len()
    }

    fn push(&mut self, r: Range<usize>, kind: Kind) {
        self.out.push(Tok {
            r,
            kind: Some(kind),
            scalar: None,
        });
    }

    /// Step over the line break at `pos`.
    fn newline(&mut self) {
        self.pos += if self.at(0) == b'\r' && self.at(1) == b'\n' {
            2
        } else {
            1
        };
        self.line += 1;
        self.line_start = self.pos;
    }

    /// Where the line that `pos` is on ends, before its line break.
    fn line_end(&self) -> usize {
        self.s[self.pos..]
            .iter()
            .position(|&b| is_break(b))
            .map_or(self.s.len(), |i| self.pos + i)
    }

    fn after_blanks(&self) -> usize {
        let mut p = self.pos;
        while self.s.get(p).is_some_and(|&b| is_blank(b)) {
            p += 1;
        }
        p
    }

    /// A `---` or `...` at the start of a line, followed by a space or the
    /// end of the line.
    fn at_document_marker(&self) -> bool {
        let three = &self.s[self.pos..self.s.len().min(self.pos + 3)];
        self.pos == self.line_start && (three == b"---" || three == b"...") && is_blankz(self.at(3))
    }

    fn roll(&mut self, column: usize) {
        if self.flows.is_empty() && self.indent() < column as isize {
            self.indents.push(column);
        }
    }

    fn unroll(&mut self, column: usize) {
        if self.flows.is_empty() {
            while self.indent() > column as isize {
                self.indents.pop();
            }
        }
    }

    fn save_key(&mut self) {
        if self.allowed {
            let key = SimpleKey {
                tok: self.out.len(),
                line: self.line,
                pos: self.pos,
                column: self.column(),
            };
            let level = self.level();
            self.keys[level] = Some(key);
        }
    }

    fn remove_key(&mut self) {
        let level = self.level();
        self.keys[level] = None;
    }

    /// Forget everything, at the start or end of a document.
    fn reset(&mut self) {
        self.indents.clear();
        self.flows.clear();
        self.keys.clear();
        self.keys.push(None);
        self.allowed = false;
        self.key_next = false;
    }

    fn run(&mut self) {
        loop {
            self.skip_to_token();
            if self.pos >= self.s.len() {
                break;
            }
            let column = self.column();
            self.unroll(column);
            let start = self.pos;
            self.token(column);
            if self.pos == start {
                // Nothing can start here, as with a NUL. Step over the
                // character.
                let len = self.s[start..]
                    .iter()
                    .skip(1)
                    .position(|&b| b & 0xc0 != 0x80)
                    .map_or(self.s.len() - start, |i| i + 1);
                self.pos += len;
            }
        }
    }

    /// Skip spaces, line breaks and comments.
    fn skip_to_token(&mut self) {
        loop {
            match self.at(0) {
                b' ' | b'\t' => self.pos += 1,
                b'\n' | b'\r' => {
                    self.newline();
                    if self.flows.is_empty() {
                        self.allowed = true;
                    }
                }
                b'#' => {
                    let end = self.line_end();
                    self.push(self.pos..end, Kind::Comment);
                    self.pos = end;
                }
                // A byte order mark can start a document.
                0xef if self.pos == self.line_start
                    && self.s[self.pos..].starts_with("\u{feff}".as_bytes()) =>
                {
                    self.pos += 3;
                    self.line_start = self.pos;
                }
                _ => break,
            }
        }
    }

    fn token(&mut self, column: usize) {
        let c = self.at(0);
        let next = self.at(1);
        let flow = !self.flows.is_empty();
        let adjacent = std::mem::take(&mut self.adjacent);
        if column == 0 && c == b'%' {
            return self.directive();
        }
        if self.at_document_marker() {
            self.reset();
            self.push(self.pos..self.pos + 3, Kind::Punctuation);
            self.pos += 3;
            return;
        }
        match c {
            b'[' | b'{' => self.flow_start(c == b'{'),
            b']' | b'}' => self.flow_end(),
            b',' => {
                self.remove_key();
                self.allowed = true;
                self.key_next = self.flows.last() == Some(&Flow::Map);
                self.indicator();
            }
            b'-' if is_blankz(next) => {
                self.roll(column);
                self.remove_key();
                self.allowed = true;
                self.key_next = false;
                self.indicator();
            }
            b'?' if is_blankz(next) => {
                self.roll(column);
                self.remove_key();
                self.allowed = !flow;
                self.key_next = true;
                self.indicator();
            }
            b':' if is_blankz(next) || (flow && (is_flow(next) || adjacent)) => {
                self.value(column);
            }
            b'*' | b'&' => {
                self.save_key();
                self.allowed = false;
                if c == b'*' {
                    self.key_next = false;
                }
                let start = self.pos;
                self.pos += 1;
                while !is_blankz(self.at(0)) && !is_flow(self.at(0)) {
                    self.pos += 1;
                }
                self.push(start..self.pos, Kind::Attribute);
            }
            b'!' => {
                self.save_key();
                self.allowed = false;
                let start = self.pos;
                self.pos += 1;
                if self.at(0) == b'<' {
                    while !is_blankz(self.at(0)) && self.at(0) != b'>' {
                        self.pos += 1;
                    }
                    if self.at(0) == b'>' {
                        self.pos += 1;
                    }
                } else {
                    while !is_blankz(self.at(0)) && !is_flow(self.at(0)) {
                        self.pos += 1;
                    }
                }
                self.push(start..self.pos, Kind::Type);
            }
            b'|' | b'>' if !flow => self.block_scalar(),
            b'\'' | b'"' => self.quoted(c),
            _ => self.plain(),
        }
    }

    /// A one-byte indicator such as `-` or `,`.
    fn indicator(&mut self) {
        self.push(self.pos..self.pos + 1, Kind::Punctuation);
        self.pos += 1;
    }

    /// A line such as `%YAML 1.2`. The name gets the keyword color, as a
    /// directive does in C.
    fn directive(&mut self) {
        self.reset();
        let start = self.pos;
        while !is_blankz(self.at(0)) {
            self.pos += 1;
        }
        self.push(start..self.pos, Kind::Keyword);
        // The rest of the line, up to a comment.
        while !is_break(self.at(0)) && self.pos < self.s.len() {
            if self.at(0) == b'#' && is_blank(self.s[self.pos - 1]) {
                break;
            }
            self.pos += 1;
        }
    }

    fn flow_start(&mut self, map: bool) {
        // A flow collection can be a key, but what's in it isn't.
        self.save_key();
        self.flows.push(if map { Flow::Map } else { Flow::Seq });
        self.keys.push(None);
        self.allowed = true;
        self.key_next = map;
        self.indicator();
    }

    fn flow_end(&mut self) {
        self.remove_key();
        // A bracket that closes nothing, as in a hunk from the middle of a
        // flow collection, changes nothing.
        if self.flows.pop().is_some() {
            self.keys.pop();
        }
        self.allowed = false;
        self.key_next = false;
        self.indicator();
        self.adjacent = true;
    }

    /// A `:` that ends a key.
    fn value(&mut self, column: usize) {
        let level = self.level();
        // Outside of flow collections, a key has to end on the line it
        // starts on, and within 1024 characters. Inside one, the scanner
        // has no limit.
        let key = self.keys[level]
            .take()
            .filter(|k| level > 0 || k.line == self.line && self.pos <= k.pos + 1024);
        if let Some(key) = key {
            self.mark_key(key.tok, level);
            self.roll(key.column);
            self.allowed = false;
        } else {
            self.roll(column);
            self.allowed = self.flows.is_empty();
        }
        self.key_next = false;
        self.indicator();
    }

    /// Make the scalar at flow level `level` among the tokens from `from`
    /// on a key, which is an identifier. A quoted key's quotes are plain.
    /// A plain key over several lines is an identifier on each line. A
    /// quoted one stays a string, since its name would be split over
    /// several tokens.
    fn mark_key(&mut self, from: usize, level: usize) {
        let Some(i) = (from..self.out.len()).find(|&i| self.out[i].scalar == Some(level)) else {
            return;
        };
        // A key after `?` or at the start of an entry in a flow mapping
        // was marked when we read it, before the `:` after it.
        if self.out[i].kind == Some(Kind::Identifier) {
            return;
        }
        // The key is the one scalar that starts there. Later scalars at
        // the same level, as after a nested flow collection, aren't part
        // of it.
        let n = self.out[i..]
            .iter()
            .take_while(|t| t.scalar == Some(level))
            .count();
        let mut r = self.out[i].r.start..self.out[i + n - 1].r.end;
        let quote = self.s[r.start];
        let quoted = quote == b'"' || quote == b'\'';
        if self.s[r.clone()].iter().any(|&b| is_break(b)) {
            if !quoted {
                for t in &mut self.out[i..i + n] {
                    t.kind = Some(Kind::Identifier);
                }
            }
            return;
        }
        for t in &mut self.out[i..i + n] {
            t.kind = None;
        }
        if quoted {
            r.start += 1;
            if r.end > r.start && self.s[r.end - 1] == quote {
                r.end -= 1;
            }
        }
        self.out[i] = Tok {
            r,
            kind: Some(Kind::Identifier),
            scalar: Some(level),
        };
    }

    /// A literal or folded block scalar, which starts with `|` or `>`.
    fn block_scalar(&mut self) {
        self.remove_key();
        self.allowed = true;
        self.key_next = false;
        let n = self.indent();
        let start = self.pos;
        self.pos += 1;
        // The chomping and indentation indicators, in either order.
        let mut increment = 0;
        let digit = |b: u8| (b'1'..=b'9').contains(&b);
        if matches!(self.at(0), b'+' | b'-') {
            self.pos += 1;
            if digit(self.at(0)) {
                increment = usize::from(self.at(0) - b'0');
                self.pos += 1;
            }
        } else if digit(self.at(0)) {
            increment = usize::from(self.at(0) - b'0');
            self.pos += 1;
            if matches!(self.at(0), b'+' | b'-') {
                self.pos += 1;
            }
        }
        self.push(start..self.pos, Kind::Punctuation);
        self.pos = self.after_blanks();
        if self.at(0) == b'#' && self.pos > start && is_blank(self.s[self.pos - 1]) {
            let end = self.line_end();
            self.push(self.pos..end, Kind::Comment);
            self.pos = end;
        }
        if !is_break(self.at(0)) {
            // Anything else after the header is an error, and there's no
            // content to read.
            return;
        }
        self.newline();

        let indent = if increment > 0 {
            // Like libyaml, the scanner counts an indicator at the top
            // level from column 0, not from column -1 as the spec does.
            n.max(0) as usize + increment
        } else {
            // The first line that isn't empty sets the indentation. Empty
            // lines before it can't have more spaces, or it would be an
            // error, so the most spaces of any of them count.
            let mut most = 0;
            loop {
                while self.at(0) == b' ' {
                    self.pos += 1;
                }
                most = most.max(self.pos - self.line_start);
                if !is_break(self.at(0)) {
                    break;
                }
                self.newline();
            }
            most.max((n + 1) as usize)
        };
        if increment > 0 {
            self.skip_indent(indent);
        }
        while self.pos - self.line_start == indent
            && self.pos < self.s.len()
            && !is_break(self.at(0))
        {
            if indent == 0 && self.at_document_marker() {
                break;
            }
            let end = self.line_end();
            self.push(self.pos..end, Kind::String);
            self.pos = end;
            if !is_break(self.at(0)) {
                break;
            }
            self.newline();
            self.skip_indent(indent);
        }
    }

    /// Skip up to `indent` spaces at the start of each line, and the lines
    /// that hold nothing else.
    fn skip_indent(&mut self, indent: usize) {
        loop {
            while self.pos - self.line_start < indent && self.at(0) == b' ' {
                self.pos += 1;
            }
            if !is_break(self.at(0)) {
                break;
            }
            self.newline();
        }
    }

    /// A single or double quoted scalar. It can go on over several lines,
    /// and one that doesn't end goes on to the end of the text. A document
    /// marker inside one is an error, so it ends the string there.
    fn quoted(&mut self, quote: u8) {
        self.save_key();
        self.allowed = false;
        let key = std::mem::take(&mut self.key_next);
        let first = self.out.len();
        let line = self.line;
        let mut start = self.pos;
        self.pos += 1;
        loop {
            let c = self.at(0);
            if self.pos >= self.s.len() || self.at_document_marker() {
                break;
            }
            if c == quote && quote == b'\'' && self.at(1) == b'\'' {
                self.push(start..self.pos, Kind::String);
                self.push(self.pos..self.pos + 2, Kind::Escape);
                self.pos += 2;
                start = self.pos;
            } else if c == quote {
                self.pos += 1;
                break;
            } else if c == b'\\'
                && quote == b'"'
                && let len @ 1.. = escape(&self.s[self.pos + 1..])
            {
                self.push(start..self.pos, Kind::String);
                self.push(self.pos..self.pos + len, Kind::Escape);
                self.pos += len;
                start = self.pos;
            } else if is_break(c) {
                self.newline();
            } else {
                self.pos += 1;
            }
        }
        self.push(start..self.pos, Kind::String);
        let level = self.level();
        for t in &mut self.out[first..] {
            t.scalar = Some(level);
        }
        if key && self.line == line {
            self.mark_key(first, level);
        }
        self.adjacent = true;
    }

    /// A plain scalar, which can go on over several lines.
    fn plain(&mut self) {
        self.save_key();
        self.allowed = false;
        let key = std::mem::take(&mut self.key_next);
        let flow = !self.flows.is_empty();
        // Lines after the first have to be indented more than the block
        // collection the scalar is in.
        let min = self.indent() + 1;
        // The text of the scalar on each line before the one we're on.
        let mut lines = Vec::new();
        // The text of the scalar on the line we're on.
        let mut seg: Option<Range<usize>> = None;
        loop {
            if self.at_document_marker() || self.at(0) == b'#' {
                break;
            }
            if !is_blankz(self.at(0)) && self.can_plain(flow) {
                let start = self.pos;
                while !is_blankz(self.at(0)) && self.can_plain(flow) {
                    self.pos += 1;
                }
                match &mut seg {
                    Some(r) => r.end = self.pos,
                    None => seg = Some(start..self.pos),
                }
            }
            if !is_blank(self.at(0)) && !is_break(self.at(0)) {
                break;
            }
            let mut broke = false;
            while is_blank(self.at(0)) || is_break(self.at(0)) {
                if is_break(self.at(0)) {
                    self.newline();
                    broke = true;
                } else {
                    self.pos += 1;
                }
            }
            if broke {
                lines.extend(seg.take());
                self.allowed = true;
                if !flow && (self.column() as isize) < min {
                    break;
                }
            }
        }
        lines.extend(seg);
        let kind = match &lines[..] {
            _ if key => Kind::Identifier,
            // The bytes are on character boundaries, since the scalar only
            // stops at ASCII.
            [r] => resolve(std::str::from_utf8(&self.s[r.clone()]).unwrap_or("")),
            // Lines are joined with spaces, and no value in the schema
            // holds one.
            _ => Kind::String,
        };
        let level = self.level();
        for r in lines {
            self.out.push(Tok {
                r,
                kind: Some(kind),
                scalar: Some(level),
            });
        }
    }

    /// Whether the byte at `pos` can go on a plain scalar. A `:` followed
    /// by a space ends it, and so does a flow indicator inside a flow
    /// collection.
    fn can_plain(&self, flow: bool) -> bool {
        let (c, next) = (self.at(0), self.at(1));
        !(c == b':' && (is_blankz(next) || (flow && is_flow(next))) || (flow && is_flow(c)))
    }
}

/// The length of the escape that starts with a backslash before `rest`,
/// or 0 if it isn't valid. A backslash at the end of a line joins the
/// next line to it, and the escape is just the backslash.
fn escape(rest: &[u8]) -> usize {
    let hex = |n: usize| rest.len() > n && rest[1..=n].iter().all(u8::is_ascii_hexdigit);
    match rest.first() {
        Some(
            b'0' | b'a' | b'b' | b't' | b'\t' | b'n' | b'v' | b'f' | b'r' | b'e' | b' ' | b'"'
            | b'/' | b'\\' | b'N' | b'_' | b'L' | b'P',
        ) => 2,
        Some(b'x') if hex(2) => 4,
        Some(b'u') if hex(4) => 6,
        Some(b'U') if hex(8) => 10,
        Some(b'\n' | b'\r') => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use saphyr_parser::{Event, Parser, ScalarStyle};

    use super::*;

    /// Each token but the identifiers, as its text and kind. Tokens of
    /// the same kind with nothing between them come out as one.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out.colored(text)
    }

    fn identifiers(text: &str) -> Vec<&str> {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out.identifiers(text)
    }

    fn kinds_of(text: &str, kind: Kind) -> Vec<&str> {
        tokens(text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_a_workflow() {
        use Kind::*;
        let text = "\
name: CI  # the build
on:
  push:
    branches: [main, 'release/*']
jobs:
  test:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo test
          cargo fmt --check
        env: {CI: true}
";
        assert_eq!(
            tokens(text),
            [
                (":", Punctuation),
                ("CI", String),
                ("# the build", Comment),
                (":", Punctuation),
                (":", Punctuation),
                (":", Punctuation),
                ("[", Punctuation),
                ("main", String),
                (",", Punctuation),
                ("'release/*'", String),
                ("]", Punctuation),
                (":", Punctuation),
                (":", Punctuation),
                (":", Punctuation),
                ("ubuntu-latest", String),
                (":", Punctuation),
                ("10", Number),
                (":", Punctuation),
                ("-", Punctuation),
                (":", Punctuation),
                ("actions/checkout@v4", String),
                ("-", Punctuation),
                (":", Punctuation),
                ("|", Punctuation),
                ("cargo test", String),
                ("cargo fmt --check", String),
                (":", Punctuation),
                ("{", Punctuation),
                (":", Punctuation),
                ("true", Constant),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn plain_scalars_follow_the_core_schema() {
        let text = "[1, -2, +3, 0o17, 0x1F, 1.5, .5, 1., 6.02e+23, .inf, -.Inf, .NaN, \
                    true, False, NULL, ~, null, yes, on, 0b1, 1_000, 1e, .nan., truex]";
        assert_eq!(
            kinds_of(text, Kind::Number),
            [
                "1", "-2", "+3", "0o17", "0x1F", "1.5", ".5", "1.", "6.02e+23", ".inf", "-.Inf",
                ".NaN"
            ]
        );
        assert_eq!(
            kinds_of(text, Kind::Constant),
            ["true", "False", "NULL", "~", "null"]
        );
        assert_eq!(
            kinds_of(text, Kind::String),
            ["yes", "on", "0b1", "1_000", "1e", ".nan.", "truex"]
        );
    }

    #[test]
    fn marks_plain_identifiers() {
        // Keys that look like values are identifiers, and so are keys
        // after `?` and the first node of each entry in a flow mapping.
        let text = "true: 1\n&a 2: b\n? null\n: x\n{c, d e: 3}";
        assert_eq!(identifiers(text), ["true", "2", "null", "c", "d e"]);
        assert_eq!(
            tokens(text),
            [
                (":", Kind::Punctuation),
                ("1", Kind::Number),
                ("&a", Kind::Attribute),
                (":", Kind::Punctuation),
                ("b", Kind::String),
                ("?", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("x", Kind::String),
                ("{", Kind::Punctuation),
                (",", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("3", Kind::Number),
                ("}", Kind::Punctuation),
            ]
        );
        // The quotes of a quoted key are plain, and the text between them
        // is an identifier, escapes and all. In a flow collection, a `:`
        // can follow one right away.
        let text = r#"{"a\tb":1, 'c''d':e, "":f, ? 'g', "h"}"#;
        assert_eq!(identifiers(text), [r"a\tb", "c''d", "g", "h"]);
        assert_eq!(
            tokens(text),
            [
                ("{", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("1", Kind::Number),
                (",", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("e", Kind::String),
                (",", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("f", Kind::String),
                (",", Kind::Punctuation),
                ("?", Kind::Punctuation),
                (",", Kind::Punctuation),
                ("}", Kind::Punctuation),
            ]
        );
        // A quoted key that goes on over several lines is a string, since
        // its name would be split over several tokens.
        assert_eq!(kinds_of("{\"a\n b\": 1}", Kind::String), ["\"a\n b\""]);
        // A `:` needs a space after it outside of flow collections, and a
        // key there has to end on its own line, within 1024 characters.
        assert_eq!(kinds_of("a:b\nc\n: d", Kind::String), ["a:b", "c", "d"]);
        assert_eq!(identifiers("a:b\nc\n: d"), [] as [&str; 0]);
        let long = "k".repeat(1030);
        assert_eq!(identifiers(&format!("{long}: v")), [] as [&str; 0]);
        // Inside a flow collection, a key can be longer, and it can go on
        // over several lines, each of which is an identifier. The scanner
        // reads one over several lines in a flow sequence too, although
        // the parser then rejects it.
        assert_eq!(identifiers(&format!("[{long}: v]")), [&*long]);
        assert_eq!(identifiers("{a\n b\n : c}"), ["a", "b"]);
        assert_eq!(identifiers("[a\n b: c]"), ["a", "b"]);
        // Inside a flow collection used as a key, words are values.
        assert_eq!(kinds_of("[a, b]: c", Kind::String), ["a", "b", "c"]);
        assert_eq!(identifiers("[a, b]: c"), [] as [&str; 0]);
    }

    #[test]
    fn plain_scalars_go_on_over_lines() {
        // The lines are joined with spaces, so the scalar is a string
        // even when its first line looks like a number. It ends at a line
        // that isn't indented enough, at a comment, and at `: `.
        assert_eq!(
            tokens("a: 1\n  2 # c\nb: x\n y\nz\n- p\n  q: r\ns: 3\n\nt: 4"),
            [
                (":", Kind::Punctuation),
                ("1", Kind::String),
                ("2", Kind::String),
                ("# c", Kind::Comment),
                (":", Kind::Punctuation),
                ("x", Kind::String),
                ("y", Kind::String),
                ("z", Kind::String),
                ("-", Kind::Punctuation),
                ("p", Kind::String),
                ("q", Kind::String),
                (":", Kind::Punctuation),
                ("r", Kind::String),
                (":", Kind::Punctuation),
                ("3", Kind::Number),
                (":", Kind::Punctuation),
                ("4", Kind::Number),
            ]
        );
        // The same goes for a scalar in a flow collection.
        assert_eq!(kinds_of("[true\n , 1\n 2]", Kind::Constant), ["true"]);
        assert_eq!(kinds_of("[true\n , 1\n 2]", Kind::String), ["1", "2"]);
        // A `#` without a space before it is part of the scalar.
        assert_eq!(tokens("a#b"), [("a#b", Kind::String)]);
    }

    #[test]
    fn quoted_scalars() {
        assert_eq!(
            tokens(
                r#"- "a\n\"\x41\u00e9\U0001F600\q"
- 'it''s \n'"#
            ),
            [
                ("-", Kind::Punctuation),
                ("\"a", Kind::String),
                ("\\n\\\"\\x41\\u00e9\\U0001F600", Kind::Escape),
                ("\\q\"", Kind::String),
                ("-", Kind::Punctuation),
                ("'it", Kind::String),
                ("''", Kind::Escape),
                ("s \\n'", Kind::String),
            ]
        );
        // Quoted scalars go on over lines, but not past a document marker.
        assert_eq!(
            tokens("a: \"b\\\n  c # d\"\ne: 'f\n---\ng"),
            [
                (":", Kind::Punctuation),
                ("\"b", Kind::String),
                ("\\", Kind::Escape),
                ("\n  c # d\"", Kind::String),
                (":", Kind::Punctuation),
                ("'f\n", Kind::String),
                ("---", Kind::Punctuation),
                ("g", Kind::String),
            ]
        );
    }

    #[test]
    fn block_scalars() {
        let text = "\
a: |-  # c
  x: y

   # z
b: >2
   q
c:
- |+
 r
d: |
";
        assert_eq!(
            tokens(text),
            [
                (":", Kind::Punctuation),
                ("|-", Kind::Punctuation),
                ("# c", Kind::Comment),
                ("x: y", Kind::String),
                (" # z", Kind::String),
                (":", Kind::Punctuation),
                (">2", Kind::Punctuation),
                (" q", Kind::String),
                (":", Kind::Punctuation),
                ("-", Kind::Punctuation),
                ("|+", Kind::Punctuation),
                ("r", Kind::String),
                (":", Kind::Punctuation),
                ("|", Kind::Punctuation),
            ]
        );
        // A block scalar at the top level can start in column 0, and a
        // document marker ends it.
        assert_eq!(
            tokens("--- |\nfoo\n...\n"),
            [
                ("---", Kind::Punctuation),
                ("|", Kind::Punctuation),
                ("foo", Kind::String),
                ("...", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn properties_and_directives() {
        assert_eq!(
            tokens("%YAML 1.2 # v\n--- !!map\n? &a !<tag:x> k\n: *a\nb: !e!t [!x , *a]\n"),
            [
                ("%YAML", Kind::Keyword),
                ("# v", Kind::Comment),
                ("---", Kind::Punctuation),
                ("!!map", Kind::Type),
                ("?", Kind::Punctuation),
                ("&a", Kind::Attribute),
                ("!<tag:x>", Kind::Type),
                (":", Kind::Punctuation),
                ("*a", Kind::Attribute),
                (":", Kind::Punctuation),
                ("!e!t", Kind::Type),
                ("[", Kind::Punctuation),
                ("!x", Kind::Type),
                (",", Kind::Punctuation),
                ("*a", Kind::Attribute),
                ("]", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn hunks_from_the_middle() {
        // A block scalar whose parent we can't see still ends where its
        // indentation does.
        assert_eq!(
            tokens("    run: |\n      x\n    c: 1"),
            [
                (":", Kind::Punctuation),
                ("|", Kind::Punctuation),
                ("x", Kind::String),
                (":", Kind::Punctuation),
                ("1", Kind::Number),
            ]
        );
    }

    fn pick(xs: &'static [&'static str]) -> BoxedStrategy<String> {
        prop::sample::select(xs).prop_map(str::to_string).boxed()
    }

    /// A node in a block collection. In an inline node, `\n` stands for a
    /// line break followed by enough indentation to go on with it.
    #[derive(Debug, Clone)]
    enum Node {
        Inline(String),
        /// A block scalar's header and lines.
        Block(String, Vec<String>),
        Map(Vec<(String, Node)>),
        Seq(Vec<Node>),
    }

    /// Scalars that can go anywhere, many of them on several lines.
    fn flow_scalar() -> BoxedStrategy<String> {
        let plain = pick(&[
            "a", "a b", "1", "-1.5", "+2", "0x1F", "0o7", "1e3", ".inf", "-.Inf", ".NaN", "true",
            "False", "Null", "~", "yes", "1_0", "-a", "?a", ":a", "a:b", "a'b", "a\"b", "a#b",
            "a\nb", "1\nb c", "a\n\nb", "x\n# c\n", "é", "a\t b",
        ]);
        let quoted = pick(&[
            "\"a b\"",
            r#""\n\"\x41é\U0001F600\\\/\ \_\e\0""#,
            "\"a # b\"",
            "\"\"",
            "''",
            "'it''s'",
            "'a: b'",
            "\"[{,}]\"",
            "'[{,}]'",
            "\"a\nb\"",
            "\"a\\\nb\"",
            "'a\n\nb'",
            "\"\\\"\"",
            "\"\\\\\"",
            "'\\'",
        ]);
        let props = pick(&["", "", "", "&a ", "!!str ", "!t ", "&b !x ", "!<tag:x> "]);
        let alias = pick(&["*a", "*b"]);
        prop_oneof![
            6 => (props, prop_oneof![plain, quoted]).prop_map(|(p, s)| p + &s),
            1 => alias,
        ]
        .boxed()
    }

    fn flow() -> BoxedStrategy<String> {
        flow_scalar()
            .prop_recursive(3, 12, 4, |inner| {
                let sep = pick(&[", ", ",", " , ", ",\n", ", # c\n"]);
                let seq =
                    prop::collection::vec((inner.clone(), sep.clone()), 0..4).prop_map(|items| {
                        let items: Vec<String> = items.into_iter().map(|(v, s)| v + &s).collect();
                        format!("[{}]", items.concat())
                    });
                let entry = prop_oneof![
                    (inner.clone(), pick(&[": ", ":\n", " : "]), inner.clone())
                        .prop_map(|(k, c, v)| k + &c + &v),
                    inner.clone(),
                    (pick(&["\"k\":", "'k':", "[k]:", "{k: v}:"]), inner.clone())
                        .prop_map(|(k, v)| k + &v),
                    (inner.clone(), inner).prop_map(|(k, v)| format!("? {k} : {v}")),
                ];
                let map = prop::collection::vec((entry, sep), 0..4).prop_map(|items| {
                    let items: Vec<String> = items.into_iter().map(|(v, s)| v + &s).collect();
                    format!("{{{}}}", items.concat())
                });
                prop_oneof![seq, map]
            })
            .boxed()
    }

    fn inline() -> BoxedStrategy<String> {
        let block_plain = pick(&["a [b]", "a, b", "a {b}", "a]", "-a, b"]);
        let comment = pick(&["", "", " # c", "  #: x"]);
        (prop_oneof![4 => flow(), 1 => block_plain], comment)
            .prop_map(|(v, c)| v + &c)
            .boxed()
    }

    fn node() -> BoxedStrategy<Node> {
        let block = (
            pick(&["|", ">", "|-", ">+", "|2", ">1-", "|+ # c"]),
            prop::collection::vec(
                pick(&[
                    "text",
                    "  more",
                    "",
                    "# no comment",
                    "- x",
                    "a: b",
                    "\"q",
                    "  ",
                    "\tt",
                ]),
                0..4,
            ),
        )
            .prop_map(|(h, lines)| Node::Block(h, lines));
        let leaf = prop_oneof![3 => inline().prop_map(Node::Inline), 1 => block];
        let key = pick(&[
            "a",
            "b c",
            "true",
            "1",
            "-x",
            "a:b",
            "\"q\"",
            "'s'",
            "\"a\\tb\"",
            "[x, y]",
            "{x: y}",
            "&k a",
            "!!str k",
            "é",
        ]);
        leaf.prop_recursive(4, 24, 4, move |inner| {
            prop_oneof![
                prop::collection::vec((key.clone(), inner.clone()), 0..4).prop_map(Node::Map),
                prop::collection::vec(inner, 0..4).prop_map(Node::Seq),
            ]
        })
        .boxed()
    }

    fn spaces(n: usize) -> String {
        " ".repeat(n)
    }

    /// Write `node` as the value after a key or `- ` that starts in
    /// column `pad`.
    fn value(node: &Node, pad: usize, out: &mut String) {
        match node {
            Node::Inline(s) => {
                out.push(' ');
                out.push_str(&s.replace('\n', &format!("\n{}", spaces(pad + 2))));
                out.push('\n');
            }
            Node::Block(header, lines) => {
                let indent = match header.bytes().find(u8::is_ascii_digit) {
                    Some(d) => pad + usize::from(d - b'0'),
                    None => pad + 2,
                };
                out.push_str(&format!(" {header}\n"));
                for l in lines {
                    if !l.is_empty() {
                        out.push_str(&spaces(indent));
                    }
                    out.push_str(l);
                    out.push('\n');
                }
            }
            Node::Map(entries) if entries.is_empty() => out.push_str(" {}\n"),
            Node::Seq(items) if items.is_empty() => out.push_str(" []\n"),
            Node::Map(entries) => {
                out.push('\n');
                map(entries, pad + 2, out);
            }
            Node::Seq(items) => {
                out.push('\n');
                seq(items, pad + 2, out);
            }
        }
    }

    fn map(entries: &[(String, Node)], pad: usize, out: &mut String) {
        for (k, v) in entries {
            out.push_str(&format!("{}{k}:", spaces(pad)));
            value(v, pad, out);
        }
    }

    fn seq(items: &[Node], pad: usize, out: &mut String) {
        for item in items {
            out.push_str(&spaces(pad));
            out.push('-');
            match item {
                // A mapping can start on the line of its `- `.
                Node::Map(entries) if !entries.is_empty() => {
                    let (k, v) = &entries[0];
                    out.push_str(&format!(" {k}:"));
                    value(v, pad + 2, out);
                    map(&entries[1..], pad + 2, out);
                }
                _ => value(item, pad, out),
            }
        }
    }

    /// A document without its markers. One with a scalar at the top
    /// starts with `---`, so that the scalar has a line to go on.
    fn document() -> impl Strategy<Value = String> {
        node().prop_map(|node| {
            let mut out = String::new();
            // Give the aliases something to point to.
            match &node {
                Node::Map(entries) if !entries.is_empty() => {
                    out.push_str("_a: &a 1\n_b: &b x\n");
                    map(entries, 0, &mut out);
                }
                Node::Seq(items) if !items.is_empty() => {
                    out.push_str("- &a 1\n- &b x\n");
                    seq(items, 0, &mut out);
                }
                _ => {
                    out.push_str("---");
                    value(&node, 0, &mut out);
                }
            }
            out
        })
    }

    /// Documents with their markers, maybe after a directive.
    fn stream() -> impl Strategy<Value = String> {
        let directive = pick(&["", "", "%YAML 1.2\n", "%TAG !e! tag:x,2000: # c\n"]);
        let doc = (
            pick(&["", "---\n", "--- # c\n"]),
            document(),
            pick(&["", "", "...\n", "... # c\n"]),
        );
        (directive, prop::collection::vec(doc, 1..3)).prop_map(|(directive, docs)| {
            let mut out = directive;
            for (start, doc, end) in docs {
                let start = if doc.starts_with("---") {
                    ""
                } else if start.is_empty() && !out.is_empty() {
                    // A document after a directive or another document
                    // needs a marker.
                    "---\n"
                } else {
                    &start
                };
                out.push_str(start);
                out.push_str(&doc);
                out.push_str(&end);
            }
            out
        })
    }

    fn is_space(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n' | b'\r')
    }

    /// Check the colors of a scalar against what saphyr-parser read.
    fn check_scalar(
        text: &str,
        kinds: &[Option<Kind>],
        covered: &mut [bool],
        mut r: Range<usize>,
        value: &str,
        style: ScalarStyle,
        key: bool,
    ) -> Result<(), TestCaseError> {
        let b = text.as_bytes();
        // The span starts with the scalar's anchor and tag, if it has them.
        let mut i = r.start;
        while i < r.end && matches!(b[i], b'&' | b'!') {
            while i < r.end && !is_space(b[i]) {
                i += 1;
            }
            while i < r.end && (is_space(b[i]) || b[i] == b'#') {
                if b[i] == b'#' {
                    while i < r.end && b[i] != b'\n' {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        }
        // And a block scalar's span starts with its header when it has
        // an anchor or a tag.
        if matches!(style, ScalarStyle::Literal | ScalarStyle::Folded)
            && i < r.end
            && matches!(b[i], b'|' | b'>')
        {
            i = text[i..r.end].find('\n').map_or(r.end, |n| i + n + 1);
        }
        // An empty node is a plain scalar whose span holds whatever comes
        // after it, such as a comma.
        if style == ScalarStyle::Plain
            && !text[i..r.end].starts_with(value.chars().next().unwrap_or(' '))
        {
            return Ok(());
        }
        // A quoted scalar's span goes on past its closing quote, over
        // spaces and comments.
        if let Some(q) = match style {
            ScalarStyle::SingleQuoted => Some(b'\''),
            ScalarStyle::DoubleQuoted => Some(b'"'),
            _ => None,
        } {
            let mut j = i + 1;
            while j < r.end {
                if b[j] == b'\\' && q == b'"' || b[j] == q && b.get(j + 1) == Some(&q) && q == b'\''
                {
                    j += 2;
                } else if b[j] == q {
                    break;
                } else {
                    j += 1;
                }
            }
            r.end = r.end.min(j + 1);
        }
        covered[r.clone()].fill(true);
        for j in i..r.end {
            if is_space(b[j]) {
                continue;
            }
            let ok = match style {
                ScalarStyle::Plain if key => kinds[j] == Some(Kind::Identifier),
                // A quoted key on one line is an identifier between plain
                // quotes. One over several lines is a string.
                ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted
                    if key && !text[i..r.end].contains('\n') =>
                {
                    if j == i || j + 1 == r.end {
                        kinds[j].is_none()
                    } else {
                        kinds[j] == Some(Kind::Identifier)
                    }
                }
                // A plain scalar over several lines is a string, since its
                // lines are joined with spaces.
                ScalarStyle::Plain => kinds[j] == Some(resolve(value)),
                ScalarStyle::Literal | ScalarStyle::Folded => kinds[j] == Some(Kind::String),
                ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted => {
                    matches!(kinds[j], Some(Kind::String | Kind::Escape))
                }
            };
            prop_assert!(
                ok,
                "{:?} ({style:?}, key: {key}) has {:?} at {j} in\n{text}",
                &text[r],
                kinds[j]
            );
        }
        Ok(())
    }

    /// Check the colors of a text against the events that saphyr-parser
    /// reads from it, if it reads it without an error.
    fn check(text: &str) -> Result<(), TestCaseError> {
        let Ok(events) = Parser::new_from_str(text).collect::<Result<Vec<_>, _>>() else {
            return Ok(());
        };
        let mut out = Tokens::default();
        lex(text, &mut out);
        let mut kinds = vec![None; text.len()];
        for (r, k) in out.0 {
            kinds[r].fill(Some(k));
        }
        // Spans count characters.
        let bytes: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        let mut covered = vec![false; text.len()];
        // For each open collection, whether it's a mapping whose next
        // node is a key.
        let mut open: Vec<Option<bool>> = Vec::new();
        for (event, span) in &events {
            let r = bytes[span.start.index()]..bytes[span.end.index()];
            let node = matches!(
                event,
                Event::Scalar(..)
                    | Event::Alias(_)
                    | Event::MappingStart(..)
                    | Event::SequenceStart(..)
            );
            let mut key = false;
            if node && let Some(Some(next_is_key)) = open.last_mut() {
                key = *next_is_key;
                *next_is_key = !key;
            }
            match event {
                Event::MappingStart(..) => open.push(Some(true)),
                Event::SequenceStart(..) => open.push(None),
                Event::MappingEnd | Event::SequenceEnd => {
                    open.pop();
                }
                Event::Alias(_) => {
                    for j in r {
                        covered[j] = true;
                        prop_assert!(
                            is_space(text.as_bytes()[j]) || kinds[j] == Some(Kind::Attribute),
                            "alias has {:?} at {j} in\n{text}",
                            kinds[j]
                        );
                    }
                }
                Event::Scalar(value, style, ..) => {
                    check_scalar(text, &kinds, &mut covered, r, value, *style, key)?;
                }
                _ => {}
            }
        }
        // Outside of scalars, nothing is a string, a number or a constant.
        for (j, k) in kinds.iter().enumerate() {
            prop_assert!(
                covered[j]
                    || !matches!(
                        k,
                        Some(Kind::String | Kind::Escape | Kind::Number | Kind::Constant)
                    ),
                "{k:?} at {j} outside of scalars in\n{text}"
            );
        }
        Ok(())
    }

    proptest! {
        /// In documents that saphyr-parser reads, every scalar has the
        /// color of its style or its type, and keys are identifiers.
        #[test]
        fn agrees_with_saphyr(text in stream()) {
            check(&text)?;
        }
    }
}
