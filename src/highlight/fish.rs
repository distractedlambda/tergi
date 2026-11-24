//! A lexer for fish scripts.
//!
//! It follows fish 4.9. The tokenizer in `src/tokenizer.rs` splits the
//! text into words and operators. It finds where a word ends by counting
//! the parentheses, braces and brackets in it, and by skipping its
//! quotes. The parser in `src/ast.rs` decides which words are keywords
//! such as `if` and `end`. That depends on where it is, and sometimes on
//! the token after the word, so the lexer keeps the parser's place in
//! `Parser`. What's inside a word is read the way `unescape_string` and
//! `expand.rs` read it. The text of a command substitution such as
//! `(...)` is lexed as commands of its own, as fish's highlighter does.
//!
//! The colors of a line never depend on the lines after it. Where fish
//! looks at the token after a word, the lexer only looks to the end of
//! the line.

use std::borrow::Cow;

use super::{Kind, Tokens};

/// How deeply command substitutions can nest before we stop reading them
/// as such. Each level takes a few stack frames.
const MAX_DEPTH: usize = 64;

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lexer = Lexer {
        text,
        b: text.as_bytes(),
        i: 0,
        out,
        depth: 0,
    };
    lexer.commands();
}

/// What the parser expects next, as far as telling words apart goes. The
/// names follow the nodes of `ast.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A job in a list of jobs. There, `end`, `else` and `case` end the
    /// list, and `and` or `or` can start the job.
    Job,
    /// The condition after `if` or `while`. It is a job too, but the
    /// parser doesn't check that it can start one, so `end`, `else` and
    /// `case` are commands there.
    Condition,
    /// Where `time` can start a pipeline.
    Pipeline,
    /// Where variable assignments such as `a=1` can come before a
    /// statement.
    Assign,
    /// Where a statement starts.
    Statement,
    /// The command after `command`, `builtin` or `exec`.
    Command,
    /// The arguments and redirections of a command, or the words after
    /// `switch`, `case` or `end`.
    Args,
    /// The variable after `for`.
    ForVar,
    /// The `in` after the variable of `for`.
    ForIn,
    /// After `else`, where `if` can come.
    Else,
    /// The name after `function`.
    FunctionName,
}

/// Arguments that name variables, so that they get the same color as the
/// variables that use them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
    None,
    /// The options of `set`, before the name of the variable. Options
    /// stop after `--`.
    Set {
        dashdash: bool,
    },
    /// The options of `function`.
    Function,
    /// The names after `function -a`, up to the next option.
    Arguments,
    /// The name after `function -V` or `-v`.
    Variable,
}

/// The tokenizer's and the parser's state between tokens.
#[derive(Clone)]
struct Parser {
    expect: Expect,
    names: Names,
    /// After a redirection, the next word is its target. The flag says
    /// whether it names a file descriptor, as after `>&`.
    target: Option<bool>,
    /// Whether newlines are skipped here, as after `|`, `&&` and `||`.
    newlines: bool,
    /// `at_command_position` in the tokenizer, which makes a `{` start a
    /// block rather than a word.
    at_cmd: bool,
    /// The number of `{` that start a block, without their `}`.
    braces: usize,
}

impl Parser {
    fn new() -> Parser {
        Parser {
            expect: Expect::Job,
            names: Names::None,
            target: None,
            newlines: false,
            at_cmd: true,
            braces: 0,
        }
    }

    fn expect(&mut self, expect: Expect) {
        self.expect = expect;
        self.names = Names::None;
        self.target = None;
        self.newlines = false;
    }
}

/// What the token after a word is, as far as the parser looks at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextKind {
    Word,
    LeftBrace,
    /// A newline or a `;`.
    End,
    /// The end of the text.
    Terminate,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Next {
    kind: NextKind,
    /// Whether it's a word that starts with `-`.
    dash: bool,
    /// Whether it's `-h` or `--help`.
    help: bool,
}

impl Next {
    /// What we assume comes after a line continuation, since we don't
    /// look at the next line.
    const UNKNOWN: Next = Next {
        kind: NextKind::Word,
        dash: false,
        help: false,
    };
}

/// A redirection or a pipe, as `PipeOrRedir` reads it.
#[derive(Debug, Clone, Copy)]
struct Redirection {
    len: usize,
    /// The length of the file descriptor in front, as in `2>`.
    fd_len: usize,
    /// The file descriptor in front, or `None` if it's too large.
    fd: Option<u32>,
    pipe: bool,
    /// Whether the target is a file descriptor, as after `>&`.
    to_fd: bool,
}

struct Lexer<'a, 'o> {
    text: &'a str,
    /// The part of the text that can be read now. It's shorter than
    /// `text` inside command substitutions and words.
    b: &'a [u8],
    i: usize,
    out: &'o mut Tokens,
    /// How many command substitutions we're inside of.
    depth: usize,
}

impl<'a> Lexer<'a, '_> {
    fn at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    /// Color `n` bytes from the current position with `kind`, if any, and
    /// move past them.
    fn take(&mut self, n: usize, kind: Option<Kind>) {
        let i = self.i;
        if let Some(kind) = kind {
            self.out.push(i..i + n, kind);
        }
        self.i += n;
    }

    /// The length of the character at `i`, from its first byte.
    fn char_len(&self, i: usize) -> usize {
        let n = match self.b[i] {
            0xf0.. => 4,
            0xe0.. => 3,
            0xc0.. => 2,
            _ => 1,
        };
        n.min(self.b.len() - i)
    }

    fn char_at(&self, i: usize) -> Option<char> {
        if i >= self.b.len() {
            return None;
        }
        self.text[i..].chars().next()
    }

    /// The length of the whitespace character at `i` other than a line
    /// feed, or 0. The tokenizer skips any whitespace that Unicode knows
    /// between tokens, but inside a word only a few characters end it.
    fn space_len(&self, i: usize) -> usize {
        match self.at(i) {
            Some(b' ' | b'\t' | b'\r' | 0x0b | 0x0c) => 1,
            Some(0x80..) => self
                .char_at(i)
                .filter(|c| c.is_whitespace())
                .map_or(0, char::len_utf8),
            _ => 0,
        }
    }

    /// The length of the character at `i` if it can be in the name of a
    /// variable, or 0.
    fn var_char_len(&self, i: usize) -> usize {
        match self.at(i) {
            Some(c @ ..0x80) => usize::from(c.is_ascii_alphanumeric() || c == b'_'),
            Some(_) => self
                .char_at(i)
                .filter(|c| c.is_alphanumeric())
                .map_or(0, char::len_utf8),
            None => 0,
        }
    }

    /// The position of the line feed that ends the line at `i`, or the end
    /// of the text.
    fn line_end(&self, i: usize) -> usize {
        comment_end(self.b, i)
    }

    /// Read with only the text up to `end`, and then go back to the whole
    /// text.
    fn within<T>(&mut self, end: usize, f: impl FnOnce(&mut Self) -> T) -> T {
        let b = self.b;
        self.b = &b[..end.min(b.len())];
        let r = f(self);
        self.b = b;
        r
    }

    /// Lex commands up to the end of the text. This follows `next` in
    /// `tokenizer.rs`, and the parser's choices in `Parser`.
    fn commands(&mut self) {
        let mut p = Parser::new();
        loop {
            let continued = self.blanks();
            // After a line continuation, the tokenizer skips the line
            // feed after a comment too, so the command goes on after it.
            while self.at(self.i) == Some(b'#') {
                self.comment();
                if continued && self.at(self.i) == Some(b'\n') {
                    self.i += 1;
                }
                while let n @ 1.. = self.space_len(self.i) {
                    self.i += n;
                }
            }
            let i = self.i;
            let Some(c) = self.at(i) else { break };
            let next = self.at(i + 1);
            match c {
                // The tokenizer stops at a null character.
                0 => {
                    self.i = self.b.len();
                    break;
                }
                b'\n' | b';' => {
                    self.take(1, (c == b';').then_some(Kind::Punctuation));
                    while matches!(self.at(self.i), Some(b'\n' | b'\r' | b' ' | b'\t')) {
                        self.i += 1;
                    }
                    if !(p.newlines && c == b'\n') {
                        p.expect(Expect::Job);
                    }
                    p.at_cmd = true;
                }
                b'{' if p.at_cmd => {
                    self.take(1, Some(Kind::Punctuation));
                    p.braces += 1;
                    p.expect(Expect::Job);
                }
                // A `}` without its `{` is an error, which leaves the
                // tokenizer's idea of where commands start alone.
                b'}' if p.braces == 0 => {
                    self.i += 1;
                    p.expect(Expect::Job);
                }
                b'}' => {
                    self.take(1, Some(Kind::Punctuation));
                    p.braces -= 1;
                    p.expect(Expect::Args);
                    p.at_cmd = false;
                }
                b'&' | b'|' if next == Some(c) => {
                    self.take(2, Some(Kind::Operator));
                    p.expect(Expect::Pipeline);
                    p.newlines = true;
                    p.at_cmd = true;
                }
                b'&' if !matches!(next, Some(b'>' | b'|')) => {
                    self.take(1, Some(Kind::Operator));
                    p.expect(Expect::Job);
                    p.at_cmd = true;
                }
                b'&' | b'|' | b'<' | b'>' | b'0'..=b'9' => match redirection(&self.b[i..]) {
                    Some(r) => self.redirection(&mut p, r, c),
                    None => self.word(&mut p),
                },
                _ => self.word(&mut p),
            }
        }
    }

    /// Skip whitespace and line continuations, and say whether there was a
    /// continuation.
    fn blanks(&mut self) -> bool {
        let mut continued = false;
        loop {
            if self.at(self.i) == Some(b'\\') && self.at(self.i + 1) == Some(b'\n') {
                self.take(2, Some(Kind::Escape));
                continued = true;
                continue;
            }
            match self.space_len(self.i) {
                0 => return continued,
                n => self.i += n,
            }
        }
    }

    fn comment(&mut self) {
        let i = self.i;
        let end = self.line_end(i);
        self.out.push_comment(self.text, i..end);
        self.i = end;
    }

    /// Read a redirection or a pipe that starts with `first`.
    fn redirection(&mut self, p: &mut Parser, r: Redirection, first: u8) {
        self.take(r.fd_len, Some(Kind::Number));
        self.take(r.len - r.fd_len, Some(Kind::Operator));
        // Fish can't pipe from standard input.
        let error = r.fd_len > 0 && r.pipe && r.fd == Some(0);
        // Only some of the paths in `next` say that a command starts after
        // a pipe.
        p.at_cmd = match first {
            b'&' | b'|' => r.pipe,
            b'<' | b'>' => false,
            _ => r.pipe && !error,
        };
        if error {
            p.expect(Expect::Job);
        } else if r.pipe {
            p.expect(Expect::Assign);
            p.newlines = true;
        } else {
            p.target = Some(r.to_fd);
            p.newlines = false;
        }
    }

    /// Look at the token at `i` the way the parser does when it looks
    /// ahead. `at_cmd` is whether a command can start there.
    fn peek(&self, i: usize, at_cmd: bool) -> Next {
        let line_end = self.line_end(i);
        let mut j = i;
        loop {
            if self.at(j) == Some(b'\\') && self.at(j + 1) == Some(b'\n') {
                return Next::UNKNOWN;
            }
            match self.space_len(j) {
                0 => break,
                n => j += n,
            }
        }
        if self.at(j) == Some(b'#') {
            j = line_end;
        }
        let kind = match self.at(j) {
            None | Some(0) => NextKind::Terminate,
            Some(b'\n' | b';') => NextKind::End,
            Some(b'{') if at_cmd => NextKind::LeftBrace,
            Some(b'}' | b'&' | b'|' | b'<' | b'>') => NextKind::Other,
            Some(b'0'..=b'9') if redirection(&self.b[j..]).is_some() => NextKind::Other,
            _ => {
                let w = read_string(&self.b[..line_end], j);
                if w.error {
                    NextKind::Other
                } else {
                    let text = &self.b[j..w.end];
                    return Next {
                        kind: NextKind::Word,
                        dash: text.starts_with(b"-"),
                        help: text == b"-h" || text == b"--help",
                    };
                }
            }
        };
        Next {
            kind,
            dash: false,
            help: false,
        }
    }

    /// Read a word, and what it means to the parser.
    fn word(&mut self, p: &mut Parser) {
        let start = self.i;
        let w = read_string(self.b, start);
        if w.error {
            // An error on a later line can't change the colors of the
            // lines before it. They get the colors that they get when the
            // text ends before the error's line.
            let line = self.b[..w.end]
                .iter()
                .rposition(|&c| c == b'\n')
                .map_or(0, |n| n + 1);
            if line > start {
                let saved = p.clone();
                self.within(line, |l| l.word(p));
                *p = saved;
            }
            // Fish drops what it read of the word and returns the
            // character as an error.
            self.i = w.end + 1;
            p.expect(Expect::Job);
            p.at_cmd = false;
            return;
        }
        // Every character that gets here starts a word, but make sure we
        // move on.
        let end = w.end.max(start + self.char_len(start));
        let text = &self.text[start..end];
        let keyword = keyword_text(text);
        p.at_cmd = p.at_cmd
            && (keyword.as_deref().is_some_and(is_subcommand) || assignment_len(text).is_some());
        // Fish reads the whole word, but a keyword on more than one line
        // would make the colors of the first line depend on the next.
        let kw = match &keyword {
            Some(k) if !text.contains('\n') => k.as_ref(),
            _ => "",
        };
        p.newlines = false;
        if let Some(fd) = p.target.take() {
            self.target(end, fd);
            return;
        }
        let next = if matches!(
            p.expect,
            Expect::Job | Expect::Condition | Expect::Pipeline | Expect::Assign | Expect::Statement
        ) {
            self.peek(end, p.at_cmd)
        } else {
            Next::UNKNOWN
        };
        loop {
            match p.expect {
                Expect::Job | Expect::Condition => {
                    if p.expect == Expect::Job {
                        let then = match kw {
                            "end" | "case" => Some(Expect::Args),
                            "else" => Some(Expect::Else),
                            _ => None,
                        };
                        if let Some(then) = then {
                            self.take(end - start, Some(Kind::Keyword));
                            p.expect(then);
                            return;
                        }
                    }
                    if matches!(kw, "and" | "or") && !next.help {
                        self.take(end - start, Some(Kind::Keyword));
                        p.expect(Expect::Pipeline);
                        return;
                    }
                    p.expect = Expect::Pipeline;
                }
                Expect::Pipeline => {
                    if kw == "time" && !next.dash {
                        self.take(end - start, Some(Kind::Keyword));
                        p.expect(Expect::Assign);
                        return;
                    }
                    p.expect = Expect::Assign;
                }
                Expect::Assign => {
                    if assignment_len(text).is_some()
                        && matches!(
                            next.kind,
                            NextKind::Word | NextKind::LeftBrace | NextKind::Terminate
                        )
                    {
                        self.assignment(end);
                        return;
                    }
                    p.expect = Expect::Statement;
                }
                Expect::Statement => {
                    self.statement(p, end, kw, next);
                    return;
                }
                Expect::Command => {
                    self.command(p, end, keyword.as_deref());
                    return;
                }
                Expect::Args => {
                    self.argument(p, end);
                    return;
                }
                Expect::ForVar => {
                    self.name(end);
                    p.expect(Expect::ForIn);
                    return;
                }
                Expect::ForIn => {
                    if kw == "in" {
                        self.take(end - start, Some(Kind::Keyword));
                    } else {
                        self.parts(end);
                    }
                    p.expect(Expect::Args);
                    return;
                }
                Expect::Else => {
                    if kw == "if" {
                        self.take(end - start, Some(Kind::Keyword));
                        p.expect(Expect::Condition);
                        return;
                    }
                    p.expect = Expect::Job;
                }
                Expect::FunctionName => {
                    if is_plain(text) && !text.starts_with('-') {
                        self.take(end - start, Some(Kind::Identifier));
                    } else {
                        self.parts(end);
                    }
                    p.expect(Expect::Args);
                    p.names = Names::Function;
                    return;
                }
            }
        }
    }

    /// Read a word where a statement starts. This follows
    /// `allocate_populate_statement` in `ast.rs`.
    fn statement(&mut self, p: &mut Parser, end: usize, kw: &str, next: Next) {
        let text = &self.text[self.i..end];
        // A lone assignment is an error, but it still reads as one.
        if assignment_len(text).is_some() {
            self.assignment(end);
            p.expect(Expect::Args);
            return;
        }
        // A keyword before an option, or before the end of the text, runs
        // the builtin of that name, which usually prints its help. Most
        // keywords only do that for `-h` and `--help`.
        let help_only = matches!(kw, "begin" | "function" | "if" | "switch" | "while");
        let builtin = if help_only { next.help } else { next.dash }
            || (!matches!(kw, "begin" | "end") && next.kind == NextKind::Terminate);
        if !builtin {
            let then = match kw {
                "not" | "!" => Some(Expect::Pipeline),
                "for" => Some(Expect::ForVar),
                "while" | "if" => Some(Expect::Condition),
                "function" => Some(Expect::FunctionName),
                "begin" => Some(Expect::Job),
                // An `end` here is an error.
                "switch" | "end" => Some(Expect::Args),
                _ => None,
            };
            if let Some(then) = then {
                let kind = if kw == "!" {
                    Kind::Operator
                } else {
                    Kind::Keyword
                };
                self.take(end - self.i, Some(kind));
                p.expect(then);
                return;
            }
        }
        if matches!(kw, "command" | "builtin" | "exec") && next.kind == NextKind::Word && !next.dash
        {
            self.take(end - self.i, Some(Kind::Keyword));
            p.expect(Expect::Command);
            return;
        }
        let keyword = keyword_text(text);
        self.command(p, end, keyword.as_deref());
    }

    /// Read the name of a command. `unescaped` is the word without its
    /// quotes, if it only has letters and digits.
    fn command(&mut self, p: &mut Parser, end: usize, unescaped: Option<&str>) {
        if is_plain(&self.text[self.i..end]) {
            self.take(end - self.i, Some(Kind::Identifier));
        } else {
            self.parts(end);
        }
        p.expect(Expect::Args);
        if unescaped == Some("set") {
            p.names = Names::Set { dashdash: false };
        }
    }

    /// Read an argument of a command.
    fn argument(&mut self, p: &mut Parser, end: usize) {
        let text = &self.text[self.i..end];
        match p.names {
            Names::None => self.parts(end),
            Names::Set { dashdash } => {
                if !dashdash && text.starts_with('-') {
                    if text == "--" {
                        p.names = Names::Set { dashdash: true };
                    }
                    self.parts(end);
                } else {
                    self.set_name(end);
                    p.names = Names::None;
                }
            }
            Names::Function => {
                p.names = match text {
                    "-a" | "--argument-names" => Names::Arguments,
                    "-V" | "--inherit-variable" | "-v" | "--on-variable" => Names::Variable,
                    _ => Names::Function,
                };
                self.parts(end);
            }
            Names::Arguments if text.starts_with('-') => {
                p.names = Names::Function;
                self.argument(p, end);
            }
            Names::Arguments => self.name(end),
            Names::Variable => {
                self.name(end);
                p.names = Names::Function;
            }
        }
    }

    /// Read a word that names a variable.
    fn name(&mut self, end: usize) {
        if is_var_name(&self.text[self.i..end]) {
            self.take(end - self.i, Some(Kind::Identifier));
        } else {
            self.parts(end);
        }
    }

    /// Read the name of the variable that `set` sets, which can have a
    /// slice, as in `set a[2] x`.
    fn set_name(&mut self, end: usize) {
        let start = self.i;
        let mut n = start;
        while n < end
            && let len @ 1.. = self.var_char_len(n)
        {
            n += len;
        }
        if n > start && (n == end || self.at(n) == Some(b'[')) {
            self.take(n - start, Some(Kind::Identifier));
            self.within(end, |l| l.slice(false));
        }
        self.parts(end);
    }

    /// Read an assignment such as `a=1`.
    fn assignment(&mut self, end: usize) {
        let n = assignment_len(&self.text[self.i..end]).unwrap_or(0);
        self.take(n, Some(Kind::Identifier));
        self.take(1, Some(Kind::Operator));
        self.parts(end);
    }

    /// Read the target of a redirection. A file descriptor such as the `2`
    /// in `>&2` is a number, and the `-` in `>&-` closes one.
    fn target(&mut self, end: usize, fd: bool) {
        let text = &self.text[self.i..end];
        if fd && !text.is_empty() && text.bytes().all(|c| c.is_ascii_digit()) {
            self.take(text.len(), Some(Kind::Number));
        } else if fd && text == "-" {
            self.take(1, Some(Kind::Operator));
        } else {
            self.parts(end);
        }
    }

    /// Read the parts of a word up to `end`: quotes, escapes, variables,
    /// command substitutions and brace expansions.
    fn parts(&mut self, end: usize) {
        self.within(end, |l| {
            // Whether each `{` we're inside of starts a brace expansion.
            let mut braces = Vec::new();
            // Fish counts braces apart from that, and a `}` without its `{`
            // makes the count negative. A `,` only separates the items of
            // an expansion while the count is above zero.
            let mut count = 0i32;
            // No word starts with a `#`, and this can start in the middle
            // of one.
            let mut token_begin = false;
            // Inside braces and brackets, a word can have blanks in it, and
            // fish skips a comment after one when it looks for command
            // substitutions. But the comment is still part of the word, and
            // its quotes and variables expand.
            let mut comment = 0;
            while let Some(c) = l.at(l.i) {
                let i = l.i;
                match c {
                    b'\\' => l.escape(),
                    b'\'' => l.single_quote(),
                    b'"' => l.double_quote(),
                    b'$' if i < comment && l.at(i + 1) == Some(b'(') => l.i += 1,
                    b'$' => l.dollar(false),
                    b'(' if i >= comment => l.substitution(1),
                    b'#' if token_begin => {
                        comment = comment_end(l.b, i);
                        l.i += 1;
                    }
                    b'{' => {
                        count += 1;
                        let expands = l.expands(i, count > 0);
                        braces.push(expands);
                        l.take(1, expands.then_some(Kind::Punctuation));
                    }
                    b'}' => {
                        count -= 1;
                        let expands = braces.pop().unwrap_or(false);
                        l.take(1, expands.then_some(Kind::Punctuation));
                    }
                    b',' if count > 0 && braces.last() == Some(&true) => {
                        l.take(1, Some(Kind::Punctuation));
                    }
                    _ => {
                        token_begin = is_token_delimiter(c, l.at(i + 1));
                        l.i += 1;
                        continue;
                    }
                }
                token_begin = false;
            }
        });
        self.i = end;
    }

    /// Whether the `{` at `open` starts a brace expansion. Fish only
    /// expands braces that have a `,` or a variable right inside them, so
    /// `{}` and `{a}` stand for themselves. `commas` says whether a `,`
    /// counts. We only look to the end of the line.
    fn expands(&self, open: usize, commas: bool) -> bool {
        let b = &self.b[..self.line_end(open)];
        let mut depth = 0;
        let mut j = open + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'\'' => match quote_end(b, j, b'\'') {
                    Some(e) => j = e + 1,
                    None => return false,
                },
                b'"' => {
                    j += 1;
                    while j < b.len() && b[j] != b'"' {
                        match b[j] {
                            b'\\' => j += 2,
                            b'$' if depth == 0 && b.get(j + 1) != Some(&b'(') => return true,
                            _ => j += 1,
                        }
                    }
                    j += 1;
                }
                b'(' => match cmdsub_end(b, j) {
                    Some(e) => j = e + 1,
                    None => return false,
                },
                b'$' if b.get(j + 1) == Some(&b'(') => j += 1,
                b'$' if depth == 0 => return true,
                b',' if depth == 0 && commas => return true,
                b'{' => {
                    depth += 1;
                    j += 1;
                }
                b'}' if depth == 0 => return false,
                b'}' => {
                    depth -= 1;
                    j += 1;
                }
                _ => j += 1,
            }
        }
        false
    }

    /// Read a backslash outside of quotes and what it escapes, as
    /// `unescape_one` in fish's `crates/common` reads them.
    fn escape(&mut self) {
        let i = self.i;
        let Some(c) = self.at(i + 1) else {
            self.i += 1;
            return;
        };
        let digits = |from: usize, max: usize, radix: u32| {
            self.b[from.min(self.b.len())..]
                .iter()
                .take(max)
                .take_while(|&&c| char::from(c).is_digit(radix))
                .count()
        };
        let n = match c {
            b'0'..=b'7' => 1 + digits(i + 1, 3, 8),
            b'x' | b'X' => 2 + digits(i + 2, 2, 16),
            b'u' => 2 + digits(i + 2, 4, 16),
            b'U' => 2 + digits(i + 2, 8, 16),
            b'c' if i + 2 < self.b.len() => 2 + self.char_len(i + 2),
            _ => 1 + self.char_len(i + 1),
        };
        self.take(n, Some(Kind::Escape));
    }

    /// Move past the bytes up to one of `stops`, coloring them with
    /// `kind`. Move past at least one character.
    fn run(&mut self, stops: &[u8], kind: Option<Kind>) {
        let i = self.i;
        let n = self.b[i + 1..]
            .iter()
            .position(|c| stops.contains(c))
            .map_or(self.b.len() - i, |n| n + 1);
        self.take(n, kind);
    }

    /// Read a string in single quotes, where only `\\` and `\'` are
    /// escapes.
    fn single_quote(&mut self) {
        self.take(1, Some(Kind::String));
        while let Some(c) = self.at(self.i) {
            match c {
                b'\'' => {
                    self.take(1, Some(Kind::String));
                    return;
                }
                b'\\' if matches!(self.at(self.i + 1), Some(b'\\' | b'\'')) => {
                    self.take(2, Some(Kind::Escape));
                }
                _ => self.run(b"'\\", Some(Kind::String)),
            }
        }
    }

    /// Read a string in double quotes, with the variables and command
    /// substitutions in it.
    fn double_quote(&mut self) {
        self.take(1, Some(Kind::String));
        while let Some(c) = self.at(self.i) {
            match c {
                b'"' => {
                    self.take(1, Some(Kind::String));
                    return;
                }
                b'\\' => self.quoted_escape(),
                b'$' => self.dollar(true),
                _ => self.run(b"\"\\$", Some(Kind::String)),
            }
        }
    }

    /// Read a backslash in double quotes, where it only escapes `\`, `$`,
    /// `"` and a line feed.
    fn quoted_escape(&mut self) {
        if matches!(self.at(self.i + 1), Some(b'\\' | b'$' | b'"' | b'\n')) {
            self.take(2, Some(Kind::Escape));
        } else {
            self.take(1, Some(Kind::String));
        }
    }

    /// Read what a `$` starts: a variable with its slices, or a command
    /// substitution.
    fn dollar(&mut self, quoted: bool) {
        let i = self.i;
        let base = quoted.then_some(Kind::String);
        let n = self.b[i..].iter().take_while(|&&c| c == b'$').count();
        let after = i + n;
        if self.at(after) == Some(b'(') {
            self.take(n - 1, Some(Kind::Punctuation));
            self.substitution(2);
            return;
        }
        // Fish removes line continuations before it expands variables, so
        // they can come between the `$` and the name.
        let continued = self.continuations(after);
        if self.var_char_len(after) == 0 && continued == 0 {
            self.take(1, base);
            return;
        }
        // `$$a` is the variable that `a` names, and it can take a slice for
        // each `$`.
        self.take(n, Some(Kind::Punctuation));
        if continued > 0 {
            self.take(continued, Some(Kind::Escape));
            if self.var_char_len(self.i) == 0 {
                return;
            }
        }
        self.var_name();
        for _ in 0..n {
            if !self.slice(quoted) {
                break;
            }
        }
    }

    /// Read the name of a variable after its `$`. Fish removes line
    /// continuations before it expands variables, so one can split the
    /// name, or come before a slice.
    fn var_name(&mut self) {
        loop {
            let start = self.i;
            while let n @ 1.. = self.var_char_len(self.i) {
                self.i += n;
            }
            self.out.push(start..self.i, Kind::Identifier);
            let continued = self.continuations(self.i);
            if continued == 0 {
                return;
            }
            self.take(continued, Some(Kind::Escape));
            if self.var_char_len(self.i) == 0 {
                return;
            }
        }
    }

    /// The length of the line continuations that come one after another
    /// at `i`.
    fn continuations(&self, i: usize) -> usize {
        self.b[i.min(self.b.len())..]
            .chunks(2)
            .take_while(|c| *c == b"\\\n")
            .count()
            * 2
    }

    /// If a slice such as `[1..-1]` comes here, read it, and say whether
    /// it did. We only read one that ends on its line.
    fn slice(&mut self, quoted: bool) -> bool {
        let open = self.i;
        if self.at(open) != Some(b'[') {
            return false;
        }
        let Some(close) = slice_end(&self.b[..self.line_end(open)], open) else {
            return false;
        };
        self.take(1, Some(Kind::Punctuation));
        self.within(close, |l| l.slice_parts(quoted));
        self.i = close;
        self.take(1, Some(Kind::Punctuation));
        true
    }

    /// Read the indices in a slice.
    fn slice_parts(&mut self, quoted: bool) {
        let base = quoted.then_some(Kind::String);
        while let Some(c) = self.at(self.i) {
            let next = self.at(self.i + 1);
            match c {
                b'0'..=b'9' | b'-' if c != b'-' || next.is_some_and(|c| c.is_ascii_digit()) => {
                    let digits = self.b[self.i + 1..]
                        .iter()
                        .take_while(|c| c.is_ascii_digit())
                        .count();
                    self.take(1 + digits, Some(Kind::Number));
                }
                b'.' if next == Some(b'.') => self.take(2, Some(Kind::Operator)),
                b'$' => self.dollar(quoted),
                b'\\' if quoted => self.quoted_escape(),
                b'\\' => self.escape(),
                b'(' if !quoted => self.substitution(1),
                b'\'' if !quoted => self.single_quote(),
                b'"' if !quoted => self.double_quote(),
                _ => {
                    let n = self.char_len(self.i);
                    self.take(n, base);
                }
            }
        }
    }

    /// Read a command substitution, which starts with `(` or `$(`. Fish
    /// finds where it ends with `locate_cmdsub` in `parse_util.rs`.
    fn substitution(&mut self, open_len: usize) {
        if self.depth >= MAX_DEPTH {
            self.i += open_len;
            return;
        }
        let close = cmdsub_end(self.b, self.i + open_len - 1);
        self.take(open_len, Some(Kind::Punctuation));
        let end = close.unwrap_or(self.b.len());
        self.depth += 1;
        self.within(end, |l| l.commands());
        self.depth -= 1;
        self.i = end;
        if close.is_some() {
            self.take(1, Some(Kind::Punctuation));
        }
    }
}

/// Where a word ends, as `read_string` in `tokenizer.rs` finds it.
struct Word {
    /// Where the word ends, or the position of the character that makes
    /// it an error.
    end: usize,
    error: bool,
}

/// Find the end of the word that starts at `start`. Inside parentheses,
/// braces and brackets, blanks and operators don't end it.
fn read_string(b: &[u8], start: usize) -> Word {
    let error = |end| Word { end, error: true };
    let mut expecting = Vec::new();
    let mut parens = 0;
    // The numbers of open parentheses at each command substitution in
    // double quotes, as in `"a$(b)c"`. Its `)` goes back into the quotes.
    let mut quoted = Vec::new();
    let mut token_begin = true;
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\\' => i += 1,
            b'#' if token_begin => i = comment_end(b, i) - 1,
            b'(' => {
                parens += 1;
                expecting.push(b'(');
            }
            b'{' => expecting.push(b'{'),
            b')' => {
                if expecting.pop() != Some(b'(') {
                    return error(i);
                }
                parens -= 1;
                if quoted.last() == Some(&parens) {
                    quoted.pop();
                    match quote_end(b, i, b'"') {
                        Some(e) => {
                            if b[e] == b'$' {
                                quoted.push(parens);
                            }
                            i = e;
                        }
                        None => {
                            i = b.len();
                            break;
                        }
                    }
                }
            }
            b'}' => match expecting.last() {
                Some(b'{') => {
                    expecting.pop();
                }
                Some(_) => return error(i),
                None => break,
            },
            // A `[` at the start, as in the command `[`, isn't a slice.
            b'[' if i != start => expecting.push(b'['),
            b']' if expecting.last() == Some(&b'[') => {
                expecting.pop();
            }
            b'\'' | b'"' => match quote_end(b, i, c) {
                Some(e) => {
                    if b[e] == b'$' {
                        quoted.push(parens);
                    }
                    i = e;
                }
                None => {
                    i = b.len();
                    break;
                }
            },
            _ if expecting.is_empty() && !is_string_char(c, b.get(i + 1).copied()) => break,
            _ => {}
        }
        token_begin = is_token_delimiter(c, b.get(i + 1).copied());
        i += 1;
    }
    Word {
        end: i.min(b.len()),
        error: false,
    }
}

/// `quote_end` in `tokenizer.rs`: the position of the quote that closes
/// the one at `i`. A double-quoted string also stops at the `$` of a
/// `$(`, which takes the string to the command substitution's `)`.
fn quote_end(b: &[u8], mut i: usize, quote: u8) -> Option<usize> {
    loop {
        i += 1;
        let c = *b.get(i)?;
        if c == b'\\' {
            i += 1;
        } else if c == quote || (quote == b'"' && c == b'$' && b.get(i + 1) == Some(&b'(')) {
            return Some(i);
        }
    }
}

/// The position of the line feed after `i`, or the end of the text.
fn comment_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |n| i + n)
}

/// `locate_cmdsub` in `parse_util.rs`: the position of the `)` that
/// closes the `(` at `open`, if there is one.
fn cmdsub_end(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quoted = Vec::new();
    let mut escaped = false;
    let mut token_begin = true;
    let mut i = open;
    while i < b.len() {
        if escaped {
            escaped = false;
            token_begin = false;
            i += 1;
            continue;
        }
        let c = b[i];
        match c {
            b'\'' | b'"' => {
                let e = quote_end(b, i, c)?;
                if b[e] == b'$' {
                    quoted.push(depth);
                }
                i = e;
            }
            b'\\' => escaped = true,
            b'#' if token_begin => i = comment_end(b, i) - 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                if quoted.last() == Some(&depth) {
                    quoted.pop();
                    let e = quote_end(b, i, b'"')?;
                    if b[e] == b'$' {
                        quoted.push(depth);
                    }
                    i = e;
                }
            }
            _ => {}
        }
        token_begin = is_token_delimiter(c, b.get(i + 1).copied());
        i += 1;
    }
    None
}

/// `slice_length` in `parse_util.rs`: the position of the `]` that closes
/// the `[` at `open`, if there is one.
fn slice_end(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => i = quote_end(b, i, b[i])?,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// `tok_is_string_character` in `tokenizer.rs`: whether `c` can be part
/// of a word, with the `ampersand-nobg-in-token` feature that fish turns
/// on by default. A `&` that comes right before more of a word is part of
/// it.
fn is_string_char(c: u8, next: Option<u8>) -> bool {
    match c {
        0 | b' ' | b'\n' | b'|' | b'\t' | b';' | b'\r' | b'<' | b'>' => false,
        b'&' => next.is_some_and(|n| is_string_char(n, None)),
        _ => true,
    }
}

/// `is_token_delimiter` in `tokenizer.rs`: whether a word can start after
/// `c`, as far as a `#` starting a comment goes.
fn is_token_delimiter(c: u8, next: Option<u8>) -> bool {
    c == b'(' || !is_string_char(c, next)
}

/// `PipeOrRedir::try_from` in `tokenizer.rs`: the redirection or pipe at
/// the start of `b`, such as `2>`, `>>?`, `&>`, `<&` or `|`.
fn redirection(b: &[u8]) -> Option<Redirection> {
    let fd_len = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let fd = std::str::from_utf8(&b[..fd_len])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&fd| i32::try_from(fd).is_ok());
    let mut n = fd_len;
    let mut eat = |c: u8| {
        let ok = b.get(n) == Some(&c);
        n += usize::from(ok);
        ok
    };
    let mut pipe = false;
    let mut to_fd = false;
    // `&|` and `&>` take standard output whatever number comes before
    // them.
    let mut fd = fd;
    if eat(b'|') {
        if fd_len > 0 {
            return None;
        }
        pipe = true;
        eat(b'&');
    } else if eat(b'>') {
        eat(b'>');
        if eat(b'|') {
            pipe = true;
        } else if eat(b'&') {
            to_fd = true;
        } else {
            eat(b'?');
        }
    } else if eat(b'<') {
        if eat(b'&') {
            to_fd = true;
        } else {
            eat(b'?');
        }
    } else if eat(b'&') {
        fd = Some(1);
        if eat(b'|') {
            pipe = true;
        } else if eat(b'>') {
            eat(b'>');
            eat(b'?');
        } else {
            return None;
        }
    } else {
        return None;
    }
    Some(Redirection {
        len: n,
        fd_len,
        fd,
        pipe,
        to_fd,
    })
}

/// The keywords and builtins after which a command can start, as far as
/// the tokenizer goes. These are the ones marked `subcommand` in
/// `parser_keywords.rs`.
fn is_subcommand(w: &str) -> bool {
    matches!(
        w,
        "!" | "and"
            | "begin"
            | "builtin"
            | "command"
            | "else"
            | "exec"
            | "if"
            | "not"
            | "or"
            | "time"
            | "while"
    )
}

/// `unescape_keyword` in `ast.rs`: the word without its quotes and
/// escapes, if it only has the characters of keywords in it. Fish reads
/// `'if'` and `\if` as the keyword `if` too.
fn keyword_text(w: &str) -> Option<Cow<'_, str>> {
    let ok = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'\'' | b'"' | b'\\' | b'\n' | b'!');
    if !w.bytes().all(ok) {
        return None;
    }
    if !w.bytes().any(|c| matches!(c, b'\'' | b'"' | b'\\')) {
        return Some(Cow::Borrowed(w));
    }
    unescape(w.as_bytes()).map(Cow::Owned)
}

/// `unescape_string` in fish's `crates/common`, for a word that only has
/// ASCII characters, without expansions. It fails where fish fails.
fn unescape(b: &[u8]) -> Option<String> {
    #[derive(PartialEq)]
    enum Mode {
        Unquoted,
        Single,
        Double,
    }
    let mut mode = Mode::Unquoted;
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match mode {
            Mode::Unquoted => match c {
                b'\\' => {
                    let (ch, n) = unescape_one(&b[i..])?;
                    out.extend(ch);
                    i += n;
                    continue;
                }
                b'\'' => mode = Mode::Single,
                b'"' => mode = Mode::Double,
                _ => out.push(char::from(c)),
            },
            Mode::Single => match (c, next) {
                (b'\\', None) => return None,
                (b'\\', Some(d @ (b'\\' | b'\''))) => {
                    out.push(char::from(d));
                    i += 1;
                }
                (b'\'', _) => mode = Mode::Unquoted,
                _ => out.push(char::from(c)),
            },
            Mode::Double => match (c, next) {
                (b'\\', None) => return None,
                (b'\\', Some(d @ (b'\\' | b'$' | b'"'))) => {
                    out.push(char::from(d));
                    i += 1;
                }
                (b'\\', Some(b'\n')) => i += 1,
                (b'"', _) => mode = Mode::Unquoted,
                _ => out.push(char::from(c)),
            },
        }
        i += 1;
    }
    Some(out)
}

/// `unescape_one` in fish's `crates/common`: the character that the
/// escape at the start of `b` stands for, if any, and its length.
fn unescape_one(b: &[u8]) -> Option<(Option<char>, usize)> {
    let number = |from: usize, max: usize, radix: u32, limit: u32| {
        let digits: Vec<u32> = b[from.min(b.len())..]
            .iter()
            .take(max)
            .map_while(|&c| char::from(c).to_digit(radix))
            .collect();
        let value = digits
            .iter()
            .fold(0u64, |v, &d| v * u64::from(radix) + u64::from(d));
        if digits.is_empty() || value > u64::from(limit) {
            return None;
        }
        // Bytes above 0x7f only make sense together with others, and
        // can't be part of a keyword.
        let c = u32::try_from(value).ok().and_then(char::from_u32);
        Some((Some(c.unwrap_or('\u{fffd}')), from + digits.len()))
    };
    match *b.get(1)? {
        b'0'..=b'7' => number(1, 3, 8, 0x7f),
        b'x' | b'X' => number(2, 2, 16, 0xff),
        b'u' => number(2, 4, 16, 0xffff),
        b'U' => number(2, 8, 16, 0x10ffff),
        // Control characters, which aren't in any keyword.
        b'a' | b'b' | b'e' | b'f' | b'n' | b'r' | b't' | b'v' => Some((Some('\u{7}'), 2)),
        b'c' => matches!(b.get(2), Some(b'@'..=b'~')).then_some((Some('\u{7}'), 3)),
        b'\n' => Some((None, 2)),
        c => Some((Some(char::from(c)), 2)),
    }
}

/// `variable_assignment_equals_pos` in `tokenizer.rs`: the length of the
/// name before the `=` of an assignment such as `a=1`.
fn assignment_len(w: &str) -> Option<usize> {
    let n = w
        .char_indices()
        .find(|&(_, c)| !is_var_char(c))
        .map(|(n, _)| n)?;
    (n > 0 && w[n..].starts_with('=')).then_some(n)
}

/// `valid_var_name_char` in fish's `common.rs`.
fn is_var_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_var_name(w: &str) -> bool {
    !w.is_empty() && w.chars().all(is_var_char)
}

/// Whether a word stands for itself and gets a color from its name, as a
/// command does. A word with quotes, escapes or expansions doesn't, and
/// `[` is better left plain like its `]`.
fn is_plain(w: &str) -> bool {
    !w.is_empty() && !w.bytes().any(|c| b"'\"\\$()*?[]{}~=\n".contains(&c))
}

#[cfg(test)]
mod tests {
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
    fn lexes_a_script() {
        use Kind::*;
        let text = r#"#!/usr/bin/env fish
# Copy the files.
for f in $argv
    if test -f "$f"; and not set -q quiet
        cp $f "$dest/" 2>/dev/null || return 1
    end
end
"#;
        assert_eq!(
            tokens(text),
            [
                ("#!/usr/bin/env fish", Comment),
                ("# Copy the files.", Comment),
                ("for", Keyword),
                ("in", Keyword),
                ("$", Punctuation),
                ("if", Keyword),
                ("\"", String),
                ("$", Punctuation),
                ("\"", String),
                (";", Punctuation),
                ("and", Keyword),
                ("not", Keyword),
                ("$", Punctuation),
                ("\"", String),
                ("$", Punctuation),
                ("/\"", String),
                ("2", Number),
                (">", Operator),
                ("||", Operator),
                ("end", Keyword),
                ("end", Keyword),
            ]
        );
        assert_eq!(
            identifiers(text),
            [
                "f", "argv", "test", "f", "set", "quiet", "cp", "f", "dest", "return"
            ]
        );
    }

    #[test]
    fn keywords_depend_on_their_place() {
        // A keyword is only one where a statement starts.
        assert_eq!(
            kinds_of("if a; echo if end; end", Kind::Keyword),
            ["if", "end"]
        );
        // Before `-h` or `--help`, or at the end of the text, a keyword
        // runs the builtin of that name. Some keywords take any option.
        assert_eq!(
            kinds_of("if --help; while -h; begin -x; end", Kind::Keyword),
            ["begin", "end"]
        );
        assert_eq!(
            kinds_of("for -h; not -q; if", Kind::Keyword),
            [] as [&str; 0]
        );
        assert_eq!(identifiers("for -h; not -q; if"), ["for", "not", "if"]);
        // `end`, `else` and `case` only end a list of jobs, so they are
        // commands in a condition.
        assert_eq!(kinds_of("if case; end", Kind::Keyword), ["if", "end"]);
        // Fish reads a keyword without its quotes and escapes.
        assert_eq!(tokens("'if' true; \\end"), {
            use Kind::*;
            [("'if'", Keyword), (";", Punctuation), ("\\e", Escape)]
        });
        // `else if` is one clause, and `in` follows the variable of `for`.
        assert_eq!(
            kinds_of(
                "if a\nelse if b\nelse\nfor in in in; end\nend",
                Kind::Keyword
            ),
            ["if", "else", "if", "else", "for", "in", "end", "end"]
        );
        assert_eq!(
            kinds_of("switch $x\ncase 'a*' b\n  echo case\nend", Kind::Keyword),
            ["switch", "case", "end"]
        );
    }

    #[test]
    fn job_prefixes() {
        // `and`, `or` and `time` only start a job, and `not` and `!` a
        // statement.
        assert_eq!(
            kinds_of("and a; or b; a && and b; a | time b", Kind::Keyword),
            ["and", "or"]
        );
        assert_eq!(
            kinds_of("time not time a; and --help; time -p a", Kind::Keyword),
            ["time", "not", "time"]
        );
        assert_eq!(kinds_of("! a; !a; a !", Kind::Operator), ["!"]);
        // `command`, `builtin` and `exec` only decorate a command that
        // follows them.
        assert_eq!(
            kinds_of(
                "command ls; command -v ls; builtin cd; exec\nexec fish",
                Kind::Keyword
            ),
            ["command", "builtin", "exec"]
        );
        assert_eq!(
            identifiers("command if; exec\nexec fish"),
            ["if", "exec", "fish"]
        );
    }

    #[test]
    fn names_of_variables() {
        // Assignments before a command, and the names that `for`, `set`
        // and `function -a` give to variables.
        assert_eq!(
            identifiers("a=1 b=2 echo $a; for i in 1; end"),
            ["a", "b", "echo", "a", "i"]
        );
        assert_eq!(kinds_of("a=1 b= c", Kind::Operator), ["=", "="]);
        assert_eq!(
            identifiers("set -l x 1; set -- -x 2; set PATH[1] a; set -e"),
            ["set", "x", "set", "set", "PATH", "set"]
        );
        assert_eq!(
            identifiers("function foo -a x y -d desc -V z w\n  echo $x\nend"),
            ["foo", "x", "y", "z", "echo", "x"]
        );
        // A name that has to be expanded first gets no color.
        assert_eq!(
            identifiers("function $f; end; for $i in a; end"),
            ["f", "i"]
        );
    }

    #[test]
    fn comments() {
        use Kind::*;
        // A `#` starts a comment only at the start of a word.
        assert_eq!(
            tokens("echo a#b #c\necho (# d  \n a)"),
            [
                ("#c", Comment),
                ("(", Punctuation),
                ("# d", Comment),
                (")", Punctuation),
            ]
        );
        // After a line continuation, a comment doesn't end the command.
        assert_eq!(
            identifiers("echo a \\\n  # c\n  b\necho c"),
            ["echo", "echo"]
        );
        assert_eq!(identifiers("echo a\n  # c\n  b"), ["echo", "b"]);
    }

    #[test]
    fn quotes_and_escapes() {
        use Kind::*;
        assert_eq!(
            tokens(r#"a 'b\'c\n' "d\"\a\$e" \x41\n\cA\u00e9\q \"#),
            [
                ("'b", String),
                ("\\'", Escape),
                ("c\\n'", String),
                ("\"d", String),
                ("\\\"", Escape),
                ("\\a", String),
                ("\\$", Escape),
                ("e\"", String),
                ("\\x41\\n\\cA\\u00e9\\q", Escape),
            ]
        );
        // The escape takes only as many digits as it can use.
        assert_eq!(kinds_of(r"a \x411 \1234", Kind::Escape), [r"\x41", r"\123"]);
        // Quotes that don't close go on to the end.
        assert_eq!(tokens("a 'b\nc"), [("'b\nc", String)]);
        assert_eq!(tokens("a \"b\nc\" d"), [("\"b\nc\"", String)]);
    }

    #[test]
    fn variables_and_slices() {
        use Kind::*;
        assert_eq!(
            tokens(r#"a $x[1..-1] $$y[1][2] "$z[2]" $ $é"#),
            [
                ("$", Punctuation),
                ("[", Punctuation),
                ("1", Number),
                ("..", Operator),
                ("-1", Number),
                ("]", Punctuation),
                ("$$", Punctuation),
                ("[", Punctuation),
                ("1", Number),
                ("][", Punctuation),
                ("2", Number),
                ("]", Punctuation),
                ("\"", String),
                ("$", Punctuation),
                ("[", Punctuation),
                ("2", Number),
                ("]", Punctuation),
                ("\"", String),
                ("$", Punctuation),
            ]
        );
        assert_eq!(
            identifiers(r#"a $x[1] $$y "$z" $é"#),
            ["a", "x", "y", "z", "é"]
        );
        // A slice must end on its line.
        assert_eq!(kinds_of("a $x[1\n]", Kind::Number), [] as [&str; 0]);
        // A variable in a slice.
        assert_eq!(identifiers("a $x[$i]"), ["a", "x", "i"]);
        // Fish removes line continuations before it expands variables, so
        // one can come after the `$`, inside the name or before a slice.
        assert_eq!(
            tokens("a $\\\nx $y\\\n[1]"),
            [
                ("$", Punctuation),
                ("\\\n", Escape),
                ("$", Punctuation),
                ("\\\n", Escape),
                ("[", Punctuation),
                ("1", Number),
                ("]", Punctuation),
            ]
        );
        assert_eq!(identifiers("a $\\\nx $y\\\nz"), ["a", "x", "y", "z"]);
        assert_eq!(identifiers("a $y\\\n\\\nz"), ["a", "y", "z"]);
    }

    #[test]
    fn command_substitutions() {
        use Kind::*;
        // What's inside is lexed as commands of its own.
        assert_eq!(
            tokens("a (if b; end) $(c)d"),
            [
                ("(", Punctuation),
                ("if", Keyword),
                (";", Punctuation),
                ("end", Keyword),
                (")", Punctuation),
                ("$(", Punctuation),
                (")", Punctuation),
            ]
        );
        // `$(...)` works in double quotes, which go on after its `)`.
        assert_eq!(
            tokens(r#"a "b$(c ")")d""#),
            [
                ("\"b", String),
                ("$(", Punctuation),
                ("\")\"", String),
                (")", Punctuation),
                ("d\"", String),
            ]
        );
        // A plain `(` in double quotes is text.
        assert_eq!(tokens(r#"a "(b)""#), [(r#""(b)""#, String)]);
        // A `)` that closes nothing is an error, and fish drops the word.
        assert_eq!(identifiers("echo a)b c"), ["echo", "b"]);
        // Inside parentheses, blanks and line feeds don't end the word.
        assert_eq!(identifiers("echo (a\nb) c"), ["echo", "a", "b"]);
    }

    #[test]
    fn braces() {
        use Kind::*;
        // Braces only expand with a `,` or a variable right inside them.
        assert_eq!(
            tokens("echo {a,b} {a} {$x} {} {a,{b}}"),
            [
                ("{", Punctuation),
                (",", Punctuation),
                ("}", Punctuation),
                ("{$", Punctuation),
                ("}", Punctuation),
                ("{", Punctuation),
                (",", Punctuation),
                ("}", Punctuation),
            ]
        );
        // Where a command starts, a brace starts a block.
        assert_eq!(
            tokens("{ echo a; } | b; a=b {c}"),
            [
                ("{", Punctuation),
                (";", Punctuation),
                ("}", Punctuation),
                ("|", Operator),
                (";", Punctuation),
                ("=", Operator),
                ("{", Punctuation),
                ("}", Punctuation),
            ]
        );
        assert_eq!(identifiers("{echo, b}"), ["echo,"]);
        // In braces, fish skips a comment after a blank when it looks for
        // command substitutions, but the comment's variables still
        // expand. A `}` there without its `{` keeps the braces after it
        // from expanding.
        assert_eq!(
            tokens("a {b, #$c $(d) } } {e,f}"),
            [
                ("{", Punctuation),
                (",", Punctuation),
                ("$", Punctuation),
                ("}", Punctuation),
            ]
        );
        assert_eq!(identifiers("a {b, #$c $(d) } } {e,f}"), ["a", "c"]);
    }

    #[test]
    fn errors_on_later_lines() {
        use Kind::*;
        // Fish drops a word with an error in it, but the lines before the
        // error's line keep the colors they get without it. The word after
        // the error is read as usual.
        assert_eq!(tokens("echo $(a\nb } c"), [("$(", Punctuation)]);
        assert_eq!(identifiers("echo $(a\nb } c"), ["echo", "a", "c"]);
        assert_eq!(
            tokens("> a[$(b)\n}"),
            [(">", Operator), ("$(", Punctuation), (")", Punctuation)]
        );
    }

    #[test]
    fn redirections_and_pipes() {
        use Kind::*;
        assert_eq!(
            tokens("a b>c 2>&1 >&- <? f &> g &>> h >>? i"),
            [
                (">", Operator),
                ("2", Number),
                (">&", Operator),
                ("1", Number),
                (">&-", Operator),
                ("<?", Operator),
                ("&>", Operator),
                ("&>>", Operator),
                (">>?", Operator),
            ]
        );
        assert_eq!(
            tokens("a | b &| c 2>| d |& e && f || g & h"),
            [
                ("|", Operator),
                ("&|", Operator),
                ("2", Number),
                (">|", Operator),
                ("|&", Operator),
                ("&&", Operator),
                ("||", Operator),
                ("&", Operator),
            ]
        );
        assert_eq!(identifiers("a | b && c & d"), ["a", "b", "c", "d"]);
        // A `&` before more of a word is part of it.
        assert_eq!(identifiers("echo a&b; c& d"), ["echo", "c", "d"]);
        // Only `>|` pipes from the number before it, and fish can't pipe
        // from standard input. `&|` always pipes standard output.
        assert_eq!(identifiers("a 0>|b; c 0&| d"), ["a", "b", "c", "d"]);
        assert_eq!(kinds_of("c 0&| {d}", Kind::Punctuation), ["{", "}"]);
        // Newlines can follow a pipe.
        assert_eq!(identifiers("a |\n\n b &&\n c"), ["a", "b", "c"]);
    }
}
