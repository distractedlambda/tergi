//! A lexer for shrubbery notation, which Rhombus and its relatives use.
//!
//! It follows the lexer that DrRacket uses to color shrubbery code:
//! `lex.rkt` in the `shrubbery-lib` package, and `lex-comment.rkt` for
//! group comments. Like Racket's reader, that lexer has a few surprises,
//! and we copy them so that tergi shows the same colors.
//!
//! A file in a Rhombus Scribble language, such as `#lang
//! rhombus/scribble/manual`, is text with @-forms in it, the way a Racket
//! Scribble document is. Inside text, an `@` starts an @-form, and the
//! @-form can hold more text in braces.

use logos::Logos;
use unicode_segmentation::UnicodeSegmentation;

use super::{Kind, Tokens, racket};

/// What a token is, as far as the lexer and the group comments care.
/// Each one is a token name from `lex.rkt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Name {
    Whitespace,
    /// `//`, `/* */`, `#!` lines, and `@//` line comments inside text.
    Comment,
    /// `#//`, which comments out the group after it.
    GroupComment,
    /// `@//` outside of text, and `@//{` inside it.
    AtComment,
    Opener,
    Closer,
    /// A `'`, until we know whether it opens or closes a quote.
    Quote,
    /// `#{` or `~#{`, which hold Racket data.
    SExp,
    /// `:`, `|`, `\`, `,` and `;`, which the parser treats specially.
    Block,
    Bar,
    Continue,
    Comma,
    Semicolon,
    Identifier,
    Keyword,
    Operator,
    Literal,
    At,
    AtOpener,
    AtCloser,
    AtContent,
    /// Anything the lexer reports as an error.
    Fail,
    Eof,
}

/// How to color a token.
#[derive(Debug, Clone, Copy)]
enum Color {
    Plain,
    Kind(Kind),
    /// A string, with its escapes colored.
    Str,
}

struct Tok {
    start: usize,
    end: usize,
    name: Name,
    color: Color,
    /// Whether the token is inside an `@//{` comment.
    comment: bool,
    /// For `#{`, the colors of the Racket data after it, and where that
    /// data ends.
    racket: Option<(Tokens, usize)>,
}

/// What the lexer is in the middle of, after a token. This is the status
/// that `lex/status` passes from one token to the next.
#[derive(Debug, Clone)]
enum Status {
    /// Between tokens. After a token such as an identifier, the lexer
    /// is `continuing`, and a `+`, `-` or `.` can't start a number.
    Plain {
        continuing: bool,
    },
    /// Inside `'` quotes. The openers are the quotes and parentheses
    /// that are open, innermost last, and `true` stands for a quote.
    /// They decide whether the next `'` opens or closes a quote.
    Quotes {
        continuing: bool,
        openers: Vec<bool>,
    },
    At(Box<InAt>),
    /// Inside an @-form that started in text. The @-form reads its
    /// command with the first status, and then the text goes on.
    Escaped(Box<Status>, Box<InAt>),
}

/// Where we are in an @-form. The names are the modes in `lex.rkt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Reading the command after the `@`, such as `foo` in `@foo{x}`.
    Initial,
    /// Reading arguments in parentheses or brackets after the command.
    Args,
    /// Reading the command `(« ... »)`, after which there are no
    /// arguments.
    NoArgs,
    /// After an identifier in the command, where an operator and
    /// another identifier follow, as in `@foo.bar`.
    OpContinue,
    /// Before the opening delimiter of text.
    Open,
    /// In text.
    Inside,
    /// Before an `@` in text.
    Escape,
    /// Before the closing delimiter of text.
    Close,
}

#[derive(Debug, Clone)]
struct InAt {
    mode: Mode,
    /// Whether this is an `@//{` comment, or inside one.
    comment: bool,
    /// Whether a closing delimiter ends the text. A whole document of
    /// text has no delimiters.
    closeable: bool,
    /// What comes between `|` and `{` in the opening delimiter of the
    /// text, with the `|`. It's empty for a plain `{`.
    opener: String,
    /// The status to go back to after the @-form, and the status for
    /// reading its command and arguments.
    sub: Status,
    /// The parentheses that are open in the command and arguments.
    openers: Vec<String>,
    /// How many opening delimiters inside the text haven't been closed.
    depth: usize,
}

impl InAt {
    fn new(mode: Mode, comment: bool, opener: Option<String>, sub: Status) -> InAt {
        InAt {
            mode,
            comment,
            closeable: true,
            opener: opener.unwrap_or_default(),
            sub,
            openers: Vec::new(),
            depth: 0,
        }
    }
}

const INITIAL: Status = Status::Plain { continuing: false };

/// The rules of the lexer in `make-lexer/status`, in order. When two
/// rules match the same text, the first one wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Space,
    Str,
    BadNumber,
    Number,
    Special,
    Bool,
    LineComment,
    BlockComment,
    GroupComment,
    HashBang,
    Script,
    Open,
    Close,
    Quote,
    SExp,
    KeywordSExp,
    Block,
    Bar,
    Continue,
    Comma,
    Semicolon,
    Identifier,
    Operator,
    Keyword,
    AtComment,
    At,
    BadHash,
    BadStr,
    BadKeyword,
    BadComment,
    BadChars,
    Any,
}

struct Lex<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Lex<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn tok(&self, start: usize, name: Name, color: Color) -> Tok {
        Tok {
            start,
            end: self.pos,
            name,
            color,
            comment: false,
            racket: None,
        }
    }

    /// Read one token outside of an @-form. This follows
    /// `make-lexer/status`, which finds the longest match among its rules.
    fn token(&mut self, continuing: bool) -> (Tok, Status) {
        use Rule::*;
        let start = self.pos;
        let rest = self.rest();
        let Some(c) = rest.chars().next() else {
            return (self.tok(start, Name::Eof, Color::Plain), INITIAL);
        };
        let initial = !continuing;
        let mut best = (0, Any);
        let mut found = |n: usize, rule: Rule| {
            if n > best.0 {
                best = (n, rule);
            }
        };
        let prefix = |s: &str| if rest.starts_with(s) { s.len() } else { 0 };

        found(space_len(rest), Space);
        found(str_len(rest), Str);
        found(bad_number_len(rest, initial), BadNumber);
        found(number_len(rest, initial), Number);
        for s in ["#inf", "#neginf", "#nan"] {
            found(prefix(s), Special);
        }
        for s in ["#true", "#false", "#void"] {
            found(prefix(s), Bool);
        }
        found(prefix("//"), LineComment);
        found(prefix("/*"), BlockComment);
        found(prefix("#//"), GroupComment);
        if let Some(after) = rest.strip_prefix("#!") {
            found(2 + run_len(after, |c| !c.is_whitespace()), HashBang);
            if after.starts_with([' ', '/']) {
                found(script_len(rest), Script);
            }
        }
        let rule = match c {
            '(' | '[' | '{' | '«' => Open,
            ')' | ']' | '}' | '»' => Close,
            '\'' => Quote,
            ':' => Block,
            '|' => Bar,
            '\\' => Continue,
            ',' => Comma,
            ';' => Semicolon,
            _ => Any,
        };
        if rule != Any {
            found(c.len_utf8(), rule);
        }
        found(prefix("#{"), SExp);
        found(prefix("~#{"), KeywordSExp);
        found(identifier_len(rest), Identifier);
        found(operator_len(rest), Operator);
        if c == '~' {
            let n = plain_identifier_len(&rest[1..]);
            if n > 0 {
                found(1 + n, Keyword);
            }
        }
        found(prefix("@//"), AtComment);
        found(prefix("@"), At);
        found(bad_hash_len(rest), BadHash);
        found(bad_str_len(rest), BadStr);
        found(prefix("~"), BadKeyword);
        found(prefix("*/"), BadComment);
        found(run_len(rest, is_bad_char), BadChars);
        found(c.len_utf8(), Any);

        let (n, rule) = best;
        self.pos += n;
        let simple = |lex: &Self, name, color, continuing| {
            (lex.tok(start, name, color), Status::Plain { continuing })
        };
        let punctuation = Color::Kind(Kind::Punctuation);
        match rule {
            Space => simple(self, Name::Whitespace, Color::Plain, false),
            Str => simple(self, Name::Literal, Color::Str, false),
            BadNumber | Number => self.number(start, rule == Number),
            Special => simple(self, Name::Literal, Color::Kind(Kind::Number), true),
            Bool => simple(self, Name::Literal, Color::Kind(Kind::Constant), true),
            LineComment => {
                self.line_comment(false);
                simple(self, Name::Comment, Color::Kind(Kind::Comment), false)
            }
            BlockComment => {
                // An unfinished comment is an error, but it gets the color
                // of a comment, like an unfinished `#|` in Racket.
                let name = if self.block_comment() {
                    Name::Comment
                } else {
                    Name::Fail
                };
                simple(self, name, Color::Kind(Kind::Comment), false)
            }
            GroupComment => simple(self, Name::GroupComment, Color::Kind(Kind::Comment), false),
            Script => simple(self, Name::Comment, Color::Kind(Kind::Comment), false),
            Open => simple(self, Name::Opener, punctuation, false),
            Close => simple(self, Name::Closer, punctuation, true),
            Quote => simple(self, Name::Quote, punctuation, false),
            SExp | KeywordSExp => {
                let mut tok = self.tok(start, Name::SExp, punctuation);
                tok.racket = Some(self.racket(rule == KeywordSExp));
                (tok, INITIAL)
            }
            Block => simple(self, Name::Block, punctuation, false),
            Bar => simple(self, Name::Bar, punctuation, false),
            Continue => simple(self, Name::Continue, punctuation, false),
            Comma => simple(self, Name::Comma, punctuation, false),
            Semicolon => simple(self, Name::Semicolon, punctuation, false),
            Identifier => simple(self, Name::Identifier, Color::Kind(Kind::Identifier), true),
            Operator => {
                // A `/` after an operator is part of it, unless it starts
                // a comment.
                let rest = self.rest();
                if rest.starts_with('/') && !rest[1..].starts_with(['/', '*']) {
                    self.pos += 1;
                }
                simple(self, Name::Operator, Color::Kind(Kind::Operator), false)
            }
            Keyword => simple(self, Name::Keyword, Color::Kind(Kind::Keyword), true),
            AtComment => {
                let color = Color::Kind(Kind::Comment);
                if let Some(opener) = self.peek_opener() {
                    let at = InAt::new(Mode::Open, true, Some(opener), INITIAL);
                    (
                        self.tok(start, Name::AtComment, color),
                        Status::At(Box::new(at)),
                    )
                } else {
                    self.line_comment(true);
                    simple(self, Name::AtComment, color, false)
                }
            }
            At => {
                let opener = self.peek_opener();
                let mode = if opener.is_some() {
                    Mode::Open
                } else {
                    Mode::Initial
                };
                let at = InAt::new(mode, false, opener, INITIAL);
                let tok = self.tok(start, Name::At, Color::Kind(Kind::Keyword));
                (tok, Status::At(Box::new(at)))
            }
            // A string that isn't finished, or has a bad escape. It's an
            // error, but it looks like a string.
            BadStr => simple(self, Name::Fail, Color::Str, false),
            HashBang | BadHash | BadKeyword | BadComment | BadChars | Any => {
                simple(self, Name::Fail, Color::Plain, false)
            }
        }
    }

    /// Finish a number, or something that starts like one but is an
    /// error, after the `n` bytes that the rule matched.
    ///
    /// A `.` right after a number is part of it, unless it starts an
    /// operator such as `..`. Only an integer can end with a `.`. An
    /// integer followed by `/` and digits that aren't all zeros is a
    /// fraction.
    fn number(&mut self, start: usize, ok: bool) -> (Tok, Status) {
        let lexeme = &self.text[start..self.pos];
        let rest = self.rest();
        let integer = is_decimal_integer(lexeme);
        let mut ok = ok;
        if rest.starts_with('.') && operator_len(rest) <= 1 {
            self.pos += 1;
            ok = ok && integer;
        } else if ok && integer && rest.starts_with('/') {
            let n = uinteger(rest.as_bytes(), 1, 10).unwrap_or(0);
            if n > 1 && rest[1..n].contains(|c| c != '0') && !rest[n..].starts_with('.') {
                self.pos += n;
            }
        }
        let (name, color) = if ok {
            (Name::Literal, Color::Kind(Kind::Number))
        } else {
            (Name::Fail, Color::Plain)
        };
        (
            self.tok(start, name, color),
            Status::Plain { continuing: true },
        )
    }

    /// Skip to the end of the line. With `newline`, also skip the line
    /// break and the spaces and tabs at the start of the next line.
    fn line_comment(&mut self, newline: bool) {
        let rest = self.rest();
        let mut n = rest.find(['\n', '\r']).unwrap_or(rest.len());
        if newline && n < rest.len() {
            n += if rest[n..].starts_with("\r\n") { 2 } else { 1 };
            n += run_len(&rest[n..], |c| c.is_whitespace() && c != '\n' && c != '\r');
        }
        self.pos += n;
    }

    /// Skip to the end of a block comment after its `/*`. Block comments
    /// nest. Returns false if the comment runs to the end of the text.
    fn block_comment(&mut self) -> bool {
        let b = self.rest().as_bytes();
        let mut depth = 1;
        let mut i = 0;
        while i < b.len() {
            match &b[i..] {
                [b'/', b'*', ..] => {
                    depth += 1;
                    i += 2;
                }
                [b'*', b'/', ..] => {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        self.pos += i;
                        return true;
                    }
                }
                _ => i += 1,
            }
        }
        self.pos += b.len();
        false
    }

    /// Read the Racket data in `#{...}` up to the `}`, which isn't read.
    /// After `~#{`, the data should be one symbol that names a keyword.
    /// Only a symbol right after the brace gets a color then, and
    /// anything else is an error.
    fn racket(&mut self, keyword: bool) -> (Tokens, usize) {
        let mut out = Tokens::default();
        let end = racket::lex_braced(self.text, self.pos, &mut out);
        // The whitespace before the `}` is part of the data too. That
        // matters to group comments, which don't count lines in it.
        let end = end + run_len(&self.text[end..], |c| c.is_whitespace() || c == '\u{FEFF}');
        if keyword {
            out = Tokens::default();
            if let Some(n) = racket::symbol_len(&self.text[self.pos..end]) {
                out.push(self.pos..self.pos + n, Kind::Keyword);
            }
        }
        self.pos = end;
        (out, end)
    }

    /// The opening delimiter of text that comes next, without the `{`.
    /// It's `{`, or `|`, some ASCII punctuation and symbols, and `{`.
    /// This follows `peek-at-opener*`.
    fn peek_opener(&self) -> Option<String> {
        let rest = self.rest();
        if rest.starts_with('{') {
            return Some(String::new());
        }
        if !rest.starts_with('|') {
            return None;
        }
        let n = 1 + run_len(&rest[1..], |c| {
            c.is_ascii() && c != '{' && (is_symbolic(c) || is_punctuation(c))
        });
        rest[n..].starts_with('{').then(|| rest[..n].to_string())
    }

    /// Read the next token of an @-form. This follows `at-lexer`.
    fn at_token(&mut self, mut at: InAt) -> (Tok, Status) {
        let text = self.text;
        match at.mode {
            Mode::Initial | Mode::Args | Mode::NoArgs | Mode::OpContinue => {
                let mode = at.mode;
                // The name of a command, such as `foo` in `@foo{text}`,
                // gets the color of the `@`.
                let command = matches!(mode, Mode::Initial | Mode::OpContinue)
                    && at.openers.is_empty()
                    && !matches!(at.sub, Status::At(_));
                let (mut tok, sub) = self.step(at.sub.clone());
                if command && tok.name == Name::Identifier {
                    tok.color = Color::Kind(Kind::Keyword);
                }
                at.sub = sub;
                let lexeme = &text[tok.start..tok.end];
                match tok.name {
                    Name::Opener | Name::SExp => {
                        if mode == Mode::Initial
                            && at.openers == ["("]
                            && tok.name == Name::Opener
                            && lexeme == "«"
                        {
                            at.mode = Mode::NoArgs;
                        }
                        let opener = if tok.name == Name::SExp { "{" } else { lexeme };
                        at.openers.push(opener.to_string());
                    }
                    Name::Closer => {
                        if at.openers.last().is_some_and(|o| closes(lexeme, o)) {
                            at.openers.pop();
                        } else {
                            tok.name = Name::Fail;
                            tok.color = Color::Plain;
                            return (tok, Status::At(Box::new(at)));
                        }
                    }
                    _ => {}
                }
                let status = self.after_command(at, mode, &tok);
                (tok, status)
            }
            Mode::Open => {
                let start = self.pos;
                self.pos += at.opener.len() + 1;
                at.mode = Mode::Inside;
                at.depth = 0;
                let tok = self.tok(start, Name::AtOpener, Color::Kind(Kind::Punctuation));
                (tok, Status::At(Box::new(at)))
            }
            Mode::Inside => self.at_text(at),
            Mode::Escape => {
                let start = self.pos;
                self.pos += at.opener.len() + 1;
                at.mode = Mode::Inside;
                let comment = at.comment;
                if self.rest().starts_with("//") {
                    self.pos += 2;
                    let color = Color::Kind(Kind::Comment);
                    if let Some(opener) = self.peek_opener() {
                        let sub = Status::Escaped(Box::new(INITIAL), Box::new(at));
                        let inner = InAt::new(Mode::Open, true, Some(opener), sub);
                        let tok = self.tok(start, Name::AtComment, color);
                        return (tok, Status::At(Box::new(inner)));
                    }
                    self.line_comment(true);
                    return (
                        self.tok(start, Name::Comment, color),
                        Status::At(Box::new(at)),
                    );
                }
                let tok = self.tok(start, Name::At, Color::Kind(Kind::Keyword));
                let opener = self.peek_opener();
                let mode = if opener.is_some() {
                    Mode::Open
                } else {
                    Mode::Initial
                };
                let sub = Status::Escaped(Box::new(INITIAL), Box::new(at));
                (
                    tok,
                    Status::At(Box::new(InAt::new(mode, comment, opener, sub))),
                )
            }
            Mode::Close => {
                let start = self.pos;
                if start == text.len() {
                    return (self.tok(start, Name::Eof, Color::Plain), INITIAL);
                }
                self.pos += 1 + at.opener.len();
                let tok = self.tok(start, Name::AtCloser, Color::Kind(Kind::Punctuation));
                // More text can follow right away.
                let status = match self.peek_opener() {
                    Some(o) => {
                        Status::At(Box::new(InAt::new(Mode::Open, at.comment, Some(o), at.sub)))
                    }
                    None => leave(at.sub),
                };
                (tok, status)
            }
        }
    }

    /// Decide what comes after a token of the command or arguments of an
    /// @-form. Once the parentheses are closed, the @-form ends unless
    /// text, arguments, or more of the command come next.
    fn after_command(&self, at: InAt, mode: Mode, tok: &Tok) -> Status {
        if matches!(at.sub, Status::At(_)) || !at.openers.is_empty() {
            return Status::At(Box::new(at));
        }
        let opener = if mode == Mode::NoArgs {
            None
        } else {
            self.peek_opener()
        };
        let still = |mode, opener| {
            Status::At(Box::new(InAt::new(
                mode,
                at.comment,
                opener,
                at.sub.clone(),
            )))
        };
        if opener.is_some() {
            still(Mode::Open, opener)
        } else if mode == Mode::Initial && tok.name == Name::Identifier && self.peek_continue() {
            still(Mode::OpContinue, None)
        } else if mode == Mode::OpContinue {
            still(Mode::Initial, None)
        } else if !matches!(mode, Mode::Args | Mode::NoArgs) && self.rest().starts_with(['(', '['])
        {
            still(Mode::Args, None)
        } else {
            leave(at.sub)
        }
    }

    /// Whether an operator and an identifier come next, with no space.
    fn peek_continue(&self) -> bool {
        let rest = self.rest();
        let n = operator_len(rest);
        n > 0 && identifier_len(&rest[n..]) > 0
    }

    /// Read text, up to the end of the line, an `@`, or the closing
    /// delimiter. A line break is a token of its own.
    fn at_text(&mut self, mut at: InAt) -> (Tok, Status) {
        let text = self.text;
        let start = self.pos;
        let opener = at.opener.clone();
        let mut p = start;
        loop {
            let rest = &text[p..];
            let Some(c) = rest.chars().next() else {
                at.mode = Mode::Close;
                break;
            };
            if c == '\n' || c == '\r' {
                if p == start {
                    p += if rest.starts_with("\r\n") { 2 } else { 1 };
                }
                break;
            }
            if at.closeable && rest.starts_with('}') && rest[1..].starts_with(&closer(&opener)) {
                if at.depth == 0 {
                    at.mode = Mode::Close;
                    break;
                }
                at.depth -= 1;
                p += 1 + opener.len();
                continue;
            }
            let after = rest.strip_prefix(opener.as_str());
            if after.is_some_and(|a| a.starts_with('@')) {
                at.mode = Mode::Escape;
                break;
            }
            if at.closeable && after.is_some_and(|a| a.starts_with('{')) {
                at.depth += 1;
                p += opener.len() + 1;
                continue;
            }
            p += c.len_utf8();
        }
        self.pos = p;
        (
            self.tok(start, Name::AtContent, Color::Plain),
            Status::At(Box::new(at)),
        )
    }

    /// Read the next token. This follows the loop in `lex/status`.
    fn step(&mut self, status: Status) -> (Tok, Status) {
        match status {
            Status::At(at) => {
                let comment = at.comment;
                let (mut tok, status) = self.at_token(*at);
                if comment && tok.name != Name::Eof {
                    tok.comment = true;
                }
                (tok, status)
            }
            Status::Escaped(sub, at) => {
                let (tok, sub) = self.step(*sub);
                (tok, Status::Escaped(Box::new(sub), at))
            }
            Status::Plain { continuing } => {
                let (mut tok, status) = self.token(continuing);
                if tok.name == Name::Quote {
                    tok.name = Name::Opener;
                    let quotes = Status::Quotes {
                        continuing: false,
                        openers: Vec::new(),
                    };
                    return (tok, quotes);
                }
                (tok, status)
            }
            Status::Quotes {
                continuing,
                openers,
            } => {
                let (tok, status) = self.token(continuing);
                quotes(tok, status, openers)
            }
        }
    }
}

/// Keep track of the quotes and parentheses that are open inside `'`
/// quotes, after `tok`. This follows `adjust-for-quotes`.
fn quotes(mut tok: Tok, status: Status, mut openers: Vec<bool>) -> (Tok, Status) {
    let quotes = |continuing, openers| Status::Quotes {
        continuing,
        openers,
    };
    if tok.name == Name::Quote {
        let status = match openers.last() {
            None => {
                tok.name = Name::Closer;
                status
            }
            Some(true) => {
                tok.name = Name::Closer;
                openers.pop();
                quotes(true, openers)
            }
            Some(false) => {
                tok.name = Name::Opener;
                openers.push(true);
                quotes(false, openers)
            }
        };
        return (tok, status);
    }
    let status = match status {
        Status::At(mut at) => {
            at.sub = quotes(true, openers);
            Status::At(at)
        }
        Status::Plain { continuing } => {
            match tok.name {
                // The `}` after the Racket data closes it.
                Name::Opener | Name::SExp => openers.push(false),
                Name::Closer => {
                    openers.pop();
                }
                _ => {}
            }
            quotes(continuing, openers)
        }
        status => status,
    };
    (tok, status)
}

/// The status to go back to when an @-form ends. An @-form that started
/// in text goes back to the text.
fn leave(sub: Status) -> Status {
    match sub {
        Status::Escaped(_, at) => Status::At(at),
        sub => sub,
    }
}

/// Whether `closer` closes `opener` in the command of an @-form.
fn closes(closer: &str, opener: &str) -> bool {
    matches!(
        (opener, closer),
        ("(", ")") | ("[", "]") | ("{", "}") | ("«", "»") | ("'", "'")
    )
}

/// The closing delimiter of text without its `}`: the opener backwards,
/// with brackets turned around.
fn closer(opener: &str) -> String {
    opener
        .chars()
        .rev()
        .map(|c| match c {
            '<' => '>',
            '>' => '<',
            '[' => ']',
            ']' => '[',
            '(' => ')',
            ')' => '(',
            c => c,
        })
        .collect()
}

/// The length of the run of characters at the start of `s` for which
/// `f` holds.
fn run_len(s: &str, f: impl Fn(char) -> bool) -> usize {
    s.find(|c| !f(c)).unwrap_or(s.len())
}

/// Whitespace up to the end of a line, with the line break.
fn space_len(s: &str) -> usize {
    let n = run_len(s, |c| c.is_whitespace() && c != '\n' && c != '\r');
    let rest = &s[n..];
    if rest.starts_with("\r\n") {
        n + 2
    } else if rest.starts_with(['\n', '\r']) {
        n + 1
    } else {
        n
    }
}

/// A `#!` line comment. A backslash at the end of a line continues it.
fn script_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 2;
    while i < b.len() {
        match b[i] {
            b'\\' if b[i + 1..].starts_with(b"\r\n") => i += 3,
            b'\\' if matches!(b.get(i + 1), Some(b'\n' | b'\r')) => i += 2,
            b'\n' | b'\r' => break,
            _ => i += 1,
        }
    }
    i
}

/// A string or byte string with only valid escapes, or 0.
fn str_len(s: &str) -> usize {
    let bytes = s.starts_with("#\"");
    let Some(body) = s.strip_prefix(if bytes { "#\"" } else { "\"" }) else {
        return 0;
    };
    let open = s.len() - body.len();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return open + i + 1,
            '\n' | '\r' => return 0,
            '\\' => {
                let Some(n) = escape_len(&body[i..], bytes) else {
                    return 0;
                };
                for _ in 1..n {
                    chars.next();
                }
            }
            c if bytes && c > '\u{FF}' => return 0,
            _ => {}
        }
    }
    0
}

/// The length in characters of an escape that starts with a backslash.
/// All of them are ASCII.
fn escape_len(s: &str, bytes: bool) -> Option<usize> {
    let b = s.as_bytes();
    let count = |from: usize, max: usize, radix: u32| {
        b[from.min(b.len())..]
            .iter()
            .take(max)
            .take_while(|c| (**c as char).is_digit(radix))
            .count()
    };
    match *b.get(1)? {
        b'"' | b'\\' | b'a' | b'b' | b't' | b'n' | b'v' | b'f' | b'r' | b'e' | b'\'' => Some(2),
        b'0'..=b'7' => Some(1 + count(1, 3, 8)),
        b'x' => Some(2 + count(2, 2, 16)).filter(|&n| n > 2),
        b'u' if !bytes => Some(2 + count(2, 4, 16)).filter(|&n| n > 2),
        b'U' if !bytes => Some(2 + count(2, 6, 16)).filter(|&n| n > 2),
        _ => None,
    }
}

/// A string that is an error, which runs up to its closing quote or the
/// end of the line.
fn bad_str_len(s: &str) -> usize {
    let open = if s.starts_with("#\"") {
        2
    } else if s.starts_with('"') {
        1
    } else {
        return 0;
    };
    let b = s.as_bytes();
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' => return i + 1,
            b'\n' | b'\r' => return i,
            b'\\' if matches!(b.get(i + 1), None | Some(b'\n' | b'\r')) => return i + 1,
            b'\\' => i += 1 + s[i + 1..].chars().next().map_or(0, char::len_utf8),
            _ => i += 1,
        }
    }
    i
}

/// `#` followed by letters, digits and underscores, or `#/`, unless
/// another rule covers it.
fn bad_hash_len(s: &str) -> usize {
    let Some(rest) = s.strip_prefix('#') else {
        return 0;
    };
    let n = run_len(rest, is_non_delimiter);
    if n == 0 && rest.starts_with('/') {
        2
    } else {
        1 + n
    }
}

/// The end of the longest run of digits in `radix` at `i`, with single
/// underscores between digits.
fn uinteger(b: &[u8], i: usize, radix: u32) -> Option<usize> {
    let digit = |j: usize| b.get(j).is_some_and(|&c| (c as char).is_digit(radix));
    if !digit(i) {
        return None;
    }
    let mut j = i + 1;
    loop {
        if digit(j) {
            j += 1;
        } else if b.get(j) == Some(&b'_') && digit(j + 1) {
            j += 2;
        } else {
            return Some(j);
        }
    }
}

/// The end of an exponent such as `e-5` at `i`.
fn exponent(b: &[u8], i: usize) -> Option<usize> {
    if !matches!(b.get(i), Some(b'e' | b'E')) {
        return None;
    }
    let i = i + 1;
    let i = if matches!(b.get(i), Some(b'+' | b'-')) {
        i + 1
    } else {
        i
    };
    uinteger(b, i, 10)
}

/// The longest number at the start of `s`, or 0. After a term, a number
/// can't start with a sign or a `.`, so `x-1` is `x`, `-` and `1`.
fn number_len(s: &str, initial: bool) -> usize {
    let b = s.as_bytes();
    let i = usize::from(initial && matches!(b.first(), Some(b'+' | b'-')));
    let mut best = 0;
    let mut found = |n: Option<usize>| best = best.max(n.unwrap_or(0));
    for (prefix, radix) in [(b"0x", 16), (b"0o", 8), (b"0b", 2)] {
        if b[i..].starts_with(prefix) {
            found(uinteger(b, i + 2, radix));
        }
    }
    if let Some(u) = uinteger(b, i, 10) {
        found(Some(u));
        found(exponent(b, u));
        if b.get(u) == Some(&b'.') {
            match uinteger(b, u + 1, 10) {
                Some(f) => {
                    found(Some(f));
                    found(exponent(b, f));
                }
                None => found(exponent(b, u + 1)),
            }
        }
    } else if initial && b.get(i) == Some(&b'.') {
        let f = uinteger(b, i + 1, 10);
        found(f);
        found(f.and_then(|f| exponent(b, f)));
    }
    best
}

/// A digit followed by letters, digits, underscores and dots before
/// them, that isn't a number. Such as `1x` or `1.5.2`.
fn bad_number_len(s: &str, initial: bool) -> usize {
    let b = s.as_bytes();
    let i = usize::from(initial && matches!(b.first(), Some(b'+' | b'-')));
    if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return 0;
    }
    let mut ends = Vec::new();
    let mut chars = s[i + 1..].chars().peekable();
    let mut j = i + 1;
    while let Some(c) = chars.next() {
        if is_non_delimiter(c) {
            j += c.len_utf8();
        } else if c == '.' && chars.peek().is_some_and(|&d| is_non_delimiter(d)) {
            j += 1 + chars.next().map_or(0, char::len_utf8);
        } else {
            break;
        }
        ends.push(j);
    }
    ends.into_iter()
        .rev()
        .find(|&e| number_len(&s[..e], initial) != e)
        .unwrap_or(0)
}

/// Whether a number is an integer, so that a `.` can end it.
fn is_decimal_integer(s: &str) -> bool {
    s.trim_start_matches(['+', '-'])
        .bytes()
        .all(|c| c.is_ascii_digit() || c == b'_')
}

fn is_non_delimiter(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == '_'
}

fn is_bad_char(c: char) -> bool {
    !(c.is_alphabetic()
        || c.is_numeric()
        || c.is_whitespace()
        || is_symbolic(c)
        || is_punctuation(c)
        || is_one_char_emoji(c))
}

/// An identifier, possibly with `#%` before it.
fn identifier_len(s: &str) -> usize {
    let n = if s.starts_with("#%") { 2 } else { 0 };
    let m = plain_identifier_len(&s[n..]);
    if m == 0 { 0 } else { n + m }
}

/// Letters, digits, underscores and emoji, not starting with a digit.
fn plain_identifier_len(s: &str) -> usize {
    let mut i = 0;
    while let Some(c) = s[i..].chars().next() {
        let one = if c.is_alphabetic() || c == '_' || i > 0 && c.is_numeric() {
            c.len_utf8()
        } else {
            0
        };
        let n = one.max(emoji_len(&s[i..]));
        if n == 0 {
            break;
        }
        i += n;
    }
    i
}

/// Whether `c` can be part of an operator.
fn is_opchar(c: char) -> bool {
    is_symbolic(c) && !is_one_char_emoji(c)
        || is_punctuation(c) && !",;#\\_@\"'()[]{}«»".contains(c)
}

/// The longest operator at the start of `s`, or 0. An operator is a run
/// of operator characters that doesn't end in `/` or `:`, a run of `:`,
/// or `#` with one of `'`, `,`, `:`, `;` or `|`. Neither `|` nor `:` on
/// its own is an operator, and an operator can't contain `//` or `/*`.
/// The `/` rule leaves out a final `/`, which the caller adds back.
fn operator_len(s: &str) -> usize {
    if s.len() >= 2 && s.starts_with('#') && s[1..].starts_with(['\'', ',', ':', ';', '|']) {
        return 2;
    }
    let chars: Vec<(usize, char)> = s
        .char_indices()
        .take_while(|&(_, c)| is_opchar(c))
        .collect();
    for k in (1..=chars.len()).rev() {
        let end = chars
            .get(k)
            .map_or_else(|| chars[k - 1].0 + chars[k - 1].1.len_utf8(), |c| c.0);
        let op = &s[..end];
        let last = chars[k - 1].1;
        let ok = if k == 1 {
            last != '~'
        } else {
            !matches!(last, '/' | ':') || op.bytes().all(|c| c == b':')
        };
        if ok && op != "|" && op != ":" && !op.contains("//") && !op.contains("/*") {
            return end;
        }
    }
    0
}

/// The Unicode general categories that Racket's `char-symbolic?` and
/// `char-punctuation?` check.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    #[regex(r"\p{S}")]
    Symbol,
    #[regex(r"\p{P}")]
    Punctuation,
}

/// Emoji, approximately. The lexer has a list of emoji sequences from
/// the Unicode standard, and we use Unicode properties instead.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Emoji {
    /// A character that is an emoji on its own.
    #[regex(r"[\p{Emoji_Presentation}--[\x{1F1E6}-\x{1F1FF}]]")]
    Presentation,
    /// A character that is an emoji when `U+FE0F` follows it.
    #[regex(r"[\p{Emoji}--\p{Emoji_Presentation}--\p{ASCII}]")]
    Text,
}

fn category(c: char) -> Option<Category> {
    let mut buf = [0; 4];
    Category::lexer(c.encode_utf8(&mut buf)).next()?.ok()
}

fn is_symbolic(c: char) -> bool {
    category(c) == Some(Category::Symbol)
}

fn is_punctuation(c: char) -> bool {
    category(c) == Some(Category::Punctuation)
}

fn emoji(c: char) -> Option<Emoji> {
    if c.is_ascii() {
        return None;
    }
    let mut buf = [0; 4];
    Emoji::lexer(c.encode_utf8(&mut buf)).next()?.ok()
}

fn is_one_char_emoji(c: char) -> bool {
    emoji(c) == Some(Emoji::Presentation)
}

/// An emoji at the start of `s`, or 0: a flag, a keycap, or emoji joined
/// with zero width joiners.
fn emoji_len(s: &str) -> usize {
    let mut chars = s.chars();
    let (Some(a), b, c) = (chars.next(), chars.next(), chars.next()) else {
        return 0;
    };
    let flag = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
    if flag(a) && b.is_some_and(flag) {
        return 8;
    }
    if "0123456789#*".contains(a) && b == Some('\u{FE0F}') && c == Some('\u{20E3}') {
        return 7;
    }
    let mut n = emoji_unit_len(s);
    if n == 0 {
        return 0;
    }
    while let Some(rest) = s[n..].strip_prefix('\u{200D}') {
        let m = emoji_unit_len(rest);
        if m == 0 {
            break;
        }
        n += 3 + m;
    }
    n
}

/// One emoji, with the skin tone or tags that follow it.
fn emoji_unit_len(s: &str) -> usize {
    let Some(c) = s.chars().next() else {
        return 0;
    };
    let mut n = match emoji(c) {
        Some(Emoji::Presentation) => c.len_utf8(),
        Some(Emoji::Text) if s[c.len_utf8()..].starts_with('\u{FE0F}') => c.len_utf8() + 3,
        _ => return 0,
    };
    if s[n..].starts_with(|c| ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)) {
        n += 4;
    }
    // Tags, as in the flag of Scotland.
    let tags = run_len(&s[n..], |c| ('\u{E0020}'..='\u{E007E}').contains(&c));
    if tags > 0 && s[n + tags..].starts_with('\u{E007F}') {
        n += tags + 4;
    }
    n
}

/// The pieces of a string. Anything else in one, such as a backslash
/// before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r#"\\([abtnvfre\\"']|[0-7]{1,3}|x[0-9a-fA-F]{1,2})"#)]
    Escape,
    // Byte strings don't take these.
    #[regex(r"\\(u[0-9a-fA-F]{1,4}|U[0-9a-fA-F]{1,6})")]
    Unicode,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

fn lex_string(text: &str, start: usize, out: &mut Tokens) {
    let bytes = text.starts_with('#');
    for (part, r) in Part::lexer(text).spanned() {
        let kind = match part {
            Ok(Part::Escape) => Kind::Escape,
            Ok(Part::Unicode) if !bytes => Kind::Escape,
            _ => Kind::String,
        };
        out.push(start + r.start..start + r.end, kind);
    }
}

/// A column, as `private/column.rkt` counts them. A tab doesn't have a
/// width, so a column is the number of characters after each tab on the
/// line, from right to left: `[a, 1, b]` is `b` characters, a tab, and
/// `a` characters. The 1s count the tabs.
#[derive(Debug, Clone, PartialEq)]
struct Column(Vec<f64>);

impl Default for Column {
    fn default() -> Column {
        Column(vec![0.0])
    }
}

impl Column {
    fn plus(&self, n: f64) -> Column {
        let mut c = self.clone();
        c.0[0] += n;
        c
    }

    /// Whether `self` is left of `other`. Columns after different tabs
    /// can't be compared, and then neither is left of the other.
    fn lt(&self, other: &Column) -> bool {
        let a: Vec<f64> = self.0.iter().rev().copied().collect();
        let b: Vec<f64> = other.0.iter().rev().copied().collect();
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            if x != y {
                return x < y && i + 1 == a.len();
            }
        }
        a.len() < b.len()
    }

    fn le(&self, other: &Column) -> bool {
        !other.lt(self)
    }
}

/// How far a token moves the position: how many lines, and the column
/// where it ends. This follows `count-graphemes`.
fn advance(text: &str) -> (usize, Column) {
    let mut lines = 0;
    let mut col = Column::default();
    for g in text.graphemes(true) {
        match g {
            "\r\n" | "\r" | "\n" => {
                lines += 1;
                col = Column::default();
            }
            "\t" => {
                col.0.splice(0..0, [0.0, 1.0]);
            }
            _ => col.0[0] += 1.0,
        }
    }
    (lines, col)
}

/// How far a group comment has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoFar {
    /// Nothing but whitespace and comments after the `#//` yet.
    Start,
    Running,
    /// The group is an alternative that starts with `|`.
    RunningBar,
}

#[derive(Debug, Clone)]
struct Pending {
    line: usize,
    column: Column,
    so_far: SoFar,
    stack: Vec<String>,
    bar: bool,
}

/// Follows group comments, which `#//` starts. A group comment covers
/// the group after it, and that group can go on over several lines. The
/// lexer doesn't parse, so it tells where the group ends from the
/// indentation and the brackets. This follows `lex-comment.rkt`, which
/// tracks the position on its own, in a way that sometimes differs from
/// the real one. We copy that too.
#[derive(Default)]
struct Groups {
    line: usize,
    column: Column,
    /// The line of the last token other than whitespace or comments.
    last_line: usize,
    pending: Option<Pending>,
    /// The brackets that are open, innermost last.
    stack: Vec<String>,
}

/// The state after a token, and whether that token is in a comment.
struct Finish {
    pending: Option<Pending>,
    stack: Vec<String>,
    whitespace: bool,
    comment: bool,
}

impl Finish {
    fn new(pending: Option<Pending>, stack: Vec<String>) -> Finish {
        let comment = pending.is_some();
        Finish {
            pending,
            stack,
            whitespace: false,
            comment,
        }
    }

    fn whitespace(mut self) -> Finish {
        self.whitespace = true;
        self
    }

    fn comment(mut self, comment: bool) -> Finish {
        self.comment = comment;
        self
    }
}

/// Whether line breaks don't matter to a group comment. They don't
/// inside `«»`, unless a `'` comes right before it.
fn not_line_sensitive(stack: &[String]) -> bool {
    let mut rest: Vec<&str> = stack.iter().rev().map(String::as_str).collect();
    loop {
        let Some(i) = rest.iter().position(|s| *s == "«") else {
            return false;
        };
        let after = &rest[i + 1..];
        if after.first() != Some(&"'") {
            return true;
        }
        rest = after[1..].to_vec();
    }
}

impl Groups {
    /// Note a token, and say whether it's in a group comment. `nested`
    /// says whether the token comes right after Racket data in `#{}`.
    fn token(&mut self, name: Name, text: &str, nested: bool) -> bool {
        let (lines, col) = advance(text);
        let mut popped = self.stack.clone();
        let (for_end, stack) = match name {
            Name::Opener | Name::AtOpener | Name::SExp => {
                let mut s = self.stack.clone();
                s.push(text.to_string());
                (self.stack.clone(), s)
            }
            Name::Closer | Name::AtCloser => {
                popped.pop();
                (popped.clone(), popped)
            }
            _ => (self.stack.clone(), popped),
        };
        let f = self.decide(name, nested, &for_end, stack);
        if !f.whitespace {
            self.last_line = self.line;
        }
        if lines == 0 {
            // A token with a tab in it doesn't move the column.
            if col.0.len() == 1 {
                self.column = self.column.plus(col.0[0]);
            }
        } else {
            self.column = col;
        }
        self.line += lines;
        self.pending = f.pending;
        self.stack = f.stack;
        f.comment
    }

    fn decide(&self, name: Name, nested: bool, for_end: &[String], stack: Vec<String>) -> Finish {
        let pending = self.pending.clone();
        if name == Name::GroupComment
            && pending
                .as_ref()
                .is_none_or(|p| self.ends(p, nested, for_end))
        {
            let p = Pending {
                line: self.line,
                column: self.column.clone(),
                so_far: SoFar::Start,
                stack: self.stack.clone(),
                bar: false,
            };
            return Finish::new(Some(p), Vec::new()).comment(pending.is_some());
        }
        if name == Name::Whitespace {
            return Finish::new(pending, stack).whitespace();
        }
        let Some(p) = pending else {
            return Finish::new(None, stack);
        };
        let insensitive = not_line_sensitive(&p.stack);
        let comment = name == Name::Comment;
        if p.so_far == SoFar::Start {
            let start_here = |so_far, bar| Pending {
                line: self.line,
                column: self.column.clone(),
                so_far,
                stack: self.stack.clone(),
                bar,
            };
            return match name {
                Name::Comment => Finish::new(Some(p), stack).whitespace(),
                Name::Bar if !insensitive => {
                    Finish::new(Some(start_here(SoFar::RunningBar, true)), stack)
                }
                Name::Comma | Name::Semicolon | Name::Closer => Finish::new(None, p.stack),
                _ if insensitive || self.line == p.line => {
                    let p = Pending {
                        so_far: SoFar::Running,
                        ..p
                    };
                    Finish::new(Some(p), stack)
                }
                _ => Finish::new(Some(start_here(SoFar::Running, false)), stack),
            };
        }
        if self.line == self.last_line || insensitive || !for_end.is_empty() || nested {
            return match name {
                Name::Comment => Finish::new(Some(p), stack).whitespace(),
                Name::Comma | Name::Semicolon => {
                    let bar = p.bar;
                    let keep = (!self.stack.is_empty()).then_some(p);
                    Finish::new(keep, stack).comment(!bar)
                }
                Name::Closer if self.stack.is_empty() => Finish::new(None, stack),
                Name::Bar
                    if p.so_far == SoFar::RunningBar
                        && self.stack.is_empty()
                        && (insensitive || self.line == p.line) =>
                {
                    Finish::new(None, p.stack)
                }
                _ => Finish::new(Some(p), stack),
            };
        }
        let half = if !p.bar && name == Name::Bar {
            0.5
        } else {
            0.0
        };
        if self.column.plus(half).le(&p.column) && self.stack.is_empty() {
            return if comment {
                Finish::new(Some(p), stack).whitespace().comment(false)
            } else {
                Finish::new(None, p.stack)
            };
        }
        if comment {
            Finish::new(Some(p), stack).whitespace()
        } else {
            Finish::new(Some(p), stack)
        }
    }

    /// Whether a `#//` ends the group comment before it, as it starts a
    /// new one. This follows `ends-only-pending?`.
    fn ends(&self, p: &Pending, nested: bool, for_end: &[String]) -> bool {
        p.so_far != SoFar::Start
            && self.line != self.last_line
            && !not_line_sensitive(&p.stack)
            && for_end.is_empty()
            && !nested
            && self.column.le(&p.column)
    }
}

/// Puts colors in the output. Tokens in a comment are colored as one
/// run, from the first to the last.
struct Emit<'a> {
    text: &'a str,
    out: &'a mut Tokens,
    run: Option<(usize, usize)>,
}

impl Emit<'_> {
    fn token(&mut self, tok: Tok, comment: bool) {
        if tok.name == Name::Whitespace {
            return;
        }
        let end = tok.racket.as_ref().map_or(tok.end, |r| r.1);
        if comment {
            let start = self.run.map_or(tok.start, |r| r.0);
            self.run = Some((start, end));
            return;
        }
        self.flush();
        let r = tok.start..tok.end;
        match tok.color {
            Color::Plain => {}
            Color::Kind(kind) => self.out.push(r, kind),
            Color::Str => lex_string(&self.text[r], tok.start, self.out),
        }
        if let Some((tokens, _)) = tok.racket {
            for (r, kind) in tokens.0 {
                self.out.push(r, kind);
            }
        }
    }

    fn flush(&mut self) {
        if let Some((start, end)) = self.run.take() {
            self.out.push(start..end, Kind::Comment);
        }
    }
}

/// Whether a language reads its modules as shrubbery notation, and if
/// so, whether it reads them as text with @-forms, the way Scribble does.
pub fn reader(lang: &str) -> Option<bool> {
    let name = lang.split_whitespace().next().unwrap_or("");
    let under = |base: &str| name == base || name.starts_with(&format!("{base}/"));
    if under("rhombus/scribble") || name == "shrubbery/text" {
        Some(true)
    } else if under("rhombus") || under("shrubbery") || under("shplait") {
        Some(false)
    } else {
        None
    }
}

/// Lex shrubbery notation from `start` to the end of `text`. With
/// `text_mode`, the whole thing is text with @-forms in it.
pub fn lex(text: &str, start: usize, text_mode: bool, out: &mut Tokens) {
    let mut lex = Lex { text, pos: start };
    let mut status = if text_mode {
        let mut at = InAt::new(Mode::Inside, false, None, INITIAL);
        at.closeable = false;
        Status::At(Box::new(at))
    } else {
        INITIAL
    };
    let mut groups = Groups::default();
    let mut emit = Emit {
        text,
        out,
        run: None,
    };
    let mut after_racket = false;
    // Every token moves forward, except for a few changes of state in an
    // @-form. This stops a mistake from making that loop forever.
    let mut stuck = 0;
    loop {
        let before = lex.pos;
        let (tok, next) = lex.step(status);
        status = next;
        if tok.name == Name::Eof {
            break;
        }
        stuck = if lex.pos == before { stuck + 1 } else { 0 };
        if stuck > 16 {
            break;
        }
        // Text can be empty, as in `@foo{}`, and isn't a token then.
        if tok.name == Name::AtContent && tok.start == tok.end {
            continue;
        }
        let nested = std::mem::replace(&mut after_racket, tok.racket.is_some());
        let comment = groups.token(tok.name, &text[tok.start..tok.end], nested)
            || tok.comment
            || tok.name == Name::GroupComment;
        emit.token(tok, comment);
    }
    emit.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each token but the identifiers, as its text and kind. Tokens of
    /// the same kind with nothing between them come out as one.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, 0, false, &mut out);
        out.colored(text)
    }

    /// Like `tokens`, for a Rhombus Scribble document.
    fn text(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, 0, true, &mut out);
        out.colored(text)
    }

    fn only(text: &str, kind: Kind) -> Vec<&str> {
        tokens(text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_a_definition() {
        use Kind::*;
        assert_eq!(
            tokens("fun f(x :: Int, ~y: y = 1.5): // add\n  x + y ++ \"!\\n\""),
            [
                ("(", Punctuation),
                ("::", Operator),
                (",", Punctuation),
                ("~y", Keyword),
                (":", Punctuation),
                ("=", Operator),
                ("1.5", Number),
                ("):", Punctuation),
                ("// add", Comment),
                ("+", Operator),
                ("++", Operator),
                ("\"!", String),
                ("\\n", Escape),
                ("\"", String),
            ]
        );
    }

    #[test]
    fn numbers() {
        // A sign starts a number only where a term can start.
        assert_eq!(
            only("-1 x-1 (2)-3 \"s\"-4", Kind::Number),
            ["-1", "1", "2", "3", "-4"]
        );
        assert_eq!(only("x-1", Kind::Operator), ["-"]);
        assert_eq!(
            only("1. 1..2 1/2 1/0 0x1F 1_000 1e5 .5 #inf #nan", Kind::Number),
            [
                "1.", "1", "2", "1/2", "1", "0", "0x1F", "1_000", "1e5", ".5", "#inf", "#nan"
            ]
        );
        // These are errors.
        assert_eq!(only("1x 1_ 1.5. 0x1.", Kind::Number), Vec::<&str>::new());
        assert_eq!(
            only("#true #false #void", Kind::Constant),
            ["#true", "#false", "#void"]
        );
    }

    #[test]
    fn operators() {
        use Kind::*;
        assert_eq!(
            tokens("a +/ b *+/ c // d"),
            [("+/", Operator), ("*+/", Operator), ("// d", Comment)]
        );
        // `:` and `|` on their own aren't operators, but longer runs are.
        assert_eq!(
            only("a: b | c :: d || e := f", Operator),
            ["::", "||", ":="]
        );
        assert_eq!(only("a: b | c \\ d", Punctuation), [":", "|", "\\"]);
        // An operator can't end with `:` or contain `//`.
        assert_eq!(tokens("+: x"), [("+", Operator), (":", Punctuation)]);
        assert_eq!(tokens("+//x"), [("+", Operator), ("//x", Comment)]);
        assert_eq!(only("#'x #:y", Operator), ["#'", "#:"]);
        // `*/` without a comment is an error.
        assert_eq!(tokens("*/"), []);
    }

    #[test]
    fn strings() {
        use Kind::*;
        assert_eq!(
            tokens(r#""a\u12345\x41" #"b\101""#),
            [
                ("\"a", String),
                ("\\u1234", Escape),
                ("5", String),
                ("\\x41", Escape),
                ("\"", String),
                ("#\"b", String),
                ("\\101", Escape),
                ("\"", String),
            ]
        );
        // A string with a bad escape, or without an end, is an error that
        // runs to its closing quote or the end of the line.
        assert_eq!(tokens("\"a\\q\" x"), [("\"a\\q\"", String)]);
        assert_eq!(tokens("\"a\nb"), [("\"a", String)]);
    }

    #[test]
    fn comments() {
        use Kind::*;
        assert_eq!(
            tokens("/* a /* b */ c */ d"),
            [("/* a /* b */ c */", Comment)]
        );
        assert_eq!(tokens("/* a\nb"), [("/* a\nb", Comment)]);
        assert_eq!(tokens("#! /bin/rhombus\nx"), [("#! /bin/rhombus", Comment)]);
    }

    #[test]
    fn group_comments() {
        use Kind::*;
        // The group after `#//` goes on while lines are indented more.
        assert_eq!(
            tokens("#// fun f():\n  body  \n  more\ng()"),
            [
                ("#// fun f():\n  body  \n  more", Comment),
                ("()", Punctuation)
            ]
        );
        // A comment on its own line covers the group on the next line.
        assert_eq!(
            tokens("#//\nf(1)\ng(2)"),
            [
                ("#//\nf(1)", Comment),
                ("(", Punctuation),
                ("2", Number),
                (")", Punctuation)
            ]
        );
        // A comma ends the group, and is part of the comment.
        assert_eq!(
            tokens("(#// a, b)"),
            [("(", Punctuation), ("#// a,", Comment), (")", Punctuation)]
        );
    }

    #[test]
    fn quotes() {
        use Kind::*;
        assert_eq!(
            tokens("'x + '1''"),
            [
                ("'", Punctuation),
                ("+", Operator),
                ("'", Punctuation),
                ("1", Number),
                ("''", Punctuation)
            ]
        );
    }

    #[test]
    fn racket_data() {
        use Kind::*;
        assert_eq!(
            tokens("#{(a #:b \"c\")} ~#{kw} ~#{ kw}"),
            [
                ("#{(", Punctuation),
                ("#:b", Keyword),
                ("\"c\"", String),
                (")}", Punctuation),
                ("~#{", Punctuation),
                ("kw", Keyword),
                ("}", Punctuation),
                ("~#{", Punctuation),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn at_forms() {
        use Kind::*;
        assert_eq!(
            tokens("@foo.bar(1){text @x more}"),
            [
                ("@foo", Keyword),
                (".", Operator),
                ("bar", Keyword),
                ("(", Punctuation),
                ("1", Number),
                ("){", Punctuation),
                ("@x", Keyword),
                ("}", Punctuation),
            ]
        );
        // Braces inside the text are text, and other delimiters only need
        // their own @-forms to be escaped.
        assert_eq!(
            tokens("@{a {b} c}"),
            [("@", Keyword), ("{", Punctuation), ("}", Punctuation)]
        );
        assert_eq!(
            tokens("@f|<{@x |<@y}>| z"),
            [
                ("@f", Keyword),
                ("|<{", Punctuation),
                ("|<@y", Keyword),
                ("}>|", Punctuation)
            ]
        );
        // An unmatched closer in the arguments is an error.
        assert_eq!(
            tokens("@foo(])"),
            [("@foo", Keyword), ("(", Punctuation), (")", Punctuation)]
        );
    }

    #[test]
    fn at_comments() {
        use Kind::*;
        assert_eq!(
            tokens("@//{a @b{c}} d @// e\n f"),
            [("@//{a @b{c}}", Comment), ("@// e\n ", Comment)]
        );
        assert_eq!(
            text("a @//{b} c @// d\ne"),
            [("@//{b}", Comment), ("@// d\n", Comment)]
        );
    }

    #[test]
    fn text_mode() {
        use Kind::*;
        // A whole document has no delimiters, so braces in it are text.
        assert_eq!(
            text("@title{Hi} {x} } @rhombus(1 + 2)"),
            [
                ("@title", Keyword),
                ("{", Punctuation),
                ("}", Punctuation),
                ("@rhombus", Keyword),
                ("(", Punctuation),
                ("1", Number),
                ("+", Operator),
                ("2", Number),
                (")", Punctuation)
            ]
        );
    }

    #[test]
    fn lang_lines() {
        use super::super::Language;
        use Kind::*;
        let lex = |lang: Language, text| -> Vec<(&str, Kind)> {
            let out = lang.lex(text);
            out.colored(text)
        };
        assert_eq!(
            lex(Language::Rhombus, "#lang rhombus\n1 // c"),
            [
                ("#lang rhombus", Attribute),
                ("1", Number),
                ("// c", Comment)
            ]
        );
        assert_eq!(
            lex(Language::Rhombus, "; c\n#lang rhombus\nx: 1"),
            [
                ("; c", Comment),
                ("#lang rhombus", Attribute),
                (":", Punctuation),
                ("1", Number)
            ]
        );
        // Without a `#lang` line, the file's name decides.
        assert_eq!(
            lex(Language::Rhombus, "1; x"),
            [("1", Number), (";", Punctuation)]
        );
        // The `#lang` line decides, whatever the file's name.
        assert_eq!(
            lex(Language::Racket, "#lang rhombus\nx: 1"),
            [
                ("#lang rhombus", Attribute),
                (":", Punctuation),
                ("1", Number)
            ]
        );
        assert_eq!(
            lex(Language::Rhombus, "#lang racket/base\n(x: 1)"),
            [
                ("#lang racket/base", Attribute),
                ("(", Punctuation),
                ("1", Number),
                (")", Punctuation)
            ]
        );
        // A language we don't know is read the way the file's name
        // suggests.
        assert_eq!(
            lex(Language::Rhombus, "#lang my_lang\nx: 1"),
            [
                ("#lang my_lang", Attribute),
                (":", Punctuation),
                ("1", Number)
            ]
        );
        assert_eq!(
            lex(
                Language::Scribble,
                "#lang rhombus/scribble/manual\n@title{x} (y)"
            ),
            [
                ("#lang rhombus/scribble/manual", Attribute),
                ("@title", Keyword),
                ("{", Punctuation),
                ("}", Punctuation)
            ]
        );
    }

    #[test]
    fn readers() {
        assert_eq!(reader("rhombus"), Some(false));
        assert_eq!(reader("rhombus/static/and_meta"), Some(false));
        assert_eq!(reader("shrubbery \"x.rhm\""), Some(false));
        assert_eq!(reader("rhombus/scribble/manual"), Some(true));
        assert_eq!(reader("shrubbery/text"), Some(true));
        assert_eq!(reader("racket/base"), None);
        assert_eq!(reader("rhombusx"), None);
    }

    /// Identifiers are marked, one token each, but the name of an
    /// @-form's command gets the color of its `@`.
    #[test]
    fn marks_plain_identifiers() {
        let text = "fun f(x, ~y: z): x + #%call @bold{a @x}";
        let mut out = Tokens::default();
        lex(text, 0, false, &mut out);
        assert_eq!(out.identifiers(text), ["fun", "f", "x", "z", "x", "#%call"]);
    }
}
