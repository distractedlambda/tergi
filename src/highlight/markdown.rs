//! A lexer for Markdown.
//!
//! It follows CommonMark 0.31.2, along with the tables and task lists of
//! GitHub Flavored Markdown. Blocks are read a line at a time, the way the
//! CommonMark reference parser reads them. Then the text of paragraphs is
//! read for code spans, links, raw HTML, escapes and entities. Emphasis is
//! plain, because colors can't show it. Code blocks that name a language
//! we have a lexer for are lexed in that language.
//!
//! A line's colors only depend on the lines before it. Most of Markdown
//! works that way, but a code span or a link can go on over several lines.
//! Whether a backtick starts a code span depends on whether a closing one
//! comes later. So we lex each line of a paragraph as if the paragraph
//! ended there. A code span that covers several lines only gets its color
//! on the line where it ends.

use std::ops::Range;

use logos::Logos;

use super::{Kind, Language, Tokens};

/// How many lines of a paragraph we lex again for each line, at most.
/// Paragraphs are almost always shorter. A longer one could otherwise take
/// time that grows with the square of its length.
const WINDOW: usize = 64;

/// The colored pieces of a text, in no particular order.
type Out = Vec<(Range<usize>, Kind)>;

pub fn lex(text: &str, out: &mut Tokens) {
    let mut p = Parser::new(text);
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        p.line(start, line.strip_suffix('\n').unwrap_or(line));
        start += line.len();
    }
    p.close_leaf();
    let mut toks = p.out;
    toks.sort_by_key(|(r, _)| r.start);
    for (r, kind) in toks {
        if kind == Kind::Comment {
            out.push_comment(text, r);
        } else {
            out.push(r, kind);
        }
    }
}

/// The language that the info string of a fenced code block names, as in
/// ```` ```rust ```` or ```` ```rust,ignore ````.
fn for_info(info: &str) -> Option<Language> {
    let word = info
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .next()?
        .to_ascii_lowercase();
    match word.as_str() {
        "javascript" => Some(Language::JavaScript),
        "rust" => Some(Language::Rust),
        "racket" => Some(Language::Racket),
        "scribble" => Some(Language::Scribble),
        "rhombus" => Some(Language::Rhombus),
        "markdown" => Some(Language::Markdown),
        // Otherwise, a file extension such as `rs` or `toml`.
        _ => Language::for_path(&format!("a.{word}")),
    }
}

/// Where we are in a line, in bytes and in columns. Tabs stop every four
/// columns, and a container can use up part of a tab, as in `>\tcode`.
#[derive(Default)]
struct Cursor<'a> {
    s: &'a [u8],
    offset: usize,
    column: usize,
    /// The next character that isn't a space or tab, and its column.
    next: usize,
    next_column: usize,
    /// How many columns of spaces come before `next`.
    indent: usize,
    blank: bool,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor {
            s: s.as_bytes(),
            ..Cursor::default()
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.offset).copied()
    }

    fn find_next_nonspace(&mut self) {
        let mut i = self.offset;
        let mut cols = self.column;
        while let Some(&c) = self.s.get(i) {
            match c {
                b' ' => cols += 1,
                b'\t' => cols += 4 - cols % 4,
                _ => break,
            }
            i += 1;
        }
        self.blank = i == self.s.len();
        self.next = i;
        self.next_column = cols;
        self.indent = cols - self.column;
    }

    fn advance_next_nonspace(&mut self) {
        self.offset = self.next;
        self.column = self.next_column;
    }

    /// Move forward by `count` characters, or by `count` columns if
    /// `columns` is true. Counting columns can stop inside a tab.
    fn advance(&mut self, mut count: usize, columns: bool) {
        while count > 0
            && let Some(c) = self.peek()
        {
            if c == b'\t' {
                let to_tab = 4 - self.column % 4;
                if columns {
                    let n = to_tab.min(count);
                    self.column += n;
                    if n == to_tab {
                        self.offset += 1;
                    }
                    count -= n;
                } else {
                    self.column += to_tab;
                    self.offset += 1;
                    count -= 1;
                }
            } else {
                self.offset += 1;
                self.column += 1;
                count -= 1;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Container {
    Quote,
    /// A list item. The lines that go on with it are indented by `width`
    /// columns. One that holds nothing yet ends at a blank line.
    Item {
        width: usize,
        empty: bool,
    },
}

/// The block that a line of text can go into, inside the containers.
enum Leaf {
    Paragraph(Paragraph),
    Table,
    Fenced(Fenced),
    Indented,
    /// An HTML block, of one of the seven kinds that CommonMark numbers.
    Html {
        kind: u8,
        lines: Vec<Range<usize>>,
    },
}

/// What kind of leaf is open, without its contents.
#[derive(Clone, Copy)]
enum State {
    Paragraph,
    Table,
    Fenced { ch: u8, len: usize, indent: usize },
    Indented,
    Html(u8),
}

struct Paragraph {
    /// The text of each line, without the markers of its containers.
    lines: Vec<Range<usize>>,
    /// The first line that the next line has to be lexed with. Everything
    /// in the lines before it has been decided.
    restart: usize,
    /// Whether link reference definitions can start at `restart`.
    defs: bool,
}

struct Fenced {
    ch: u8,
    len: usize,
    indent: usize,
    lang: Option<Language>,
    /// The lines of code, kept until the block ends if we lex them.
    lines: Vec<Range<usize>>,
}

struct Parser<'a> {
    text: &'a str,
    out: Out,
    stack: Vec<Container>,
    leaf: Option<Leaf>,

    // The line being read.
    base: usize,
    body: &'a str,
    /// The end of the line, including a carriage return before its line
    /// feed.
    end: usize,
    c: Cursor<'a>,
    /// How many containers the line goes on with, whether it goes on with
    /// the leaf, and whether the blocks it doesn't go on with are closed.
    matched: usize,
    leaf_ok: bool,
    all_closed: bool,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            text,
            out: Vec::new(),
            stack: Vec::new(),
            leaf: None,
            base: 0,
            body: "",
            end: 0,
            c: Cursor::default(),
            matched: 0,
            leaf_ok: false,
            all_closed: true,
        }
    }

    /// Color a range of the line, given relative to its start.
    fn push(&mut self, r: Range<usize>, kind: Kind) {
        if r.start < r.end {
            self.out
                .push((self.base + r.start..self.base + r.end, kind));
        }
    }

    /// The end of the line without the spaces and tabs at its end.
    fn trimmed_end(&self) -> usize {
        self.body.trim_end_matches([' ', '\t']).len()
    }

    fn state(&self) -> Option<State> {
        Some(match self.leaf.as_ref()? {
            Leaf::Paragraph(_) => State::Paragraph,
            Leaf::Table => State::Table,
            Leaf::Fenced(f) => State::Fenced {
                ch: f.ch,
                len: f.len,
                indent: f.indent,
            },
            Leaf::Indented => State::Indented,
            Leaf::Html { kind, .. } => State::Html(*kind),
        })
    }

    /// Read one line, without its line feed. This follows `incorporateLine`
    /// in commonmark.js, the reference parser.
    fn line(&mut self, base: usize, raw: &'a str) {
        let body = raw.strip_suffix('\r').unwrap_or(raw);
        self.base = base;
        self.body = body;
        self.end = base + raw.len();
        self.c = Cursor::new(body);

        // See which containers the line goes on with.
        self.matched = 0;
        while self.matched < self.stack.len() {
            self.c.find_next_nonspace();
            let ok = match self.stack[self.matched] {
                Container::Quote => self.quote_marker(),
                Container::Item { width, empty } => {
                    if self.c.blank {
                        self.c.advance_next_nonspace();
                        !empty
                    } else if self.c.indent >= width {
                        self.c.advance(width, true);
                        true
                    } else {
                        false
                    }
                }
            };
            if !ok {
                break;
            }
            self.matched += 1;
        }

        // Then whether it goes on with the leaf.
        self.leaf_ok = false;
        let state = self.state();
        if self.matched == self.stack.len()
            && let Some(state) = state
        {
            self.c.find_next_nonspace();
            self.leaf_ok = match state {
                State::Paragraph | State::Table => !self.c.blank,
                State::Fenced { ch, len, indent } => {
                    if self.c.indent <= 3 && closing_fence(&body[self.c.next..], ch, len) {
                        self.push(self.c.next..self.trimmed_end(), Kind::Punctuation);
                        self.close_leaf();
                        return;
                    }
                    let mut i = indent;
                    while i > 0 && matches!(self.c.peek(), Some(b' ' | b'\t')) {
                        self.c.advance(1, true);
                        i -= 1;
                    }
                    true
                }
                State::Indented => {
                    if self.c.indent >= 4 {
                        self.c.advance(4, true);
                        true
                    } else if self.c.blank {
                        self.c.advance_next_nonspace();
                        true
                    } else {
                        false
                    }
                }
                State::Html(kind) => !(self.c.blank && kind >= 6),
            };
        }
        self.all_closed = self.matched == self.stack.len() && (state.is_none() || self.leaf_ok);

        // Look for the starts of new blocks, unless the line goes on with a
        // block that takes its lines as they are.
        let mut in_paragraph = self.leaf_ok && matches!(state, Some(State::Paragraph));
        let verbatim = self.leaf_ok
            && matches!(
                state,
                Some(State::Fenced { .. } | State::Indented | State::Html(_))
            );
        loop {
            if verbatim {
                break;
            }
            self.c.find_next_nonspace();
            let next = self.c.next;
            let rest = &body[next..];
            if self.c.indent >= 4 {
                // Indented code can't interrupt a paragraph. In
                // pulldown-cmark, it can't interrupt a table either.
                if !matches!(self.leaf, Some(Leaf::Paragraph(_) | Leaf::Table)) && !self.c.blank {
                    self.c.advance(4, true);
                    self.close_unmatched();
                    self.open_leaf(Leaf::Indented);
                } else {
                    self.c.advance_next_nonspace();
                }
                break;
            }
            if rest.starts_with('>') {
                self.close_unmatched();
                self.quote_marker();
                self.open_container(Container::Quote);
                in_paragraph = false;
                continue;
            }
            if atx_heading(rest) {
                self.close_unmatched();
                self.add_child();
                self.push(next..self.trimmed_end(), Kind::Keyword);
                return;
            }
            if let Some((ch, len)) = opening_fence(rest) {
                self.close_unmatched();
                self.push(next..next + len, Kind::Punctuation);
                let info = rest[len..].trim_matches([' ', '\t']);
                let at = next + len + rest[len..].find(info).unwrap_or(0);
                self.push(at..at + info.len(), Kind::Attribute);
                let indent = self.c.indent;
                self.open_leaf(Leaf::Fenced(Fenced {
                    ch,
                    len,
                    indent,
                    lang: for_info(info),
                    lines: Vec::new(),
                }));
                return;
            }
            if let Some(kind) = html_start(rest) {
                let lazy = !self.all_closed
                    && !self.c.blank
                    && matches!(self.leaf, Some(Leaf::Paragraph(_)));
                // The seventh kind can't interrupt a paragraph.
                if kind < 7 || !(in_paragraph || lazy) {
                    self.close_unmatched();
                    self.open_leaf(Leaf::Html {
                        kind,
                        lines: Vec::new(),
                    });
                    break;
                }
            }
            if in_paragraph && setext_underline(rest) && !self.all_definitions() {
                self.push(next..self.trimmed_end(), Kind::Keyword);
                self.leaf = None;
                return;
            }
            if thematic_break(rest) {
                self.close_unmatched();
                self.add_child();
                self.push(next..self.trimmed_end(), Kind::Punctuation);
                return;
            }
            if self.list_marker(in_paragraph) {
                in_paragraph = false;
                continue;
            }
            // The last line of a paragraph can be the header row of a table, unless
            // it's a list item, as in `- |`.
            if in_paragraph
                && let Some(Leaf::Paragraph(p)) = &self.leaf
                && let Some(header) = p.lines.last()
                && let Some(n) = delimiter_row(rest)
                && n == cells(&self.text[header.clone()])
            {
                self.push(next..self.trimmed_end(), Kind::Punctuation);
                self.leaf = Some(Leaf::Table);
                return;
            }
            self.c.advance_next_nonspace();
            break;
        }

        // What's left of the line is text. A line that doesn't go on with
        // the containers of a paragraph can still go on with the
        // paragraph, as a lazy continuation line.
        if !self.all_closed && !self.c.blank && matches!(self.leaf, Some(Leaf::Paragraph(_))) {
            self.paragraph_line();
            return;
        }
        self.close_unmatched();
        let at = self.base + self.c.offset;
        match self.state() {
            Some(State::Paragraph) => self.paragraph_line(),
            Some(State::Table) => self.table_row(),
            Some(State::Fenced { .. }) => {
                if let Some(Leaf::Fenced(f)) = &mut self.leaf
                    && f.lang.is_some()
                {
                    f.lines.push(at..self.end);
                } else {
                    self.push_code(at..self.end);
                }
            }
            Some(State::Indented) => self.push_code(at..self.end),
            Some(State::Html(kind)) => {
                if let Some(Leaf::Html { lines, .. }) = &mut self.leaf {
                    lines.push(at..self.end);
                }
                if html_end(kind, &self.text[at..self.end]) {
                    self.close_leaf();
                }
            }
            None if !self.c.blank => {
                let task = matches!(self.stack.last(), Some(Container::Item { empty: true, .. }))
                    && task_marker(&body[self.c.offset..]);
                if task {
                    let at = self.c.offset;
                    self.push(at..at + 3, Kind::Punctuation);
                    self.c.advance(3, false);
                    self.c.find_next_nonspace();
                    self.c.advance_next_nonspace();
                    // A task with nothing after its marker has no paragraph,
                    // and pulldown-cmark counts its item as empty, so that a
                    // blank line ends it.
                    if self.c.blank {
                        return;
                    }
                }
                self.open_leaf(Leaf::Paragraph(Paragraph {
                    lines: Vec::new(),
                    restart: 0,
                    defs: !task,
                }));
                self.paragraph_line();
            }
            None => {}
        }
    }

    /// Read a `>` that starts or goes on with a block quote, and the space
    /// after it.
    fn quote_marker(&mut self) -> bool {
        if self.c.indent > 3 || self.c.s.get(self.c.next) != Some(&b'>') {
            return false;
        }
        self.push(self.c.next..self.c.next + 1, Kind::Punctuation);
        self.c.advance_next_nonspace();
        self.c.advance(1, false);
        if matches!(self.c.peek(), Some(b' ' | b'\t')) {
            self.c.advance(1, true);
        }
        true
    }

    /// Start a list item if the line has a list marker. This follows
    /// `parseListMarker` in commonmark.js.
    fn list_marker(&mut self, in_paragraph: bool) -> bool {
        let rest = &self.body[self.c.next..];
        let b = rest.as_bytes();
        let len = match b.first() {
            Some(b'*' | b'+' | b'-') => 1,
            Some(b'0'..=b'9') => {
                let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
                // Only a list that starts at 1 can interrupt a paragraph.
                if digits > 9
                    || !matches!(b.get(digits), Some(b'.' | b')'))
                    || (in_paragraph && rest[..digits].parse() != Ok(1))
                {
                    return false;
                }
                digits + 1
            }
            _ => return false,
        };
        if !matches!(b.get(len), None | Some(b' ' | b'\t')) {
            return false;
        }
        // Nor can an empty list item.
        if in_paragraph && rest[len..].trim_matches([' ', '\t']).is_empty() {
            return false;
        }
        self.close_unmatched();
        let marker = self.c.next;
        let marker_offset = self.c.indent;
        self.c.advance_next_nonspace();
        self.c.advance(len, true);
        let (column, offset) = (self.c.column, self.c.offset);
        loop {
            self.c.advance(1, true);
            if self.c.column - column >= 5 || !matches!(self.c.peek(), Some(b' ' | b'\t')) {
                break;
            }
        }
        let spaces = self.c.column - column;
        // With five or more spaces after the marker, or none before the end
        // of the line, the content starts one space after the marker.
        let padding = if !(1..5).contains(&spaces) || self.c.peek().is_none() {
            self.c.column = column;
            self.c.offset = offset;
            if matches!(self.c.peek(), Some(b' ' | b'\t')) {
                self.c.advance(1, true);
            }
            len + 1
        } else {
            len + spaces
        };
        self.push(marker..marker + len, Kind::Punctuation);
        self.open_container(Container::Item {
            width: marker_offset + padding,
            empty: true,
        });
        true
    }

    /// Close the containers and the leaf that the line doesn't go on with.
    fn close_unmatched(&mut self) {
        if !self.all_closed {
            if !self.leaf_ok {
                self.close_leaf();
            }
            self.stack.truncate(self.matched);
            self.all_closed = true;
        }
    }

    /// Close the leaf to make room for another block in the innermost
    /// container, which then isn't empty.
    fn add_child(&mut self) {
        self.close_leaf();
        if let Some(Container::Item { empty, .. }) = self.stack.last_mut() {
            *empty = false;
        }
    }

    fn open_container(&mut self, c: Container) {
        self.add_child();
        self.stack.push(c);
        self.matched = self.stack.len();
    }

    fn open_leaf(&mut self, leaf: Leaf) {
        self.add_child();
        self.leaf = Some(leaf);
        self.leaf_ok = true;
    }

    /// Lex the lines of a block that we only lex once it ends.
    fn close_leaf(&mut self) {
        match self.leaf.take() {
            Some(Leaf::Fenced(Fenced {
                lang: Some(lang),
                lines,
                ..
            })) => {
                let joined = Joined::new(self.text, &lines, true);
                for (r, kind) in lang.lex(&joined.text).0 {
                    joined.map(r, kind, &mut self.out);
                }
            }
            Some(Leaf::Html { lines, .. }) => {
                let joined = Joined::new(self.text, &lines, true);
                let mut toks = Vec::new();
                html(&joined.text, 0, &mut toks);
                for (r, kind) in toks {
                    joined.map(r, kind, &mut self.out);
                }
            }
            _ => {}
        }
    }

    /// Color a line of code that we don't know the language of.
    fn push_code(&mut self, r: Range<usize>) {
        let end = r.start + self.text[r.clone()].trim_end().len();
        if r.start < end {
            self.out.push((r.start..end, Kind::String));
        }
    }

    /// Whether the paragraph is all link reference definitions, so that
    /// an underline after it doesn't make it a heading.
    fn all_definitions(&self) -> bool {
        let Some(Leaf::Paragraph(p)) = &self.leaf else {
            return false;
        };
        let joined = Joined::new(self.text, &p.lines, false);
        let mut pos = 0;
        while pos < joined.text.len() {
            match definition(&joined.text, pos, &mut Vec::new()) {
                Def::Yes { end, .. } => pos = end,
                Def::No { .. } => return false,
            }
        }
        true
    }

    /// Add a line to the paragraph and color it. We lex the paragraph from
    /// the start, or from the last line where nothing was left undecided,
    /// up to the end of this line.
    fn paragraph_line(&mut self) {
        let line = self.base + self.c.offset..self.base + self.body.len();
        let Some(Leaf::Paragraph(p)) = &mut self.leaf else {
            return;
        };
        p.lines.push(line);
        let k = p.lines.len() - 1;
        if k + 1 - p.restart > WINDOW {
            p.restart = k + 1 - WINDOW;
            p.defs = false;
        }
        let joined = Joined::new(self.text, &p.lines[p.restart..], false);
        let mut toks = Vec::new();
        let scan = paragraph(&joined.text, p.defs, &mut toks);
        let start = joined.last_start();
        for (r, kind) in toks {
            if r.end > start {
                joined.map(r.start.max(start)..r.end, kind, &mut self.out);
            }
        }
        if scan.clean {
            p.restart = k + 1;
            p.defs = scan.defs;
        }
    }

    /// Color a row of a table. Each cell is lexed on its own.
    fn table_row(&mut self) {
        let start = self.base + self.c.offset;
        let row = &self.text[start..self.base + self.body.len()];
        let b = row.as_bytes();
        let mut cell = 0;
        let mut i = 0;
        while i <= b.len() {
            match b.get(i) {
                Some(b'\\') => i += 1,
                Some(b'|') | None => {
                    let mut toks = Vec::new();
                    inline(&row[cell..i], 0, &mut toks);
                    for (r, kind) in toks {
                        self.out
                            .push((start + cell + r.start..start + cell + r.end, kind));
                    }
                    if i < b.len() {
                        self.out.push((start + i..start + i + 1, Kind::Punctuation));
                    }
                    cell = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Lines of the source joined into one text with line feeds, so that they
/// can be lexed without the markers of their containers.
struct Joined {
    text: String,
    /// Where each line starts in `text`, and its range in the source.
    lines: Vec<(usize, Range<usize>)>,
}

impl Joined {
    fn new(src: &str, lines: &[Range<usize>], newline_at_end: bool) -> Self {
        let mut text = String::new();
        let mut out = Vec::with_capacity(lines.len());
        for (i, r) in lines.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            out.push((text.len(), r.clone()));
            text.push_str(&src[r.clone()]);
        }
        if newline_at_end && !lines.is_empty() {
            text.push('\n');
        }
        Joined { text, lines: out }
    }

    fn last_start(&self) -> usize {
        self.lines.last().map_or(0, |(at, _)| *at)
    }

    /// Color the source for a range of the joined text, one piece per line.
    fn map(&self, r: Range<usize>, kind: Kind, out: &mut Out) {
        let first = self
            .lines
            .partition_point(|(at, src)| at + src.len() <= r.start);
        for (at, src) in &self.lines[first..] {
            if *at >= r.end {
                break;
            }
            let s = r.start.max(*at);
            let e = r.end.min(at + src.len());
            if s < e {
                out.push((src.start + s - at..src.start + e - at, kind));
            }
        }
    }
}

fn atx_heading(rest: &str) -> bool {
    let n = rest.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&n) && matches!(rest.as_bytes().get(n), None | Some(b' ' | b'\t'))
}

fn thematic_break(rest: &str) -> bool {
    let Some(c @ (b'*' | b'-' | b'_')) = rest.bytes().next() else {
        return false;
    };
    let mut n = 0;
    for b in rest.bytes() {
        if b == c {
            n += 1;
        } else if b != b' ' && b != b'\t' {
            return false;
        }
    }
    n >= 3
}

fn setext_underline(rest: &str) -> bool {
    let Some(c @ (b'=' | b'-')) = rest.bytes().next() else {
        return false;
    };
    rest.trim_end_matches([' ', '\t']).bytes().all(|b| b == c)
}

/// The character and length of a fence that opens a code block. A fence of
/// backticks can't have a backtick in its info string.
fn opening_fence(rest: &str) -> Option<(u8, usize)> {
    let c = *rest.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = rest.bytes().take_while(|&b| b == c).count();
    (n >= 3 && (c == b'~' || !rest[n..].contains('`'))).then_some((c, n))
}

fn closing_fence(rest: &str, ch: u8, len: usize) -> bool {
    let n = rest.bytes().take_while(|&b| b == ch).count();
    n >= len && rest[n..].bytes().all(|b| b == b' ' || b == b'\t')
}

/// A task list marker, such as `[x]`, at the start of a list item.
fn task_marker(rest: &str) -> bool {
    let b = rest.as_bytes();
    b.len() >= 3
        && b[0] == b'['
        && matches!(b[1], b' ' | b'x' | b'X')
        && b[2] == b']'
        && matches!(b.get(3), None | Some(b' ' | b'\t'))
}

/// The number of cells in a table's delimiter row, such as `| --- | :-: |`.
/// It needs at least one pipe, or it would be a setext heading's underline
/// or a thematic break.
fn delimiter_row(rest: &str) -> Option<usize> {
    let row = rest.trim_end_matches([' ', '\t']);
    if !row.contains('|') {
        return None;
    }
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').unwrap_or(row);
    let mut n = 0;
    for cell in row.split('|') {
        let c = cell.trim_matches([' ', '\t']);
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        if c.is_empty() || c.bytes().any(|b| b != b'-') {
            return None;
        }
        n += 1;
    }
    Some(n)
}

/// The number of cells in a table row. A pipe after a backslash is part of
/// a cell, and so are pipes at the start and end of the row.
fn cells(row: &str) -> usize {
    let row = row.trim_matches([' ', '\t']);
    let row = row.strip_prefix('|').unwrap_or(row);
    let b = row.as_bytes();
    let mut n = 1;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'|' if i + 1 < b.len() => n += 1,
            _ => {}
        }
        i += 1;
    }
    n
}

/// The tags that start an HTML block of the sixth kind.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// The tags that start an HTML block of the first kind, which holds text
/// that isn't Markdown.
const RAW_TAGS: &[&str] = &["pre", "script", "style", "textarea"];

/// The kind of HTML block that a line starts, numbered as in CommonMark.
fn html_start(rest: &str) -> Option<u8> {
    let after = rest.strip_prefix('<')?;
    let b = after.as_bytes();
    let name_len = |s: &str| s.bytes().take_while(u8::is_ascii_alphanumeric).count();
    let n = name_len(after);
    if RAW_TAGS.iter().any(|t| t.eq_ignore_ascii_case(&after[..n]))
        && matches!(b.get(n), None | Some(b' ' | b'\t' | b'>'))
    {
        return Some(1);
    }
    if after.starts_with("!--") {
        return Some(2);
    }
    if after.starts_with('?') {
        return Some(3);
    }
    if after.starts_with('!') && b.get(1).is_some_and(u8::is_ascii_alphabetic) {
        return Some(4);
    }
    if after.starts_with("![CDATA[") {
        return Some(5);
    }
    let tag = after.strip_prefix('/').unwrap_or(after);
    let n = name_len(tag);
    if BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(&tag[..n])) {
        let r = &tag[n..];
        if r.is_empty() || r.starts_with([' ', '\t', '>']) || r.starts_with("/>") {
            return Some(6);
        }
    }
    // A complete open or closing tag alone on its line, with any name but
    // those of the first kind.
    let name = &tag[..tag
        .bytes()
        .take_while(|&c| c.is_ascii_alphanumeric() || c == b'-')
        .count()];
    if (b.first().is_some_and(u8::is_ascii_alphabetic) || after.starts_with('/'))
        && !RAW_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name))
        && let Found::Yes(e) = inline_html(rest, 0)
        && rest[e..].trim_matches([' ', '\t']).is_empty()
    {
        return Some(7);
    }
    None
}

/// Whether a line ends an HTML block of the first five kinds.
fn html_end(kind: u8, line: &str) -> bool {
    match kind {
        1 => {
            let l = line.to_ascii_lowercase();
            RAW_TAGS.iter().any(|t| l.contains(&format!("</{t}>")))
        }
        2 => line.contains("-->"),
        3 => line.contains("?>"),
        4 => line.contains('>'),
        5 => line.contains("]]>"),
        _ => false,
    }
}

/// The result of reading a construct that may go on to later lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// It's there, and it ends here.
    Yes(usize),
    No,
    /// The text ended before we could tell. Another line could finish it.
    Eof,
}

/// What lexing a paragraph up to some line found out.
struct Scan {
    /// Whether everything is decided, so that more lines can't change the
    /// colors of the text so far.
    clean: bool,
    /// Whether the text was all link reference definitions, so that another
    /// can follow.
    defs: bool,
}

fn paragraph(s: &str, mut defs: bool, out: &mut Out) -> Scan {
    let mut pos = 0;
    let mut clean = true;
    while defs && pos < s.len() {
        match definition(s, pos, out) {
            Def::Yes { end, open } => {
                pos = end;
                clean &= !open;
            }
            Def::No { open } => {
                clean &= !open;
                defs = false;
            }
        }
    }
    clean &= inline(s, pos, out);
    Scan { clean, defs }
}

enum Def {
    Yes { end: usize, open: bool },
    No { open: bool },
}

/// A link reference definition at `p`, as in `[label]: /url "title"`. It
/// ends after its line feed. It's `open` if more text could change it, as
/// when a title could follow on the next line.
fn definition(s: &str, p: usize, out: &mut Out) -> Def {
    let b = s.as_bytes();
    let len = b.len();
    let close = match label(s, p) {
        Found::Yes(e) => e,
        Found::No => return Def::No { open: false },
        Found::Eof => return Def::No { open: true },
    };
    if b.get(close + 1) != Some(&b':') {
        return Def::No { open: false };
    }
    let start = ws(s, close + 2);
    if start == len {
        return Def::No { open: true };
    }
    let end = match destination(s, start) {
        Found::Yes(e) => e,
        Found::No => return Def::No { open: false },
        Found::Eof => return Def::No { open: true },
    };
    let after = spaces(s, end);
    let line_ends = after == len || b[after] == b'\n';
    let line_end = (after + 1).min(len);
    let mut toks = vec![
        (p..p + 1, Kind::Punctuation),
        (close..close + 2, Kind::Punctuation),
        (start..end, Kind::Attribute),
    ];
    let t = ws(s, end);
    let mut open = after == len;
    if t > end && t < len && matches!(b[t], b'"' | b'\'' | b'(') {
        match title(s, t) {
            Found::Yes(te) => {
                let e = spaces(s, te);
                if e == len || b[e] == b'\n' {
                    toks.push((t..te, Kind::String));
                    out.extend(toks);
                    return Def::Yes {
                        end: (e + 1).min(len),
                        open: false,
                    };
                }
            }
            Found::Eof => open = true,
            Found::No => {}
        }
    }
    if !line_ends {
        return Def::No { open };
    }
    out.extend(toks);
    Def::Yes {
        end: line_end,
        open,
    }
}

/// Skip spaces and tabs.
fn spaces(s: &str, mut p: usize) -> usize {
    let b = s.as_bytes();
    while p < b.len() && matches!(b[p], b' ' | b'\t') {
        p += 1;
    }
    p
}

/// Skip spaces, tabs and a line feed.
fn ws(s: &str, p: usize) -> usize {
    let p = spaces(s, p);
    if s.as_bytes().get(p) == Some(&b'\n') {
        spaces(s, p + 1)
    } else {
        p
    }
}

fn is_punct(c: u8) -> bool {
    c.is_ascii_punctuation()
}

/// A link label in brackets at `p`. Gives the position of the closing
/// bracket.
fn label(s: &str, p: usize) -> Found {
    let b = s.as_bytes();
    if b.get(p) != Some(&b'[') {
        return Found::No;
    }
    let mut q = p + 1;
    loop {
        match b.get(q) {
            None => return Found::Eof,
            Some(b'\\') => q += 2,
            Some(b'[') => return Found::No,
            Some(b']') => break,
            Some(_) => q += 1,
        }
        if q - p > 1000 {
            return Found::No;
        }
    }
    if s[p + 1..q].trim().is_empty() {
        return Found::No;
    }
    Found::Yes(q)
}

/// A link destination at `p`, as in `/url` or `<url>`.
fn destination(s: &str, p: usize) -> Found {
    let b = s.as_bytes();
    if b.get(p) == Some(&b'<') {
        let mut q = p + 1;
        loop {
            match b.get(q) {
                None => return Found::Eof,
                Some(b'\n' | b'<') => return Found::No,
                Some(b'\\') => q += 2,
                Some(b'>') => return Found::Yes(q + 1),
                Some(_) => q += 1,
            }
        }
    }
    let mut q = p;
    let mut depth = 0;
    while let Some(&c) = b.get(q) {
        match c {
            0..=b' ' | 0x7f => break,
            b'\\' if b.get(q + 1).copied().is_some_and(is_punct) => q += 1,
            b'(' => {
                depth += 1;
                if depth > 32 {
                    return Found::No;
                }
            }
            b')' if depth == 0 => break,
            b')' => depth -= 1,
            _ => {}
        }
        q += 1;
    }
    if q == p || depth != 0 {
        return Found::No;
    }
    Found::Yes(q)
}

/// A link title at `p`, in quotes or parentheses.
fn title(s: &str, p: usize) -> Found {
    let b = s.as_bytes();
    let close = match b.get(p) {
        Some(b'(') => b')',
        Some(&c @ (b'"' | b'\'')) => c,
        _ => return Found::No,
    };
    let mut q = p + 1;
    loop {
        match b.get(q) {
            None => return Found::Eof,
            Some(b'\\') => q += 2,
            Some(&c) if c == close => return Found::Yes(q + 1),
            Some(b'(') if close == b')' => return Found::No,
            Some(_) => q += 1,
        }
    }
}

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Inline {
    #[regex(r"\\[!-/:-@\[-`{-~]")]
    #[regex(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});")]
    Escape,
    #[regex("`+")]
    Backticks,
    #[token("<")]
    Angle,
    #[token("![")]
    Image,
    #[token("[")]
    Open,
    #[token("]")]
    Close,
    #[regex(r"[^\\`<&!\[\]]+")]
    #[token("\\")]
    #[token("&")]
    #[token("!")]
    Text,
}

/// Color code spans, links, raw HTML, escapes and entities in `s`, from
/// `from` on, following the CommonMark rules for inline content. Returns
/// whether everything is decided, so that more text can't change it.
fn inline(s: &str, from: usize, out: &mut Out) -> bool {
    struct Opener {
        at: usize,
        image: bool,
        active: bool,
    }
    let mut clean = true;
    let mut openers: Vec<Opener> = Vec::new();
    let mut lex = Inline::lexer(s);
    lex.bump(from);
    while let Some(tok) = lex.next() {
        let r = lex.span();
        match tok {
            Ok(Inline::Escape) => out.push((r, Kind::Escape)),
            Ok(Inline::Backticks) => match closing_backticks(lex.remainder(), r.len()) {
                Some(i) => {
                    lex.bump(i + r.len());
                    out.push((r.start..lex.span().end, Kind::String));
                }
                None => clean = false,
            },
            Ok(Inline::Angle) => {
                if let Some(e) = autolink(s, r.start) {
                    out.push((r.start..r.end, Kind::Punctuation));
                    out.push((r.end..e - 1, Kind::Attribute));
                    out.push((e - 1..e, Kind::Punctuation));
                    lex.bump(e - r.end);
                } else {
                    match inline_html(s, r.start) {
                        Found::Yes(e) => {
                            html(&s[r.start..e], r.start, out);
                            lex.bump(e - r.end);
                        }
                        Found::Eof => clean = false,
                        Found::No => {}
                    }
                }
            }
            Ok(Inline::Image) => openers.push(Opener {
                at: r.start,
                image: true,
                active: true,
            }),
            Ok(Inline::Open) => openers.push(Opener {
                at: r.start,
                image: false,
                active: true,
            }),
            Ok(Inline::Close) => {
                let Some(op) = openers.pop() else {
                    continue;
                };
                if !op.active {
                    continue;
                }
                let mut tail = Vec::new();
                match link_tail(s, r.end, &mut tail) {
                    Found::Yes(e) => {
                        let len = if op.image { 2 } else { 1 };
                        out.push((op.at..op.at + len, Kind::Punctuation));
                        out.push((r.clone(), Kind::Punctuation));
                        out.extend(tail);
                        lex.bump(e - r.end);
                        // Links can't hold other links.
                        if !op.image {
                            for o in &mut openers {
                                o.active &= o.image;
                            }
                        }
                    }
                    Found::Eof => clean = false,
                    Found::No => {}
                }
            }
            _ => {}
        }
    }
    clean && openers.is_empty()
}

/// Where the first run of exactly `n` backticks starts in `rest`.
fn closing_backticks(rest: &str, n: usize) -> Option<usize> {
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            let run = b[i..].iter().take_while(|&&c| c == b'`').count();
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// What follows the `]` of a link: a destination in parentheses, or the
/// label of a reference in brackets. We don't know which labels the
/// document defines, since a definition can come after its use. So we take
/// any label as defined, except in a link like `[foo]` that is only a
/// label, which more often isn't a link at all.
fn link_tail(s: &str, p: usize, out: &mut Out) -> Found {
    let b = s.as_bytes();
    match b.get(p) {
        Some(b'(') => {}
        Some(b'[') => {
            if b.get(p + 1) == Some(&b']') {
                out.push((p..p + 2, Kind::Punctuation));
                return Found::Yes(p + 2);
            }
            let close = match label(s, p) {
                Found::Yes(close) => close,
                other => return other,
            };
            out.push((p..p + 1, Kind::Punctuation));
            out.push((p + 1..close, Kind::Attribute));
            out.push((close..close + 1, Kind::Punctuation));
            return Found::Yes(close + 1);
        }
        _ => return Found::No,
    }
    let mut toks = vec![(p..p + 1, Kind::Punctuation)];
    let mut q = ws(s, p + 1);
    if q == b.len() {
        return Found::Eof;
    }
    if b[q] != b')' {
        let end = match destination(s, q) {
            Found::Yes(e) => e,
            other => return other,
        };
        toks.push((q..end, Kind::Attribute));
        q = ws(s, end);
        if q == b.len() {
            return Found::Eof;
        }
        if q > end && matches!(b[q], b'"' | b'\'' | b'(') {
            let te = match title(s, q) {
                Found::Yes(te) => te,
                other => return other,
            };
            toks.push((q..te, Kind::String));
            q = ws(s, te);
            if q == b.len() {
                return Found::Eof;
            }
        }
    }
    if b[q] != b')' {
        return Found::No;
    }
    toks.push((q..q + 1, Kind::Punctuation));
    out.extend(toks);
    Found::Yes(q + 1)
}

/// An autolink at `p`, as in `<https://example.com>` or `<me@example.com>`.
/// It can't hold a line feed, so it never needs the next line.
fn autolink(s: &str, p: usize) -> Option<usize> {
    let b = &s.as_bytes()[p + 1..];
    let end = |i: usize| Some(p + 1 + i + 1);
    let scheme = b
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        .count();
    if (2..=32).contains(&scheme) && b[0].is_ascii_alphabetic() && b.get(scheme) == Some(&b':') {
        let i = scheme
            + 1
            + b[scheme + 1..]
                .iter()
                .position(|&c| c <= b' ' || c == b'<' || c == b'>' || c == 0x7f)?;
        return if b[i] == b'>' { end(i) } else { None };
    }
    let local = b
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(c))
        .count();
    if local == 0 || b.get(local) != Some(&b'@') {
        return None;
    }
    let mut i = local + 1;
    loop {
        let n = b[i..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
            .count();
        if n == 0 || n > 63 || b[i] == b'-' || b[i + n - 1] == b'-' {
            return None;
        }
        i += n;
        match b.get(i) {
            Some(b'.') => i += 1,
            Some(b'>') => return end(i),
            _ => return None,
        }
    }
}

/// Raw HTML at `p`, as CommonMark defines it: an open or closing tag, a
/// comment, a processing instruction, a declaration or a CDATA section.
fn inline_html(s: &str, p: usize) -> Found {
    let b = s.as_bytes();
    let find = |from: usize, end: &str| match s.get(from..).and_then(|r| r.find(end)) {
        Some(i) => Found::Yes(from + i + end.len()),
        None => Found::Eof,
    };
    let name = |q: usize| {
        if !b.get(q).is_some_and(u8::is_ascii_alphabetic) {
            return q;
        }
        q + b[q..]
            .iter()
            .take_while(|&&c| c.is_ascii_alphanumeric() || c == b'-')
            .count()
    };
    let skip = |mut q: usize| {
        while q < b.len() && matches!(b[q], b' ' | b'\t' | b'\n') {
            q += 1;
        }
        q
    };
    let rest = &s[p + 1..];
    if let Some(comment) = rest.strip_prefix("!--") {
        if comment.starts_with('>') {
            return Found::Yes(p + 5);
        }
        if comment.starts_with("->") {
            return Found::Yes(p + 6);
        }
        return find(p + 4, "-->");
    }
    if rest.starts_with("![CDATA[") {
        return find(p + 9, "]]>");
    }
    if rest.starts_with('!') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic) {
        return find(p + 2, ">");
    }
    if rest.starts_with('?') {
        return find(p + 2, "?>");
    }
    if rest.starts_with('/') {
        let n = name(p + 2);
        if n == p + 2 {
            return Found::No;
        }
        let q = skip(n);
        return match b.get(q) {
            None => Found::Eof,
            Some(b'>') => Found::Yes(q + 1),
            Some(_) => Found::No,
        };
    }
    let mut q = name(p + 1);
    if q == p + 1 {
        return Found::No;
    }
    loop {
        let w = skip(q);
        let Some(&c) = b.get(w) else {
            return Found::Eof;
        };
        match c {
            b'>' => return Found::Yes(w + 1),
            b'/' => {
                return if b.get(w + 1) == Some(&b'>') {
                    Found::Yes(w + 2)
                } else {
                    Found::No
                };
            }
            c if w > q && (c.is_ascii_alphabetic() || c == b'_' || c == b':') => {
                q = w + b[w..]
                    .iter()
                    .take_while(|&&c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
                    .count();
                let eq = skip(q);
                if eq == b.len() {
                    return Found::Eof;
                }
                if b[eq] != b'=' {
                    continue;
                }
                let v = skip(eq + 1);
                match b.get(v) {
                    None => return Found::Eof,
                    Some(&quote @ (b'"' | b'\'')) => match s[v + 1..].find(quote as char) {
                        Some(i) => q = v + 1 + i + 1,
                        None => return Found::Eof,
                    },
                    Some(_) => {
                        let n = b[v..]
                            .iter()
                            .take_while(|c| !b" \t\n\r\"'=<>`".contains(c))
                            .count();
                        if n == 0 {
                            return Found::No;
                        }
                        q = v + n;
                    }
                }
            }
            _ => return Found::No,
        }
    }
}

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Markup {
    #[token("<!--")]
    Comment,
    #[token("<?")]
    Instruction,
    #[token("<![CDATA[")]
    Cdata,
    #[regex("<![A-Za-z]")]
    Declaration,
    #[regex("</?[A-Za-z][A-Za-z0-9-]*")]
    Tag,
    #[regex(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});")]
    Entity,
    #[regex("[^<&]+")]
    #[token("<")]
    #[token("&")]
    Text,
}

/// Color HTML, offset by `base`. Tags and comments take whatever follows
/// until they end, so a tag's color doesn't depend on whether it's closed
/// on a later line.
fn html(s: &str, base: usize, out: &mut Out) {
    let mut push = |r: Range<usize>, kind| {
        if r.start < r.end {
            out.push((base + r.start..base + r.end, kind));
        }
    };
    let until = |rest: &str, end: &str| rest.find(end).map_or(rest.len(), |i| i + end.len());
    let mut lex = Markup::lexer(s);
    while let Some(tok) = lex.next() {
        let r = lex.span();
        let rest = lex.remainder();
        let (n, kind) = match tok {
            Ok(Markup::Comment) => {
                let n = if rest.starts_with('>') {
                    1
                } else if rest.starts_with("->") {
                    2
                } else {
                    until(rest, "-->")
                };
                (n, Kind::Comment)
            }
            Ok(Markup::Instruction) => (until(rest, "?>"), Kind::Keyword),
            Ok(Markup::Declaration) => (until(rest, ">"), Kind::Keyword),
            Ok(Markup::Cdata) => (until(rest, "]]>"), Kind::String),
            Ok(Markup::Entity) => (0, Kind::Escape),
            Ok(Markup::Tag) => {
                let lt = if s[r.clone()].starts_with("</") { 2 } else { 1 };
                push(r.start..r.start + lt, Kind::Punctuation);
                push(r.start + lt..r.end, Kind::Keyword);
                let (end, closed) = tag_rest(s, r.end, &mut push);
                // Scripts and styles hold text that isn't HTML.
                let name = &s[r.start + lt..r.end];
                let end = if lt == 1
                    && closed
                    && ["script", "style"]
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(name))
                {
                    let close = format!("</{name}");
                    s[end..]
                        .match_indices("</")
                        .find(|(i, _)| {
                            s.get(end + i..end + i + close.len())
                                .is_some_and(|t| t.eq_ignore_ascii_case(&close))
                        })
                        .map_or(s.len(), |(i, _)| end + i)
                } else {
                    end
                };
                lex.bump(end - r.end);
                continue;
            }
            _ => continue,
        };
        lex.bump(n);
        push(r.start..r.end + n, kind);
    }
}

/// Color the attributes of a tag up to its `>`. Returns where it ends, and
/// whether it ended with `>` rather than `/>` or not at all.
fn tag_rest(s: &str, mut q: usize, push: &mut impl FnMut(Range<usize>, Kind)) -> (usize, bool) {
    let b = s.as_bytes();
    let mut value = false;
    loop {
        while q < b.len() && b[q].is_ascii_whitespace() {
            q += 1;
        }
        let Some(&c) = b.get(q) else {
            return (q, false);
        };
        let run = |stop: &[u8]| {
            b[q..]
                .iter()
                .take_while(|c| !c.is_ascii_whitespace() && !stop.contains(c))
                .count()
        };
        match c {
            b'>' => {
                push(q..q + 1, Kind::Punctuation);
                return (q + 1, true);
            }
            b'/' if b.get(q + 1) == Some(&b'>') => {
                push(q..q + 2, Kind::Punctuation);
                return (q + 2, false);
            }
            b'<' => return (q, false),
            b'=' => {
                push(q..q + 1, Kind::Punctuation);
                q += 1;
                value = true;
                continue;
            }
            b'"' | b'\'' => {
                let e = s[q + 1..]
                    .find(c as char)
                    .map_or(s.len(), |i| q + 1 + i + 1);
                push(q..e, Kind::String);
                q = e;
            }
            _ => {
                let (n, kind) = if value {
                    (run(b"\"'=<>`"), Kind::String)
                } else {
                    (run(b"\"'<>/="), Kind::Attribute)
                };
                push(q..q + n, kind);
                q += n.max(1);
            }
        }
        value = false;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Each token as its text and kind. Tokens of the same kind with
    /// nothing between them come out as one.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out.0.iter().map(|(r, k)| (&text[r.clone()], *k)).collect()
    }

    fn kinds_of(text: &str, kind: Kind) -> Vec<&str> {
        tokens(text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_blocks() {
        use Kind::*;
        let text = "\
# Title #

Setext
------

> - [x] done
>   1. item
***
    indented
| a | b |
|---|:-:|
| `c` | d \\| e |
";
        assert_eq!(
            tokens(text),
            [
                ("# Title #", Keyword),
                ("------", Keyword),
                (">", Punctuation),
                ("-", Punctuation),
                ("[x]", Punctuation),
                (">", Punctuation),
                ("1.", Punctuation),
                ("***", Punctuation),
                ("indented", String),
                ("|---|:-:|", Punctuation),
                ("|", Punctuation),
                ("`c`", String),
                ("|", Punctuation),
                ("\\|", Escape),
                ("|", Punctuation),
            ]
        );
        // An ordered list can only interrupt a paragraph if it starts at 1,
        // and indented code can't at all.
        assert_eq!(tokens("a\n2. b\n    c\n"), []);
        // An underline after a paragraph that is only definitions is a
        // thematic break.
        assert_eq!(kinds_of("[a]: /u\n---\n", Punctuation), ["[", "]:", "---"]);
    }

    #[test]
    fn code_spans() {
        assert_eq!(
            kinds_of("a `b` ``c`d`` ``` e", Kind::String),
            ["`b`", "``c`d``"]
        );
        // A code span that goes on to the next line gets its color there.
        assert_eq!(tokens("a `b\nc` d\n"), [("c`", Kind::String)]);
        // Escapes don't work in code spans, but one can stop a backtick
        // from starting one.
        assert_eq!(
            tokens(r"\`a` `\`"),
            [
                ("\\`", Kind::Escape),
                ("` `", Kind::String),
                ("\\`", Kind::Escape)
            ]
        );
    }

    #[test]
    fn links() {
        use Kind::*;
        assert_eq!(
            tokens("![a](<b c> 'd') [e][f] [g][] [h] <i@j.k> <ab:m>"),
            [
                ("![", Punctuation),
                ("](", Punctuation),
                ("<b c>", Attribute),
                ("'d'", String),
                (")", Punctuation),
                ("[", Punctuation),
                ("][", Punctuation),
                ("f", Attribute),
                ("]", Punctuation),
                ("[", Punctuation),
                ("][]", Punctuation),
                ("<", Punctuation),
                ("i@j.k", Attribute),
                (">", Punctuation),
                ("<", Punctuation),
                ("ab:m", Attribute),
                (">", Punctuation),
            ]
        );
        // Links can't hold links, but they can hold images.
        assert_eq!(
            kinds_of("[a [b](c) d](e) [![f](g)](h)", Attribute),
            ["c", "g", "h"]
        );
        // A definition, and a link whose destination is on the next line.
        assert_eq!(
            tokens("[a]: /u\n  \"t\"\n[b](\n/v)"),
            [
                ("[", Punctuation),
                ("]:", Punctuation),
                ("/u", Attribute),
                ("\"t\"", String),
                ("/v", Attribute),
                (")", Punctuation),
            ]
        );
    }

    #[test]
    fn html() {
        use Kind::*;
        assert_eq!(
            tokens("<div class=\"x\">\n*a* &amp; <!-- b\nc -->\n\nd <br/> e"),
            [
                ("<", Punctuation),
                ("div", Keyword),
                ("class", Attribute),
                ("=", Punctuation),
                ("\"x\"", String),
                (">", Punctuation),
                ("&amp;", Escape),
                ("<!-- b", Comment),
                ("c -->", Comment),
                ("<", Punctuation),
                ("br", Keyword),
                ("/>", Punctuation),
            ]
        );
        // A tag that isn't finished on its line is text, until the line
        // that finishes it.
        assert_eq!(
            tokens("a <b\nc=\"d\">"),
            [
                ("c", Attribute),
                ("=", Punctuation),
                ("\"d\"", String),
                (">", Punctuation)
            ]
        );
    }

    #[test]
    fn fenced_code() {
        use Kind::*;
        assert_eq!(
            tokens("> ```toml\n> a = 1\n> ```\n~~~\nb\n~~~"),
            [
                (">", Punctuation),
                ("```", Punctuation),
                ("toml", Attribute),
                (">", Punctuation),
                ("=", Operator),
                ("1", Number),
                (">", Punctuation),
                ("```", Punctuation),
                ("~~~", Punctuation),
                ("b", String),
                ("~~~", Punctuation),
            ]
        );
        for lang in ["js", "javascript"] {
            let text = format!("```{lang}\nreturn /a/\n```");
            assert_eq!(kinds_of(&text, Keyword), ["return"]);
            assert_eq!(kinds_of(&text, String), ["/a/"]);
        }
        assert_eq!(kinds_of("```json\n{\"a\": 1}\n```", Number), ["1"]);
        // A fence inside a list item ends with the item.
        assert_eq!(kinds_of("- ```\n  a\nb\n", String), ["a"]);
    }

    fn pick(xs: &'static [&'static str]) -> BoxedStrategy<String> {
        prop::sample::select(xs).prop_map(str::to_string).boxed()
    }

    /// Text for a paragraph, from pieces that start or end inline
    /// constructs.
    fn words() -> BoxedStrategy<String> {
        let piece = pick(&[
            "a",
            "b c",
            " ",
            "`x`",
            "``y`z``",
            "`",
            "``",
            "[l](/u)",
            "[l](</u v> \"t\")",
            "![i](i.png 'q')",
            "[r][foo]",
            "[foo][]",
            "[foo]",
            "[",
            "(",
            ")",
            "<http://x.y>",
            "<m@x.y>",
            "<span class=\"s\">",
            "</span>",
            "<!-- c -->",
            "<",
            ">",
            "&amp;",
            "&#35;",
            "\\*",
            "\\`",
            "\\[",
            "*e*",
            "**s**",
            "_u_",
            "!",
            "&",
            "|",
            "\\|",
            "[a\nb](c)",
            "`d\ne`",
            "<b\nc=\"d\">",
        ]);
        prop::collection::vec(piece, 1..6)
            .prop_map(|p| p.concat())
            .boxed()
    }

    /// A list of lines for a block, which containers then indent.
    fn block() -> BoxedStrategy<Vec<String>> {
        let leaf = prop_oneof![
            4 => words().prop_map(|w| w.lines().map(str::to_string).collect()),
            1 => (1usize..7, words()).prop_map(|(n, w)| vec![format!("{} {}", "#".repeat(n), w.replace('\n', " "))]),
            1 => (words(), pick(&["===", "---"])).prop_map(|(w, u)| w.lines().map(str::to_string).chain([u]).collect()),
            1 => (pick(&["```", "~~~", "````"]), pick(&["", "sh", "text x"]), words(), any::<bool>())
                .prop_map(|(f, info, w, close)| {
                    let mut v = vec![format!("{f}{info}")];
                    v.extend(w.lines().map(str::to_string));
                    if close { v.push(f) }
                    v
                }),
            1 => words().prop_map(|w| w.lines().map(|l| format!("    {l}")).collect()),
            1 => pick(&["***", "- - -", "___"]).prop_map(|l| vec![l]),
            1 => pick(&["<div>\n*x*\n</div>", "<!-- a\nb -->", "<pre>\n*y*\n</pre>", "<a href=\"x\">"])
                .prop_map(|h| h.lines().map(str::to_string).collect()),
            // A blank line ends the table. After a table, pulldown-cmark
            // reads an indented line as code if it has a pipe, and as a
            // row if it doesn't, which we don't copy.
            1 => (words(), words()).prop_map(|(a, b)| vec![
                "| a | b |".to_string(),
                "|---|:-:|".to_string(),
                format!("| {} | {} |", a.replace('\n', " "), b.replace('\n', " ")),
                String::new(),
            ]),
            1 => pick(&["[foo]: /url \"title\"", "[bar]: <a b>", "[foo]:\n/url\n'title'"])
                .prop_map(|d| d.lines().map(str::to_string).collect()),
        ];
        leaf.prop_recursive(3, 12, 3, |inner| {
            let blocks = prop::collection::vec(inner, 1..3).prop_map(|bs| {
                let mut v = Vec::new();
                for (i, b) in bs.into_iter().enumerate() {
                    if i > 0 {
                        v.push(String::new());
                    }
                    v.extend(b);
                }
                v
            });
            prop_oneof![
                // A block quote, where lines after the first may leave out
                // the marker.
                (blocks.clone(), any::<bool>()).prop_map(|(b, lazy)| b
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| if lazy && i > 0 { l } else { format!("> {l}") })
                    .collect()),
                (
                    blocks.clone(),
                    pick(&["- ", "* ", "1. ", "2) ", "- [ ] ", "- [x] "])
                )
                    .prop_map(|(b, m)| {
                        let pad = " ".repeat(m.len().min(3));
                        b.into_iter()
                            .enumerate()
                            .map(|(i, l)| {
                                if i == 0 {
                                    format!("{m}{l}")
                                } else if l.is_empty() {
                                    l
                                } else {
                                    format!("{pad}{l}")
                                }
                            })
                            .collect()
                    }),
                blocks,
            ]
        })
        .boxed()
    }

    fn document() -> impl Strategy<Value = String> {
        prop::collection::vec((block(), any::<bool>()), 1..5).prop_map(|bs| {
            // Full references are only links when their label is defined,
            // and the lexer takes every label to be. So we define every
            // label that the pieces above can make next to another one.
            // At the end, an unclosed code block could hide the definitions.
            let mut s =
                String::from("[foo]: /f\n[r]: /r\n[a]: /a\n[b c]: /b\n[l]: /l\n[a b]: /ab\n\n");
            for (b, blank) in bs {
                for l in b {
                    s.push_str(&l);
                    s.push('\n');
                }
                if blank {
                    s.push('\n');
                }
            }
            s
        })
    }

    /// Lexing must take time in proportion to the length of the text,
    /// even for a long paragraph whose first line leaves something open.
    #[test]
    fn long_paragraphs_are_fast() {
        let text = format!("` [ <a\n{}", "word `x` [a](b) <i>\n".repeat(20_000));
        let start = std::time::Instant::now();
        let mut out = Tokens::default();
        lex(&text, &mut out);
        assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    }

    proptest! {
        /// Compare the colors with what pulldown-cmark parses. Code spans
        /// and code blocks are strings, headings are keywords, markers are
        /// punctuation, and text is plain. A line that a code span, link or
        /// tag goes on from isn't checked, since its colors can't depend
        /// on the next line.
        #[test]
        fn agrees_with_pulldown_cmark(text in document()) {
            use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
            let mut out = Tokens::default();
            lex(&text, &mut out);
            let mut kinds = vec![None; text.len()];
            for (r, k) in out.0 {
                kinds[r].fill(Some(k));
            }
            let line_of = |i: usize| text[..i].matches('\n').count();
            // Where the last line of a range starts, after the markers of
            // block quotes.
            let last_line = |r: &Range<usize>| {
                let start = text[..r.end - 1].rfind('\n').map_or(0, |n| n + 1).max(r.start);
                start + text[start..r.end].len()
                    - text[start..r.end].trim_start_matches(['>', ' ', '\t']).len()
            };
            let events: Vec<_> = Parser::new_ext(&text, Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS)
                .into_offset_iter()
                .collect();
            // Lines that a construct goes on from.
            let mut skip = vec![false; text.lines().count() + 1];
            for (ev, r) in &events {
                if matches!(ev, Event::Code(_) | Event::InlineHtml(_) | Event::Start(Tag::Link { .. } | Tag::Image { .. })) {
                    skip[line_of(r.start)..line_of(r.end - 1)].fill(true);
                }
            }
            let check = |r: Range<usize>, ok: &[Option<Kind>], what: &str| -> Result<(), TestCaseError> {
                for i in r.clone() {
                    if !text.as_bytes()[i].is_ascii_whitespace() && !ok.contains(&kinds[i]) {
                        prop_assert!(false, "{what} {:?} has {:?} at {i} in\n{text}", &text[r], kinds[i]);
                    }
                }
                Ok(())
            };
            let mut heading = 0;
            // Whether each link we are inside of is an autolink, whose text
            // pulldown-cmark gives as text.
            let mut links: Vec<bool> = Vec::new();
            let mut code: Vec<bool> = Vec::new();
            for (ev, r) in events {
                match ev {
                    Event::Start(Tag::Heading { .. }) => {
                        heading += 1;
                        let first = text[r.clone()].trim_start();
                        if first.starts_with('#') {
                            let end = r.start + text[r.clone()].find('\n').unwrap_or(r.len());
                            check(r.start..end, &[Some(Kind::Keyword)], "ATX heading")?;
                        } else {
                            check(last_line(&r)..r.end, &[Some(Kind::Keyword)], "underline")?;
                        }
                    }
                    Event::End(TagEnd::Heading(_)) => heading -= 1,
                    Event::Start(Tag::Link { link_type, .. }) => {
                        links.push(matches!(link_type, LinkType::Autolink | LinkType::Email));
                    }
                    Event::End(TagEnd::Link) => {
                        links.pop();
                    }
                    Event::Start(Tag::CodeBlock(kind)) => code.push(match kind {
                        CodeBlockKind::Indented => true,
                        CodeBlockKind::Fenced(info) => for_info(&info).is_none(),
                    }),
                    Event::End(TagEnd::CodeBlock) => { code.pop(); }
                    // Headings are all one color, even their code spans.
                    Event::Code(_) if heading == 0 => {
                        check(last_line(&r)..r.end, &[Some(Kind::String)], "code span")?;
                    }
                    Event::Rule => check(r, &[Some(Kind::Punctuation)], "rule")?,
                    Event::TaskListMarker(_) => check(r, &[Some(Kind::Punctuation)], "task")?,
                    Event::Text(_) if code.last() == Some(&true) => check(r, &[Some(Kind::String)], "code")?,
                    Event::Text(_) if code.is_empty() && heading == 0 && !links.contains(&true) && !skip[line_of(r.start)] => {
                        check(r, &[None, Some(Kind::Escape)], "text")?;
                    }
                    _ => {}
                }
            }
        }
    }
}
