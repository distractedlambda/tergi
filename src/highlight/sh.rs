//! A lexer for shell scripts.
//!
//! It follows bash 5.3's parser in `parse.y`. `read_token` splits the
//! text into operators and words, `read_token_word` reads a word along
//! with the quotes and expansions inside it, and `parse_matched_pair`
//! finds where those end. Bash only knows which words are reserved words
//! from the tokens before them, so the lexer keeps the same state that
//! the parser keeps for that, in `State`. Expansions are read the way
//! `subst.c` reads them later, and the text of an expansion such as
//! `$(...)` is lexed as commands of its own.
//!
//! Bash removes each line continuation, a `\` before a line break, as it
//! reads the text, so a word or an operator can go on on the next line.
//! It doesn't remove them in single quotes, in comments or in the body of
//! a here-document whose delimiter is quoted. So the lexer reads a copy of
//! the text without them, in `Joined`, and reads those three from the
//! text itself.

use std::borrow::Cow;
use std::ops::Range;

use super::{Kind, Tokens};

/// How deeply expansions and quotes can nest before we stop reading them
/// as such. Each level takes a few stack frames.
const MAX_DEPTH: usize = 64;

pub fn lex(text: &str, out: &mut Tokens) {
    let joined = Joined::new(text);
    let mut lexer = Lexer {
        raw: text,
        joined: &joined,
        b: joined.text.as_bytes(),
        i: 0,
        out,
        heredocs: Vec::new(),
        depth: 0,
    };
    lexer.commands(End::Eof);
    if joined.src.is_empty() {
        return;
    }
    // The line continuations that bash removed get the escape color,
    // unless they're part of a token that bash read them in.
    let mut last = 0;
    let mut src = joined.src.iter().peekable();
    for (r, kind) in std::mem::take(&mut out.0) {
        while let Some(&&c) = src.peek()
            && c < r.end
        {
            if c >= last && c + 2 <= r.start {
                out.push(c..c + 2, Kind::Escape);
            }
            src.next();
        }
        last = r.end;
        out.push(r, kind);
    }
    for &c in src {
        if c >= last {
            out.push(c..c + 2, Kind::Escape);
        }
    }
}

/// The text as bash reads it, without the line continuations that it
/// removes, and where those were.
struct Joined<'a> {
    text: Cow<'a, str>,
    /// For each line continuation, where the character after it is in
    /// `text`.
    at: Vec<usize>,
    /// For each line continuation, where it is in the source.
    src: Vec<usize>,
}

impl<'a> Joined<'a> {
    fn new(source: &'a str) -> Joined<'a> {
        let mut joined = Joined {
            text: Cow::Borrowed(source),
            at: Vec::new(),
            src: Vec::new(),
        };
        if !source.contains("\\\n") {
            return joined;
        }
        let b = source.as_bytes();
        let mut text = String::with_capacity(source.len());
        let mut copied = 0;
        // The number of backslashes right before `i`. A backslash after
        // an odd number of them is escaped, so it doesn't start a line
        // continuation.
        let mut backslashes = 0;
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && b.get(i + 1) == Some(&b'\n') && backslashes % 2 == 0 {
                text.push_str(&source[copied..i]);
                joined.at.push(text.len());
                joined.src.push(i);
                i += 2;
                copied = i;
                backslashes = 0;
                continue;
            }
            backslashes = if b[i] == b'\\' { backslashes + 1 } else { 0 };
            i += 1;
        }
        text.push_str(&source[copied..]);
        joined.text = Cow::Owned(text);
        joined
    }

    /// Where the character at `i` in `text` is in the source.
    fn src(&self, i: usize) -> usize {
        i + 2 * self.at.partition_point(|&a| a <= i)
    }

    /// Where the text up to `i` ends in the source.
    fn src_end(&self, i: usize) -> usize {
        i + 2 * self.at.partition_point(|&a| a < i)
    }

    /// Where the character at `p` in the source is in `text`. A line
    /// continuation at `p` stands for the character after it.
    fn pos(&self, p: usize) -> usize {
        p - 2 * self.src.partition_point(|&s| s < p)
    }

    /// Whether `p` in the source is the start of a line continuation that
    /// bash removes.
    fn removed(&self, p: usize) -> bool {
        self.src.binary_search(&p).is_ok()
    }
}

/// The tokens that the parser tells apart, as far as recognizing words
/// goes. Reserved words are tokens of their own, as in bash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// The start of the text or of a command substitution.
    Start,
    Newline,
    Semi,
    Amp,
    AndAnd,
    OrOr,
    Pipe,
    PipeAmp,
    /// `;;`, `;&` or `;;&`.
    CaseEnd,
    LParen,
    RParen,
    Word,
    Assignment,
    /// The number of a file descriptor before a redirection, as in `2>`.
    Number,
    Redirect,
    /// `<&` or `>&`.
    Duplicate,
    /// The `-` in `<&-`.
    Dash,
    ArithCmd,
    ArithFor,
    TimeOpt,
    TimeIgn,
    If,
    Then,
    Else,
    Elif,
    Fi,
    Case,
    Esac,
    For,
    Select,
    While,
    Until,
    Do,
    Done,
    In,
    Function,
    Time,
    LBrace,
    RBrace,
    Bang,
    CondStart,
    CondEnd,
    Coproc,
}

/// `word_token_alist` in `parse.y`.
const RESERVED: &[(&str, Tok)] = &[
    ("if", Tok::If),
    ("then", Tok::Then),
    ("else", Tok::Else),
    ("elif", Tok::Elif),
    ("fi", Tok::Fi),
    ("case", Tok::Case),
    ("esac", Tok::Esac),
    ("for", Tok::For),
    ("select", Tok::Select),
    ("while", Tok::While),
    ("until", Tok::Until),
    ("do", Tok::Do),
    ("done", Tok::Done),
    ("in", Tok::In),
    ("function", Tok::Function),
    ("time", Tok::Time),
    ("{", Tok::LBrace),
    ("}", Tok::RBrace),
    ("!", Tok::Bang),
    ("[[", Tok::CondStart),
    ("]]", Tok::CondEnd),
    ("coproc", Tok::Coproc),
];

/// Builtins after which bash reads assignments, as in `local x=1`. These
/// have the `ASSIGNMENT_BUILTIN` flag, except `eval` and `let`, which
/// `read_token_word` adds.
const ASSIGNING: &[&str] = &[
    "alias", "declare", "eval", "export", "let", "local", "readonly", "typeset",
];

/// Builtins whose other arguments name variables too, as `x` does in
/// `local x`.
const DECLARING: &[&str] = &["declare", "export", "local", "readonly", "typeset"];

/// What ends a list of commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The end of the text, or of the text inside backquotes.
    Eof,
    /// The `)` of `$(...)`, `<(...)` or `>(...)`.
    Paren,
    /// The `}` of `${ ...; }`.
    Brace,
}

/// Where text is, as far as backslashes and `$` go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Unquoted, in a word.
    Word,
    /// Inside double quotes.
    Quoted,
    /// In the body of a here-document with an unquoted delimiter.
    Heredoc,
}

impl Ctx {
    /// The color of the text that isn't an escape or an expansion.
    fn base(self) -> Option<Kind> {
        match self {
            Ctx::Word => None,
            Ctx::Quoted | Ctx::Heredoc => Some(Kind::String),
        }
    }

    /// Whether a backslash before `c` quotes it.
    fn escapes(self, c: u8) -> bool {
        match self {
            Ctx::Word => true,
            Ctx::Quoted => matches!(c, b'$' | b'`' | b'"' | b'\\' | b'\n'),
            Ctx::Heredoc => matches!(c, b'$' | b'`' | b'\\' | b'\n'),
        }
    }
}

/// What ends an arithmetic expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArithEnd {
    /// The `)` that closes this many open parentheses, as in `$((...))`.
    Parens(usize),
    /// The `]` of `$[...]`.
    Bracket,
    /// The `]` of a subscript in an assignment.
    Subscript,
    /// The `]` of a subscript in `${a[...]}`, or the `}` before it.
    BraceBracket,
    /// The `}` of `${a:1:2}`, which isn't read.
    Brace,
}

/// A here-document whose body starts after the next newline.
#[derive(Debug, Clone)]
struct Heredoc {
    /// The delimiter after quote removal.
    delimiter: Vec<u8>,
    /// Whether it came after `<<-`, which removes tabs at the start of
    /// each line.
    strip_tabs: bool,
    /// Whether any of the delimiter was quoted. Then the body has no
    /// expansions.
    quoted: bool,
}

/// What the parser knows about the tokens before the next one, as far as
/// reading words goes. The fields follow the variables of the same
/// purpose in `parse.y`, such as `last_read_token` and `parser_state`.
struct State {
    last: Tok,
    before: Tok,
    /// Whether words are patterns of a `case`, up to the next `)`.
    case_patterns: bool,
    /// The number of `case ... in` without their `esac`.
    esacs: usize,
    /// The number of `for`, `select` and `case` that may still take an
    /// `in` after their word.
    expecting_in: usize,
    /// Whether the `in` that comes next belongs to a `case`.
    in_case: bool,
    /// The number of `{` reserved words without their `}`.
    braces: usize,
    /// The number of `(` without their `)`, so that `$(...)` ends at the
    /// right one.
    parens: usize,
    /// Whether the simple command has its name. Later words are its
    /// arguments.
    named: bool,
    /// Whether the command is a builtin such as `local`, whose arguments
    /// can be assignments.
    assigning: bool,
    /// Whether the arguments of the command name variables.
    declaring: bool,
    /// Whether words are the list after `for x in`.
    list: bool,
    /// After `<<` or `<<-`, whether it strips tabs. The next word is the
    /// delimiter of a here-document.
    heredoc: Option<bool>,
}

impl State {
    fn new() -> State {
        State {
            last: Tok::Start,
            before: Tok::Start,
            case_patterns: false,
            esacs: 0,
            expecting_in: 0,
            in_case: false,
            braces: 0,
            parens: 0,
            named: false,
            assigning: false,
            declaring: false,
            list: false,
            heredoc: None,
        }
    }

    fn push(&mut self, tok: Tok) {
        self.before = self.last;
        self.last = tok;
        if !matches!(
            tok,
            Tok::Word | Tok::Assignment | Tok::Number | Tok::Redirect | Tok::Duplicate | Tok::Dash
        ) {
            self.named = false;
            self.assigning = false;
            self.declaring = false;
        }
        if tok != Tok::Word {
            self.list = false;
        }
        self.heredoc = None;
    }

    /// `reserved_word_acceptable` in `parse.y`: whether a reserved word
    /// can come next.
    fn reserved_ok(&self) -> bool {
        use Tok::*;
        match self.last {
            Start | Newline | Semi | LParen | RParen | Pipe | Amp | LBrace | RBrace | AndAnd
            | ArithCmd | Bang | PipeAmp | CondEnd | Do | Done | Elif | Else | Esac | Fi | If
            | OrOr | CaseEnd | Then | Time | TimeOpt | TimeIgn | Coproc | Until | While => true,
            Word => matches!(self.before, Coproc | Function),
            _ => false,
        }
    }

    /// `time_command_acceptable` in `parse.y`.
    fn time_ok(&self) -> bool {
        use Tok::*;
        match self.last {
            Start | Semi | Newline => self.before != Pipe,
            AndAnd | OrOr | Amp | While | Do | Until | If | Then | Elif | Else | LBrace
            | LParen | RParen | Bang | Time | TimeOpt | TimeIgn => true,
            _ => false,
        }
    }

    /// The reserved word that `w` is here, if any, and what reading it
    /// changes. This follows `special_case_tokens` and then
    /// `CHECK_FOR_RESERVED_WORD` in `parse.y`.
    fn reserved(&mut self, w: &str) -> Option<Tok> {
        use Tok::*;
        let after_for = self.last == Word && matches!(self.before, For | Case | Select);
        if w == "in"
            && (after_for || (self.expecting_in > 0 && matches!(self.last, Word | Newline)))
        {
            if self.in_case {
                self.case_patterns = true;
                self.esacs += 1;
            }
            self.expecting_in = self.expecting_in.saturating_sub(1);
            self.in_case = false;
            return Some(In);
        }
        if w == "do"
            && ((self.expecting_in > 0 && matches!(self.last, Newline | Semi))
                || (self.last == Word && matches!(self.before, For | Select)))
        {
            self.expecting_in = self.expecting_in.saturating_sub(1);
            self.in_case = false;
            return Some(Do);
        }
        if self.esacs > 0 && self.last == In && w == "esac" {
            self.esacs -= 1;
            self.case_patterns = false;
            return Some(Esac);
        }
        if self.last == ArithFor && w == "do" {
            return Some(Do);
        }
        if self.last == ArithFor && w == "{" {
            self.braces += 1;
            return Some(LBrace);
        }
        if self.braces > 0 && self.reserved_ok() && w == "}" {
            self.braces -= 1;
            return Some(RBrace);
        }
        if self.last == Time && w == "-p" {
            return Some(TimeOpt);
        }
        if matches!(self.last, Time | TimeOpt) && w == "--" {
            return Some(TimeIgn);
        }
        if !self.reserved_ok() {
            return None;
        }
        let tok = RESERVED.iter().find(|(r, _)| *r == w)?.1;
        if self.case_patterns && (tok != Esac || matches!(self.last, Pipe | LParen)) {
            return None;
        }
        if tok == Time && !self.time_ok() {
            return None;
        }
        match tok {
            Esac => {
                self.case_patterns = false;
                self.esacs = self.esacs.saturating_sub(1);
            }
            Case => self.in_case = true,
            For => self.in_case = false,
            LBrace => self.braces += 1,
            RBrace => self.braces = self.braces.saturating_sub(1),
            _ => {}
        }
        Some(tok)
    }
}

/// What `Lexer::assignment` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Assignment {
    No,
    Yes,
    /// A word with a name and brackets, as in `a[1]`, which isn't an
    /// assignment.
    Word,
}

/// Where the lexer was, so that it can go back after reading ahead.
struct Mark {
    i: usize,
    len: usize,
    last: Option<(Range<usize>, Kind)>,
    heredocs: usize,
}

struct Lexer<'a, 'o> {
    /// The source, which has the line continuations.
    raw: &'a str,
    joined: &'a Joined<'a>,
    /// The part of the joined text that can be read now. It's shorter
    /// than the whole inside backquotes and here-documents.
    b: &'a [u8],
    i: usize,
    out: &'o mut Tokens,
    /// Here-documents whose bodies start after the next newline.
    heredocs: Vec<Heredoc>,
    /// How many expansions and quotes we're inside of.
    depth: usize,
}

impl<'a> Lexer<'a, '_> {
    fn at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    /// Color the range `r` of the joined text. The line continuations
    /// inside it keep their own color.
    fn push(&mut self, r: Range<usize>, kind: Kind) {
        let j = self.joined;
        let mut s = r.start;
        let first = j.at.partition_point(|&a| a <= s);
        for &a in &j.at[first..] {
            if a >= r.end {
                break;
            }
            if a > s {
                self.out.push(j.src(s)..j.src_end(a), kind);
                s = a;
            }
        }
        self.out.push(j.src(s)..j.src_end(r.end), kind);
    }

    /// Where the part of the text that can be read now ends in the
    /// source.
    fn src_limit(&self) -> usize {
        self.joined.src(self.b.len())
    }

    /// The part of the source that can be read now.
    fn raw_bytes(&self) -> &'a [u8] {
        &self.raw.as_bytes()[..self.src_limit()]
    }

    /// Color `n` bytes from the current position with `kind`, if any, and
    /// move past them.
    fn take(&mut self, n: usize, kind: Option<Kind>) {
        let i = self.i;
        if let Some(kind) = kind {
            self.push(i..i + n, kind);
        }
        self.i += n;
    }

    /// The length of the character at `i`, from its first byte.
    fn char_len(&self, i: usize) -> usize {
        char_len(self.b, i)
    }

    fn mark(&self) -> Mark {
        Mark {
            i: self.i,
            len: self.out.0.len(),
            last: self.out.0.last().cloned(),
            heredocs: self.heredocs.len(),
        }
    }

    fn reset(&mut self, m: Mark) {
        self.i = m.i;
        self.out.0.truncate(m.len);
        // A token pushed after the mark may have been joined to the last
        // one before it.
        if let (Some(last), Some(slot)) = (m.last, self.out.0.last_mut()) {
            *slot = last;
        }
        self.heredocs.truncate(m.heredocs);
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

    /// Lex commands up to `end`, and the token that ends them.
    fn commands(&mut self, end: End) {
        let outer = std::mem::take(&mut self.heredocs);
        let mut st = State::new();
        loop {
            self.blanks();
            let i = self.i;
            let Some(c) = self.at(i) else { break };
            match c {
                b'#' => {
                    if self.comment() {
                        st.push(Tok::Newline);
                        self.here_documents(end);
                    }
                }
                b'\n' => {
                    self.i += 1;
                    st.push(Tok::Newline);
                    self.here_documents(end);
                }
                b')' if end == End::Paren && st.parens == 0 && !st.case_patterns => {
                    self.take(1, Some(Kind::Punctuation));
                    break;
                }
                b'}' if end == End::Brace && st.braces == 0 && st.reserved_ok() => {
                    self.take(1, Some(Kind::Punctuation));
                    break;
                }
                b'(' if self.at(i + 1) == Some(b'(')
                    && (st.last == Tok::For || st.reserved_ok())
                    && self.arith_command() =>
                {
                    st.push(if st.last == Tok::For {
                        Tok::ArithFor
                    } else {
                        Tok::ArithCmd
                    });
                }
                b';' | b'&' | b'|' | b'(' | b')' => self.operator(&mut st),
                b'<' | b'>' if self.at(i + 1) != Some(b'(') => self.operator(&mut st),
                _ => self.word(&mut st, end),
            }
        }
        // When a command substitution ends with here-documents still open,
        // bash reads their bodies from the next line, before the bodies
        // of those outside it. The rest of the line is still commands.
        // Inside backquotes, there are no more lines to read.
        let inner = std::mem::replace(&mut self.heredocs, outer);
        if end != End::Eof {
            self.heredocs.splice(0..0, inner);
        }
    }

    /// Skip spaces and tabs.
    fn blanks(&mut self) {
        while matches!(self.at(self.i), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    /// Read a comment. Bash doesn't remove a line continuation in one, so
    /// a comment can end at one. Return whether it did, which means that
    /// the line break after it has been read too.
    fn comment(&mut self) -> bool {
        let j = self.joined;
        let b = self.raw_bytes();
        let start = j.src(self.i);
        let end = b[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |n| start + n);
        self.out.push(start..end, Kind::Comment);
        if end > 0 && j.removed(end - 1) {
            self.i = j.pos(end + 1);
            true
        } else {
            self.i = j.pos(end);
            false
        }
    }

    fn operator(&mut self, st: &mut State) {
        use Kind::{Operator, Punctuation};
        let (n, tok, kind) = match &self.b[self.i..] {
            [b';', b';', b'&', ..] => (3, Tok::CaseEnd, Punctuation),
            [b';', b';' | b'&', ..] => (2, Tok::CaseEnd, Punctuation),
            [b';', ..] => (1, Tok::Semi, Punctuation),
            [b'&', b'&', ..] => (2, Tok::AndAnd, Operator),
            [b'&', b'>', b'>', ..] => (3, Tok::Redirect, Operator),
            [b'&', b'>', ..] => (2, Tok::Redirect, Operator),
            [b'&', ..] => (1, Tok::Amp, Operator),
            [b'|', b'|', ..] => (2, Tok::OrOr, Operator),
            [b'|', b'&', ..] => (2, Tok::PipeAmp, Operator),
            [b'|', ..] => (1, Tok::Pipe, Operator),
            [b'<', b'<', b'-' | b'<', ..] => (3, Tok::Redirect, Operator),
            [b'<' | b'>', b'&', ..] => (2, Tok::Duplicate, Operator),
            [b'<', b'<' | b'>', ..] | [b'>', b'>' | b'|', ..] => (2, Tok::Redirect, Operator),
            [b'<' | b'>', ..] => (1, Tok::Redirect, Operator),
            [b'(', ..] => (1, Tok::LParen, Punctuation),
            _ => (1, Tok::RParen, Punctuation),
        };
        let heredoc = match &self.b[self.i..self.i + n] {
            b"<<" => Some(false),
            b"<<-" => Some(true),
            _ => None,
        };
        self.take(n, Some(kind));
        match tok {
            Tok::CaseEnd => st.case_patterns = true,
            // A `(` in a case pattern is optional and doesn't need a `)`
            // of its own.
            Tok::LParen if !st.case_patterns => st.parens += 1,
            Tok::RParen if st.case_patterns => st.case_patterns = false,
            Tok::RParen => st.parens = st.parens.saturating_sub(1),
            _ => {}
        }
        st.push(tok);
        st.heredoc = heredoc;
    }

    /// If the `((` here starts an arithmetic command, read it. Bash reads
    /// it as a subshell inside a subshell if the `)` that matches the
    /// second `(` doesn't have another `)` right after it. Bash reads
    /// that one without removing a line continuation before it. When
    /// there's no `)` at all, which is an error, we guess arithmetic.
    fn arith_command(&mut self) -> bool {
        let m = self.mark();
        self.take(2, Some(Kind::Punctuation));
        if !self.arith(ArithEnd::Parens(1)) {
            return true;
        }
        let after = self.joined.src_end(self.i);
        if self.at(self.i) == Some(b')') && self.raw.as_bytes().get(after) == Some(&b')') {
            self.take(1, Some(Kind::Punctuation));
            return true;
        }
        self.reset(m);
        false
    }

    /// If all of the word at `i` stands for itself, without quotes,
    /// escapes or expansions, its text.
    fn literal_word(&self, i: usize) -> Option<&'a str> {
        let rest: &'a [u8] = &self.b[i..];
        let n = rest
            .iter()
            .position(|&c| is_break(c) || matches!(c, b'\'' | b'"' | b'\\' | b'`' | b'$'))
            .unwrap_or(rest.len());
        match rest.get(n) {
            None => {}
            Some(&c) if !is_break(c) => return None,
            // An extended pattern such as `@(a|b)`, or a process
            // substitution.
            Some(b'(') if n > 0 && is_pattern_char(rest[n - 1]) => return None,
            Some(b'<' | b'>') if rest.get(n + 1) == Some(&b'(') => return None,
            _ => {}
        }
        if n == 0 {
            return None;
        }
        std::str::from_utf8(&rest[..n]).ok()
    }

    /// Read a word, and what it means to the parser.
    fn word(&mut self, st: &mut State, end: End) {
        let start = self.i;
        if st.last == Tok::Duplicate && self.b[start] == b'-' {
            self.take(1, Some(Kind::Operator));
            st.push(Tok::Dash);
            return;
        }
        // Without `extglob`, which is off by default, `!(` is `!` before
        // a subshell. With it, the `(` would start a pattern, but a
        // pattern makes no sense where a command starts.
        if self.b[start] == b'!'
            && self.at(start + 1) == Some(b'(')
            && st.reserved_ok()
            && !st.case_patterns
        {
            self.take(1, Some(Kind::Operator));
            st.push(Tok::Bang);
            return;
        }
        let literal = self.literal_word(start);
        if let Some(w) = literal {
            let after = self.at(start + w.len());
            if w.bytes().all(|c| c.is_ascii_digit())
                && (matches!(after, Some(b'<' | b'>')) || st.last == Tok::Duplicate)
            {
                self.take(w.len(), Some(Kind::Number));
                st.push(Tok::Number);
                return;
            }
            if let Some(tok) = st.reserved(w) {
                let kind = match tok {
                    Tok::LBrace | Tok::RBrace => Some(Kind::Punctuation),
                    Tok::Bang => Some(Kind::Operator),
                    Tok::TimeOpt | Tok::TimeIgn => None,
                    _ => Some(Kind::Keyword),
                };
                self.take(w.len(), kind);
                st.push(tok);
                match tok {
                    Tok::In if !st.case_patterns => st.list = true,
                    Tok::CondStart => {
                        self.cond(end);
                        st.push(Tok::CondEnd);
                    }
                    _ => {}
                }
                return;
            }
        }
        if let Some(strip_tabs) = st.heredoc {
            self.delimiter(strip_tabs);
            st.push(Tok::Word);
            return;
        }
        let identifier = |name: bool| match literal {
            Some(w) if !name || is_name(w) => Some(Kind::Identifier),
            _ => None,
        };
        match st.last {
            Tok::Redirect | Tok::Duplicate => self.parts(false),
            _ if st.case_patterns || st.list => self.parts(false),
            Tok::For | Tok::Select => {
                self.word_as(identifier(true));
                st.expecting_in += 1;
            }
            Tok::Case => {
                self.parts(false);
                st.expecting_in += 1;
            }
            Tok::Function => self.word_as(identifier(false)),
            _ => {
                self.simple_word(st, literal);
                return;
            }
        }
        st.push(Tok::Word);
    }

    /// Read a word of a simple command, which is an assignment, the name
    /// of the command or one of its arguments.
    fn simple_word(&mut self, st: &mut State, literal: Option<&'a str>) {
        // Bash also takes an assignment after `coproc NAME`.
        let command = !st.named || (st.last == Tok::Word && st.before == Tok::Coproc);
        if command || st.assigning {
            match self.assignment(command) {
                Assignment::Yes => {
                    st.push(if st.named { Tok::Word } else { Tok::Assignment });
                    return;
                }
                Assignment::Word => {
                    st.push(Tok::Word);
                    st.named = true;
                    return;
                }
                Assignment::No => {}
            }
        }
        if literal.is_some_and(|w| self.redirect_word(w)) {
            st.push(Tok::Word);
            return;
        }
        if !st.named {
            let name = literal.filter(|w| is_command_name(w));
            self.word_as(name.map(|_| Kind::Identifier));
            st.push(Tok::Word);
            st.named = true;
            st.assigning = literal.is_some_and(|w| ASSIGNING.contains(&w));
            st.declaring = literal.is_some_and(|w| DECLARING.contains(&w));
            return;
        }
        match literal {
            Some(w) if st.declaring && is_name(w) => self.take(w.len(), Some(Kind::Identifier)),
            _ => self.parts(false),
        }
        st.push(Tok::Word);
    }

    /// Color a literal word as a whole with `kind`, or read its parts.
    fn word_as(&mut self, kind: Option<Kind>) {
        match kind {
            Some(kind) => {
                let n = self.literal_word(self.i).map_or(0, str::len);
                self.take(n, Some(kind));
            }
            None => self.parts(false),
        }
    }

    /// If the literal word `w` is a redirection such as `{fd}>`, which
    /// puts the file descriptor in the variable `fd`, read it.
    fn redirect_word(&mut self, w: &str) -> bool {
        let Some(name) = w.strip_prefix('{').and_then(|w| w.strip_suffix('}')) else {
            return false;
        };
        if !is_name(name) || !matches!(self.at(self.i + w.len()), Some(b'<' | b'>')) {
            return false;
        }
        self.take(1, Some(Kind::Punctuation));
        self.take(name.len(), Some(Kind::Identifier));
        self.take(1, Some(Kind::Punctuation));
        true
    }

    /// If the word here is an assignment such as `x=1`, `x+=1` or
    /// `a[i]=1`, read it. Where a command starts, bash reads the brackets
    /// after a name as part of the word whether or not an `=` follows,
    /// so with `command` this reads them too.
    fn assignment(&mut self, command: bool) -> Assignment {
        let start = self.i;
        let n = name_len(&self.b[start..]);
        if n == 0 {
            return Assignment::No;
        }
        let m = self.mark();
        self.take(n, Some(Kind::Identifier));
        let op = |l: &Self| match (l.at(l.i), l.at(l.i + 1)) {
            (Some(b'='), _) => 1,
            (Some(b'+'), Some(b'=')) => 2,
            _ => 0,
        };
        if self.at(self.i) == Some(b'[') {
            let open = self.i;
            self.take(1, Some(Kind::Punctuation));
            if !self.arith(ArithEnd::Subscript) || op(self) == 0 {
                self.reset(m);
                if !command {
                    return Assignment::No;
                }
                self.i = open;
                self.group(b'[');
                self.parts(false);
                return Assignment::Word;
            }
        }
        let n = op(self);
        if n == 0 {
            self.reset(m);
            return Assignment::No;
        }
        self.take(n, Some(Kind::Operator));
        if self.at(self.i) == Some(b'(') {
            self.compound();
        }
        self.parts(false);
        Assignment::Yes
    }

    /// Read the list of a compound assignment such as `a=(1 2 [k]=v)`.
    fn compound(&mut self) {
        self.take(1, Some(Kind::Punctuation));
        loop {
            self.blanks();
            let i = self.i;
            let Some(c) = self.at(i) else { return };
            match c {
                b'\n' => self.i += 1,
                b'#' => {
                    self.comment();
                }
                b')' => {
                    self.take(1, Some(Kind::Punctuation));
                    return;
                }
                b'<' | b'>' if self.at(i + 1) == Some(b'(') => self.parts(false),
                c if is_break(c) => return,
                b'[' => {
                    self.take(1, Some(Kind::Punctuation));
                    if self.arith(ArithEnd::Subscript) {
                        match (self.at(self.i), self.at(self.i + 1)) {
                            (Some(b'='), _) => self.take(1, Some(Kind::Operator)),
                            (Some(b'+'), Some(b'=')) => self.take(2, Some(Kind::Operator)),
                            _ => {}
                        }
                    }
                    self.parts(false);
                }
                _ => self.parts(false),
            }
        }
    }

    /// Read the delimiter of a here-document, and remember it until the
    /// next newline.
    fn delimiter(&mut self, strip_tabs: bool) {
        let start = self.i;
        let m = self.mark();
        self.parts(false);
        let end = self.i;
        self.reset(m);
        self.i = end;
        // Bash keeps the line continuations in single quotes, so quote
        // removal needs the source.
        let j = self.joined;
        let src = j.src(start)..j.src_end(end);
        self.out.push(src.clone(), Kind::Attribute);
        let (delimiter, quoted) = quote_removal(&self.raw.as_bytes()[src]);
        self.heredocs.push(Heredoc {
            delimiter,
            strip_tabs,
            quoted,
        });
    }

    /// Read the bodies of the pending here-documents, which start here.
    /// This follows `make_here_document` in `make_cmd.c`.
    fn here_documents(&mut self, end: End) {
        let pending = std::mem::take(&mut self.heredocs);
        // A line that starts with the delimiter also ends the body inside
        // a command substitution if the `)` comes later on it.
        let close = match end {
            End::Eof => None,
            End::Paren => Some(b')'),
            End::Brace => Some(b'}'),
        };
        for (n, doc) in pending.iter().enumerate() {
            // Bash reads the lines of the body as they are when the
            // delimiter is quoted, and removes line continuations
            // otherwise, as in the joined text.
            let j = self.joined;
            let (b, body) = if doc.quoted {
                (self.raw_bytes(), j.src(self.i))
            } else {
                (self.b, self.i)
            };
            let found = heredoc_end(b, body, doc, close);
            let (body_end, next) = found
                .as_ref()
                .map_or((b.len(), b.len()), |f| (f.body_end, f.next));
            if doc.quoted {
                self.out.push(body..body_end, Kind::String);
                if let Some(f) = &found {
                    self.out.push(f.delimiter.clone(), Kind::Attribute);
                }
                self.i = j.pos(next);
            } else {
                self.heredoc_body(body, body_end);
                if let Some(f) = &found {
                    self.push(f.delimiter.clone(), Kind::Attribute);
                }
                self.i = next;
            }
            if found.is_some_and(|f| f.rest_of_line) {
                // The other here-documents start after the next newline.
                self.heredocs.extend_from_slice(&pending[n + 1..]);
                return;
            }
        }
    }

    /// Color the body of a here-document with an unquoted delimiter, from
    /// `start` to `end`.
    fn heredoc_body(&mut self, start: usize, end: usize) {
        self.i = start;
        self.within(end, |l| {
            while let Some(c) = l.at(l.i) {
                match c {
                    b'\\' => l.escape(Ctx::Heredoc),
                    b'$' => l.dollar(Ctx::Heredoc),
                    b'`' => l.backquote(),
                    _ => l.run(b"\\$`", Some(Kind::String)),
                }
            }
        });
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

    /// Read the parts of a word up to a character that ends it. In the
    /// regular expression after `=~`, `|` and parentheses don't end it.
    fn parts(&mut self, regex: bool) {
        while let Some(c) = self.at(self.i) {
            let next = self.at(self.i + 1);
            match c {
                b'\\' => self.escape(Ctx::Word),
                b'\'' => self.single_quote(),
                b'"' => self.double_quote(),
                b'`' => self.backquote(),
                // In `$@(a)`, the `@(` starts a pattern, and the `$` is
                // just a `$`.
                b'$' if matches!(next, Some(b'@' | b'*' | b'?' | b'!'))
                    && self.at(self.i + 2) == Some(b'(') =>
                {
                    self.i += 1;
                }
                b'$' => self.dollar(Ctx::Word),
                b'<' | b'>' if next == Some(b'(') => self.process_substitution(),
                b'@' | b'*' | b'+' | b'?' | b'!' if next == Some(b'(') => {
                    self.i += 1;
                    self.group(b'(');
                }
                b'(' if regex => self.group(b'('),
                b'|' if regex => self.i += 1,
                c if is_break(c) => return,
                _ => self.run(b"\\'\"`$<>@*+?!()|;& \t\n", None),
            }
        }
    }

    /// Read text in parentheses or brackets that belongs to a word, as in
    /// the pattern `@(a|b)` or `a[1 + 1]`. Quotes and expansions inside
    /// keep their colors.
    fn group(&mut self, open: u8) {
        let close = if open == b'(' { b')' } else { b']' };
        // `parse_matched_pair` only nests quotes in parentheses, so the
        // parentheses of a `$(...)` there count like any others. In
        // brackets, it nests expansions too.
        let nests = open == b'[';
        if self.depth >= MAX_DEPTH {
            self.i += 1;
            return;
        }
        self.depth += 1;
        let mut depth = 0;
        while let Some(c) = self.at(self.i) {
            let next = self.at(self.i + 1);
            match c {
                _ if c == open => {
                    depth += 1;
                    self.i += 1;
                }
                _ if c == close => {
                    self.i += 1;
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                b'\\' => self.escape(Ctx::Word),
                b'\'' => self.single_quote(),
                b'"' => self.double_quote(),
                b'`' => self.backquote(),
                b'$' if !nests && matches!(next, Some(b'(' | b'{' | b'[')) => self.i += 1,
                b'$' => self.dollar(Ctx::Word),
                b'<' | b'>' if nests && next == Some(b'(') => self.process_substitution(),
                _ => self.run(b"()[]\\'\"`$<>", None),
            }
        }
        self.depth -= 1;
    }

    /// Read a backslash and the character after it.
    fn escape(&mut self, ctx: Ctx) {
        let i = self.i;
        let Some(c) = self.at(i + 1) else {
            self.take(1, ctx.base());
            return;
        };
        let kind = if ctx.escapes(c) {
            Some(Kind::Escape)
        } else {
            ctx.base()
        };
        let n = 1 + self.char_len(i + 1);
        self.take(n, kind);
    }

    /// Read a string in single quotes, from the source, because bash
    /// doesn't remove line continuations in it.
    fn single_quote(&mut self) {
        let b = self.raw_bytes();
        let start = self.joined.src(self.i);
        let end = b[start + 1..]
            .iter()
            .position(|&c| c == b'\'')
            .map_or(b.len(), |n| start + n + 2);
        self.out.push(start..end, Kind::String);
        self.i = self.joined.pos(end);
    }

    fn double_quote(&mut self) {
        self.take(1, Some(Kind::String));
        while let Some(c) = self.at(self.i) {
            match c {
                b'"' => {
                    self.take(1, Some(Kind::String));
                    return;
                }
                b'\\' => self.escape(Ctx::Quoted),
                b'$' => self.dollar(Ctx::Quoted),
                b'`' => self.backquote(),
                _ => self.run(b"\"\\$`", Some(Kind::String)),
            }
        }
    }

    /// Read a string such as `$'a\n'`, whose escapes are read the way
    /// `ansicstr` in `lib/sh/strtrans.c` reads them. Bash doesn't remove
    /// line continuations inside the quotes, so we read it from the
    /// source.
    fn ansi_c_string(&mut self) {
        let b = self.raw_bytes();
        let open = self.joined.src(self.i + 1);
        self.take(2, Some(Kind::String));
        // A backslash quotes the character after it, so `\'` doesn't end
        // the string.
        let mut j = open + 1;
        while j < b.len() && b[j] != b'\'' {
            j += if b[j] == b'\\' { 2 } else { 1 };
        }
        let close = j.min(b.len());
        let body = &b[..close];
        let mut i = open + 1;
        while i < close {
            if b[i] != b'\\' {
                let n = body[i..]
                    .iter()
                    .position(|&c| c == b'\\')
                    .unwrap_or(close - i);
                self.out.push(i..i + n, Kind::String);
                i += n;
                continue;
            }
            match ansi_escape(&body[i..]) {
                0 => {
                    let n = if i + 1 < close {
                        1 + char_len(body, i + 1)
                    } else {
                        1
                    };
                    self.out.push(i..i + n, Kind::String);
                    i += n;
                }
                n => {
                    // `\c` takes only the first byte of the character after
                    // it.
                    let end = i + n;
                    let end = end
                        + body[end..]
                            .iter()
                            .take_while(|&&c| (0x80..0xc0).contains(&c))
                            .count();
                    self.out.push(i..end, Kind::Escape);
                    i = end;
                }
            }
        }
        let end = if b.get(close) == Some(&b'\'') {
            self.out.push(close..close + 1, Kind::String);
            close + 1
        } else {
            close
        };
        self.i = self.joined.pos(end);
    }

    /// Read what a `$` starts.
    fn dollar(&mut self, ctx: Ctx) {
        let i = self.i;
        let next = self.at(i + 1);
        if self.depth >= MAX_DEPTH && matches!(next, Some(b'(' | b'{' | b'[' | b'\'' | b'"')) {
            self.take(1, ctx.base());
            return;
        }
        self.depth += 1;
        match next {
            Some(b'(') if self.at(i + 2) == Some(b'(') => {
                self.take(3, Some(Kind::Punctuation));
                self.arith(ArithEnd::Parens(2));
            }
            Some(b'(') => {
                self.take(2, Some(Kind::Punctuation));
                self.commands(End::Paren);
            }
            // bash 5.3 runs `${ ...; }` and `${| ...; }` in the current
            // shell.
            Some(b'{') if matches!(self.at(i + 2), Some(b' ' | b'\t' | b'\n' | b'|')) => {
                let n = if self.at(i + 2) == Some(b'|') { 3 } else { 2 };
                self.take(n, Some(Kind::Punctuation));
                self.commands(End::Brace);
            }
            Some(b'{') => self.parameter(ctx),
            Some(b'[') => {
                self.take(2, Some(Kind::Punctuation));
                self.arith(ArithEnd::Bracket);
            }
            Some(b'\'') if ctx == Ctx::Word => self.ansi_c_string(),
            Some(b'"') if ctx == Ctx::Word => {
                self.take(1, Some(Kind::String));
                self.double_quote();
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                self.take(1, Some(Kind::Punctuation));
                let n = name_len(&self.b[self.i..]);
                self.take(n, Some(Kind::Identifier));
            }
            Some(c) if c.is_ascii_digit() || b"@*#?-$!".contains(&c) => {
                self.take(1, Some(Kind::Punctuation));
                self.take(1, Some(Kind::Identifier));
            }
            _ => self.take(1, ctx.base()),
        }
        self.depth -= 1;
    }

    /// Read a parameter expansion such as `${a[i]:-x}`. Bash finds its end
    /// with `parse_matched_pair`, and `parameter_brace_expand` in `subst.c`
    /// reads what's inside.
    fn parameter(&mut self, ctx: Ctx) {
        self.take(2, Some(Kind::Punctuation));
        // `${#x}` is the length of `x`, and `${!x}` names the variable
        // that `x` holds, but `${#}` and `${!}` are parameters.
        if matches!(self.at(self.i), Some(b'#' | b'!'))
            && !matches!(self.at(self.i + 1), Some(b'}') | None)
        {
            self.take(1, Some(Kind::Operator));
        }
        let name = name_len(&self.b[self.i..]);
        let n = match self.at(self.i) {
            _ if name > 0 => name,
            Some(c) if c.is_ascii_digit() => self.b[self.i..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count(),
            // `${$(...)}` holds a command substitution, which is an
            // error when it runs, but bash reads it.
            Some(b'$') if matches!(self.at(self.i + 1), Some(b'(' | b'{' | b'[')) => 0,
            Some(c) if b"@*#?-$!".contains(&c) => 1,
            _ => 0,
        };
        self.take(n, Some(Kind::Identifier));
        if name > 0 && self.at(self.i) == Some(b'[') {
            self.take(1, Some(Kind::Punctuation));
            self.arith(ArithEnd::BraceBracket);
        }
        enum Then {
            Word,
            Replace,
            Arith,
        }
        let (n, then) = match &self.b[self.i..] {
            [b'}', ..] => {
                self.take(1, Some(Kind::Punctuation));
                return;
            }
            [b':', b'-' | b'=' | b'?' | b'+', ..]
            | [b'#', b'#', ..]
            | [b'%', b'%', ..]
            | [b'^', b'^', ..]
            | [b',', b',', ..] => (2, Then::Word),
            [b'/', b'/' | b'#' | b'%', ..] => (2, Then::Replace),
            [b'/', ..] => (1, Then::Replace),
            [b':', ..] => (1, Then::Arith),
            [
                b'-' | b'=' | b'?' | b'+' | b'#' | b'%' | b'^' | b',' | b'@',
                ..,
            ] => (1, Then::Word),
            _ => (0, Then::Word),
        };
        self.take(n, Some(Kind::Operator));
        match then {
            Then::Arith => {
                self.arith(ArithEnd::Brace);
            }
            Then::Word => self.brace_word(ctx, false),
            Then::Replace => {
                self.brace_word(ctx, true);
                if self.at(self.i) == Some(b'/') {
                    self.take(1, Some(Kind::Operator));
                    self.brace_word(ctx, false);
                }
            }
        }
        if self.at(self.i) == Some(b'}') {
            self.take(1, Some(Kind::Punctuation));
        }
    }

    /// Read the word in a parameter expansion, up to its `}`, or up to a
    /// `/` if `slash`.
    fn brace_word(&mut self, ctx: Ctx, slash: bool) {
        while let Some(c) = self.at(self.i) {
            match c {
                b'}' => return,
                b'/' if slash => return,
                b'\\' => self.escape(if ctx == Ctx::Word {
                    Ctx::Word
                } else {
                    Ctx::Quoted
                }),
                b'\'' => self.single_quote(),
                b'"' => self.double_quote(),
                b'`' => self.backquote(),
                // `parse_matched_pair` reads `$'...'` here even inside
                // double quotes, so a `\'` in it doesn't end it.
                b'$' if self.at(self.i + 1) == Some(b'\'') => self.ansi_c_string(),
                b'$' => self.dollar(ctx),
                b'<' | b'>' if self.at(self.i + 1) == Some(b'(') => self.process_substitution(),
                _ => self.run(b"}/\\'\"`$<>", ctx.base()),
            }
        }
    }

    /// Read an arithmetic expression up to `end`, and the `)` or `]` that
    /// ends it, but not a `}`. Return whether it found its end.
    fn arith(&mut self, end: ArithEnd) -> bool {
        use ArithEnd::*;
        let mut depth = match end {
            Parens(n) => n,
            _ => 1,
        };
        let brackets = matches!(end, Bracket | Subscript | BraceBracket);
        // In `$((...))` and `$[...]`, `parse_matched_pair` only nests
        // quotes and `$(...)`, so a `)` inside `${...}` can end them. We
        // read the `${` and `}` there as tokens on their own.
        let nests = !matches!(end, Parens(_) | Bracket);
        while let Some(c) = self.at(self.i) {
            let next = self.at(self.i + 1);
            match c {
                b' ' | b'\t' | b'\n' => self.i += 1,
                b'(' | b')' | b'[' | b']' => {
                    self.take(1, Some(Kind::Punctuation));
                    let counts = if matches!(c, b'(' | b')') {
                        matches!(end, Parens(_))
                    } else {
                        brackets
                    };
                    if counts {
                        if matches!(c, b'(' | b'[') {
                            depth += 1;
                        } else {
                            depth -= 1;
                            if depth == 0 {
                                return true;
                            }
                        }
                    }
                }
                b'}' if matches!(end, Brace | BraceBracket) => return true,
                b'}' => self.take(1, Some(Kind::Punctuation)),
                b'$' if !nests
                    && next == Some(b'{')
                    && !matches!(self.at(self.i + 2), Some(b' ' | b'\t' | b'\n' | b'|')) =>
                {
                    self.take(2, Some(Kind::Punctuation));
                    if matches!(self.at(self.i), Some(b'#' | b'!')) {
                        self.take(1, Some(Kind::Operator));
                    }
                }
                b'$' if !nests && next == Some(b'[') => self.i += 1,
                b'0'..=b'9' => {
                    let n = self.b[self.i..]
                        .iter()
                        .take_while(|&&c| {
                            c.is_ascii_alphanumeric() || matches!(c, b'_' | b'@' | b'#')
                        })
                        .count();
                    self.take(n, Some(Kind::Number));
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let n = name_len(&self.b[self.i..]);
                    self.take(n, Some(Kind::Identifier));
                }
                b'$' => self.dollar(Ctx::Word),
                b'\'' => self.single_quote(),
                b'"' => self.double_quote(),
                b'`' => self.backquote(),
                b'\\' => self.escape(Ctx::Word),
                b';' => self.take(1, Some(Kind::Punctuation)),
                _ if is_arith_operator(c) => {
                    let n = self.b[self.i..]
                        .iter()
                        .take_while(|&&c| is_arith_operator(c))
                        .count();
                    self.take(n, Some(Kind::Operator));
                }
                _ => {
                    let n = self.char_len(self.i);
                    self.i += n;
                }
            }
        }
        false
    }

    /// Read a command substitution in backquotes. It ends at the next
    /// backquote that no backslash quotes, and what's inside is read as
    /// commands of its own.
    fn backquote(&mut self) {
        if self.depth >= MAX_DEPTH {
            self.i += 1;
            return;
        }
        let start = self.i;
        let mut j = start + 1;
        while j < self.b.len() && self.b[j] != b'`' {
            j += if self.b[j] == b'\\' { 2 } else { 1 };
        }
        let close = j.min(self.b.len());
        self.take(1, Some(Kind::Punctuation));
        self.depth += 1;
        self.within(close, |l| l.commands(End::Eof));
        self.depth -= 1;
        self.i = close;
        if self.at(close) == Some(b'`') {
            self.take(1, Some(Kind::Punctuation));
        }
    }

    /// Read a process substitution such as `<(cmd)`.
    fn process_substitution(&mut self) {
        if self.depth >= MAX_DEPTH {
            self.i += 1;
            return;
        }
        self.depth += 1;
        // `parse_comsub` reads `<((` as it reads `$((`.
        if self.at(self.i + 2) == Some(b'(') {
            self.take(3, Some(Kind::Punctuation));
            self.arith(ArithEnd::Parens(2));
        } else {
            self.take(2, Some(Kind::Punctuation));
            self.commands(End::Paren);
        }
        self.depth -= 1;
    }

    /// Read a conditional command after its `[[`, up to its `]]`. This
    /// follows `cond_term` in `parse.y`. A token that can't be in one
    /// ends it, for the commands to read.
    fn cond(&mut self, end: End) {
        #[derive(PartialEq)]
        enum Expect {
            Term,
            /// The operand of a unary operator such as `-f`.
            Operand,
            /// A binary operator or the end of a term.
            Operator,
            /// The right side of a binary operator, which is a regular
            /// expression after `=~`.
            Right {
                regex: bool,
            },
        }
        let mut expect = Expect::Term;
        let mut parens = 0;
        loop {
            self.blanks();
            let i = self.i;
            let Some(c) = self.at(i) else { return };
            let next = self.at(i + 1);
            match c {
                b'\n' => {
                    self.i += 1;
                    self.here_documents(end);
                }
                b'#' => {
                    if self.comment() {
                        self.here_documents(end);
                    }
                }
                // Bash reads a regular expression as a word before it
                // looks for operators, and `(` and `|` don't end it.
                _ if expect == (Expect::Right { regex: true })
                    && (matches!(c, b'(' | b'|') || !is_break(c))
                    && self.literal_word(i) != Some("]]") =>
                {
                    self.parts(true);
                    expect = Expect::Operator;
                }
                b'(' => {
                    self.take(1, Some(Kind::Punctuation));
                    parens += 1;
                    expect = Expect::Term;
                }
                b')' if parens > 0 => {
                    self.take(1, Some(Kind::Punctuation));
                    parens -= 1;
                    expect = Expect::Operator;
                }
                b'&' | b'|' if next == Some(c) => {
                    self.take(2, Some(Kind::Operator));
                    expect = Expect::Term;
                }
                b'<' | b'>' if !matches!(next, Some(b'(' | b'<' | b'>' | b'&' | b'|')) => {
                    self.take(1, Some(Kind::Operator));
                    expect = Expect::Right { regex: false };
                }
                b'<' | b'>' if next == Some(b'(') => {
                    self.parts(false);
                    expect = Expect::Operator;
                }
                c if is_break(c) => return,
                _ => {
                    let w = self.literal_word(i);
                    if w == Some("]]") {
                        self.take(2, Some(Kind::Keyword));
                        return;
                    }
                    match expect {
                        Expect::Term if w == Some("!") => self.take(1, Some(Kind::Operator)),
                        Expect::Term if w.is_some_and(is_unary) => {
                            self.take(2, Some(Kind::Operator));
                            expect = Expect::Operand;
                        }
                        Expect::Operator if w == Some("=~") => {
                            self.take(2, Some(Kind::Operator));
                            expect = Expect::Right { regex: true };
                        }
                        Expect::Operator if w.is_some_and(is_binary) => {
                            self.take(w.map_or(0, str::len), Some(Kind::Operator));
                            expect = Expect::Right { regex: false };
                        }
                        Expect::Right { regex } => {
                            self.parts(regex);
                            expect = Expect::Operator;
                        }
                        _ => {
                            self.parts(false);
                            expect = Expect::Operator;
                        }
                    }
                }
            }
        }
    }
}

/// The length of the character at `i` in `b`, from its first byte.
fn char_len(b: &[u8], i: usize) -> usize {
    let n = match b[i] {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    };
    n.min(b.len() - i)
}

/// Where a here-document's body ends, as `heredoc_end` finds it.
struct BodyEnd {
    /// Where the line that ends the body starts.
    body_end: usize,
    /// Where the delimiter is on that line.
    delimiter: Range<usize>,
    /// Where to go on reading. That's the next line, or the rest of the
    /// delimiter's line when a `)` on it ends the command substitution.
    next: usize,
    rest_of_line: bool,
}

/// Find the line that ends the body of a here-document, which starts at
/// `body` in `b`. In a command substitution, `close` is the character
/// that ends it.
fn heredoc_end(b: &[u8], body: usize, doc: &Heredoc, close: Option<u8>) -> Option<BodyEnd> {
    let d = doc.delimiter.as_slice();
    let mut line = body;
    while line < b.len() {
        let end = b[line..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |n| line + n);
        let next = (end + 1).min(b.len());
        let text = &b[line..end];
        let found = |delimiter: Range<usize>| BodyEnd {
            body_end: line,
            delimiter,
            next,
            rest_of_line: false,
        };
        // Bash compares the line before it strips the tabs too.
        if doc.strip_tabs && text == d {
            return Some(found(line..end));
        }
        let tabs = if doc.strip_tabs {
            text.iter().take_while(|&&c| c == b'\t').count()
        } else {
            0
        };
        let t = &text[tabs..];
        if t == d {
            return Some(found(line + tabs..end));
        }
        if let Some(c) = close
            && t.starts_with(d)
            && t[d.len()..].contains(&c)
        {
            let s = line + tabs;
            return Some(BodyEnd {
                body_end: line,
                delimiter: s..s + d.len(),
                next: s + d.len(),
                rest_of_line: true,
            });
        }
        line = next;
    }
    None
}

/// `shell_break_chars` in `syntax.h`: the characters that end a word.
fn is_break(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b';' | b'&' | b'|' | b' ' | b'\t' | b'\n'
    )
}

/// The characters that start an extended pattern before a `(`.
fn is_pattern_char(c: u8) -> bool {
    matches!(c, b'@' | b'*' | b'+' | b'?' | b'!')
}

fn is_arith_operator(c: u8) -> bool {
    b"+-*/%<>=!&|^~?:,@".contains(&c)
}

/// The length of the variable name at the start of `b`, or 0.
fn name_len(b: &[u8]) -> usize {
    match b.first() {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => b
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count(),
        _ => 0,
    }
}

fn is_name(w: &str) -> bool {
    !w.is_empty() && name_len(w.as_bytes()) == w.len()
}

/// Whether a literal command name gets a color from its name. A name with
/// a pattern, braces or a tilde in it is expanded before it runs, and
/// `[` is better left plain like its `]`.
fn is_command_name(w: &str) -> bool {
    !w.bytes().any(|c| b"*?[]{}~=".contains(&c))
}

/// `test_unop` in `test.c`.
fn is_unary(w: &str) -> bool {
    matches!(w.as_bytes(), [b'-', c] if b"abcdefghknoprstuvwxzGLOSNR".contains(c))
}

/// `test_binop` in `test.c`, without `=~`.
fn is_binary(w: &str) -> bool {
    matches!(
        w,
        "=" | "=="
            | "!="
            | "<"
            | ">"
            | "-nt"
            | "-ot"
            | "-ef"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
    )
}

/// The length of the escape at the start of `b`, which starts with a
/// backslash, in `$'...'`, or 0 if it isn't one.
fn ansi_escape(b: &[u8]) -> usize {
    let hex = |b: &[u8], max: usize| {
        b.iter()
            .take(max)
            .take_while(|c| c.is_ascii_hexdigit())
            .count()
    };
    match b.get(1) {
        Some(
            b'a' | b'b' | b'e' | b'E' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' | b'\'' | b'"'
            | b'?',
        ) => 2,
        Some(b'0'..=b'7') => {
            2 + b[2..]
                .iter()
                .take(2)
                .take_while(|c| (b'0'..=b'7').contains(c))
                .count()
        }
        Some(b'x') if b.get(2) == Some(&b'{') => {
            let n = 3 + hex(&b[3..], usize::MAX);
            n + usize::from(b.get(n) == Some(&b'}'))
        }
        Some(b'x') => match hex(&b[2..], 2) {
            0 => 0,
            n => 2 + n,
        },
        Some(b'u') => match hex(&b[2..], 4) {
            0 => 0,
            n => 2 + n,
        },
        Some(b'U') => match hex(&b[2..], 8) {
            0 => 0,
            n => 2 + n,
        },
        // `\c\\` takes both backslashes.
        Some(b'c') => match &b[2..] {
            [] => 0,
            [b'\\', b'\\', ..] => 4,
            _ => 3,
        },
        _ => 0,
    }
}

/// The delimiter of a here-document after quote removal, and whether any
/// of it was quoted.
fn quote_removal(word: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut i = 0;
    while i < word.len() {
        match word[i] {
            // Bash removes a line continuation before it reads the word,
            // so it doesn't count as a quote.
            b'\\' if word.get(i + 1) == Some(&b'\n') => i += 2,
            b'\\' => {
                quoted = true;
                out.extend(word.get(i + 1));
                i += 2;
            }
            b'\'' => {
                quoted = true;
                let end = word[i + 1..]
                    .iter()
                    .position(|&c| c == b'\'')
                    .map_or(word.len(), |n| i + 1 + n);
                out.extend_from_slice(&word[i + 1..end]);
                i = end + 1;
            }
            b'"' => {
                quoted = true;
                i += 1;
                while i < word.len() && word[i] != b'"' {
                    match (word[i], word.get(i + 1)) {
                        (b'\\', Some(b'\n')) => i += 2,
                        (b'\\', Some(&c @ (b'$' | b'`' | b'"' | b'\\'))) => {
                            out.push(c);
                            i += 2;
                        }
                        (c, _) => {
                            out.push(c);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            // `$'...'` and `$"..."` are quotes too.
            b'$' if matches!(word.get(i + 1), Some(b'\'' | b'"')) => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, quoted)
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
        let text = r#"#!/bin/sh
# Copy the files.
for f in "$@"; do
  if [ -f "$f" ]; then cp "$f" "${dest:-/tmp}/" 2>/dev/null || exit 1; fi
done
"#;
        assert_eq!(
            tokens(text),
            [
                ("#!/bin/sh", Comment),
                ("# Copy the files.", Comment),
                ("for", Keyword),
                ("in", Keyword),
                ("\"", String),
                ("$", Punctuation),
                ("\"", String),
                (";", Punctuation),
                ("do", Keyword),
                ("if", Keyword),
                ("\"", String),
                ("$", Punctuation),
                ("\"", String),
                (";", Punctuation),
                ("then", Keyword),
                ("\"", String),
                ("$", Punctuation),
                ("\"", String),
                ("\"", String),
                ("${", Punctuation),
                (":-", Operator),
                ("/tmp", String),
                ("}", Punctuation),
                ("/\"", String),
                ("2", Number),
                (">", Operator),
                ("||", Operator),
                (";", Punctuation),
                ("fi", Keyword),
                ("done", Keyword),
            ]
        );
        assert_eq!(
            identifiers(text),
            ["f", "@", "f", "cp", "f", "dest", "exit"]
        );
    }

    #[test]
    fn reserved_words() {
        // A reserved word is only one where a command can start.
        assert_eq!(
            kinds_of("if a; then echo if then; fi", Kind::Keyword),
            ["if", "then", "fi"]
        );
        // Bash removes a line continuation before it reads words, so a
        // reserved word can go on over one, and one that goes on into
        // another word isn't one any more.
        assert_eq!(
            kinds_of("i\\\nf a; then\\\nx; fi", Kind::Keyword),
            ["i", "f", "fi"]
        );
        // Not after an assignment, and not when any of it is quoted.
        assert_eq!(
            kinds_of("a=1 if; \"if\"; i\\f", Kind::Keyword),
            [] as [&str; 0]
        );
        // `in` and `do` follow the word after `for`, even on the next line.
        assert_eq!(
            kinds_of(
                "for in in do\ndo echo done; done\nfor x\nin a; do :; done",
                Kind::Keyword
            ),
            ["for", "in", "do", "done", "for", "in", "do", "done"]
        );
        assert_eq!(
            kinds_of("for x do :; done; select y do :; done", Kind::Keyword),
            ["for", "do", "done", "select", "do", "done"]
        );
        // `time` only starts a pipeline.
        assert_eq!(
            kinds_of("time a | time b; ! time -p c", Kind::Keyword),
            ["time", "time"]
        );
        assert_eq!(kinds_of("! a; a !", Kind::Operator), ["!"]);
        // Braces group commands where a command starts, and are plain
        // words elsewhere.
        assert_eq!(
            tokens("{ echo {a,b} }; }"),
            [
                ("{", Kind::Punctuation),
                (";", Kind::Punctuation),
                ("}", Kind::Punctuation)
            ]
        );
        assert_eq!(
            kinds_of(
                "while :; do coproc c { :; }; done; until x; do :; done",
                Kind::Keyword
            ),
            ["while", "do", "coproc", "done", "until", "do", "done"]
        );
    }

    #[test]
    fn comments() {
        use Kind::*;
        // A `#` starts a comment only at the start of a word.
        assert_eq!(
            tokens("echo a#b #c\necho $# ${#x};# d  \n"),
            [
                ("#c", Comment),
                ("$", Punctuation),
                ("${", Punctuation),
                ("#", Operator),
                ("};", Punctuation),
                ("# d  ", Comment),
            ]
        );
        assert_eq!(identifiers("echo $# ${#x}"), ["echo", "#", "x"]);
    }

    #[test]
    fn quotes_and_escapes() {
        use Kind::*;
        assert_eq!(
            tokens(r#"a 'b\n"' "c\"\a\$" \x\é $"d" e"#),
            [
                (r#"'b\n"'"#, String),
                ("\"c", String),
                ("\\\"", Escape),
                ("\\a", String),
                ("\\$", Escape),
                ("\"", String),
                ("\\x\\é", Escape),
                ("$\"d\"", String),
            ]
        );
        // A backslash before a line break joins the lines, but not in
        // single quotes.
        assert_eq!(
            tokens("a \\\n b \"c\\\nd\" 'e\\\nf'"),
            [
                ("\\\n", Escape),
                ("\"c", String),
                ("\\\n", Escape),
                ("d\"", String),
                ("'e\\\nf'", String),
            ]
        );
        // Quotes that don't close go on to the end.
        assert_eq!(tokens("a 'b\nc"), [("'b\nc", String)]);
    }

    #[test]
    fn line_continuations() {
        use Kind::*;
        // Bash removes them before it reads operators, so `>` and `&` on
        // the next line are `>&`, and the word goes on after one. An
        // escaped backslash doesn't start one.
        assert_eq!(
            tokens("a >\\\n&2 b\\\n#c \\\\\n#d"),
            [
                (">", Operator),
                ("\\\n", Escape),
                ("&", Operator),
                ("2", Number),
                ("\\\n", Escape),
                ("\\\\", Escape),
                ("#d", Comment),
            ]
        );
        assert_eq!(identifiers("x\\\n=1 a$\\\nb"), ["x", "b"]);
        // But not in a comment, which ends at the line break.
        assert_eq!(
            tokens("# a \\\nif b; then :; fi"),
            [
                ("# a \\", Comment),
                ("if", Keyword),
                (";", Punctuation),
                ("then", Keyword),
                (";", Punctuation),
                ("fi", Keyword),
            ]
        );
        // And not in `$'...'`.
        assert_eq!(
            tokens("a $\\\n'b\\\nc' d"),
            [("$", String), ("\\\n", Escape), ("'b\\\nc'", String),]
        );
        // Bash reads the second `)` of `((` without removing one, so this
        // is a subshell in a subshell, where `b` is an argument.
        assert_eq!(identifiers("((a b)\\\n)"), ["a"]);
        assert_eq!(identifiers("((a\nb))"), ["a", "b"]);
    }

    #[test]
    fn ansi_c_strings() {
        use Kind::*;
        assert_eq!(
            tokens(r"a $'\n\'\x41\x{263a}\u00e9\101\cA\c\\\q\x'"),
            [
                ("$'", String),
                (r"\n\'\x41\x{263a}\u00e9\101\cA\c\\", Escape),
                (r"\q\x'", String),
            ]
        );
        // `$'` is plain inside double quotes.
        assert_eq!(tokens(r#"a "$'\n'""#), [(r#""$'\n'""#, String)]);
        // `\c` takes only one byte, but the color covers the character.
        assert_eq!(
            tokens("a $'\\cé'"),
            [("$'", String), ("\\cé", Escape), ("'", String)]
        );
    }

    #[test]
    fn parameters() {
        use Kind::*;
        assert_eq!(
            tokens("a $x$1$10 $@ $_y $% $"),
            [
                ("$", Punctuation),
                ("$", Punctuation),
                ("$", Punctuation),
                ("$", Punctuation),
                ("$", Punctuation)
            ]
        );
        assert_eq!(
            identifiers("a $x$1$10 $@ $_y $% $"),
            ["a", "x", "1", "1", "@", "_y"]
        );
        assert_eq!(
            tokens("a ${x/a b/c} ${x//\\}/d} ${!p*} ${x:1:-2} ${10}"),
            [
                ("${", Punctuation),
                ("/", Operator),
                ("/", Operator),
                ("}", Punctuation),
                ("${", Punctuation),
                ("//", Operator),
                ("\\}", Escape),
                ("/", Operator),
                ("}", Punctuation),
                ("${", Punctuation),
                ("!", Operator),
                ("}", Punctuation),
                ("${", Punctuation),
                (":", Operator),
                ("1", Number),
                (":-", Operator),
                ("2", Number),
                ("}", Punctuation),
                ("${", Punctuation),
                ("}", Punctuation),
            ]
        );
        // The first `}` ends the expansion, unless it's quoted or in
        // another expansion.
        assert_eq!(
            tokens("a ${x:-{b}} ${x:-'}'} ${x:-${y}}"),
            [
                ("${", Punctuation),
                (":-", Operator),
                ("}", Punctuation),
                ("${", Punctuation),
                (":-", Operator),
                ("'}'", String),
                ("}", Punctuation),
                ("${", Punctuation),
                (":-", Operator),
                ("${", Punctuation),
                ("}}", Punctuation),
            ]
        );
        // `$'...'` is read inside `${...}` even in double quotes.
        assert_eq!(
            tokens(r#""${v/$'\''/x}""#),
            [
                ("\"", String),
                ("${", Punctuation),
                ("/", Operator),
                ("$'", String),
                (r"\'", Escape),
                ("'", String),
                ("/", Operator),
                ("x", String),
                ("}", Punctuation),
                ("\"", String),
            ]
        );
        // In double quotes, the word is part of the string.
        assert_eq!(
            tokens("\"${a[i+1]#x}\""),
            [
                ("\"", String),
                ("${", Punctuation),
                ("[", Punctuation),
                ("+", Operator),
                ("1", Number),
                ("]", Punctuation),
                ("#", Operator),
                ("x", String),
                ("}", Punctuation),
                ("\"", String),
            ]
        );
    }

    #[test]
    fn command_substitutions() {
        use Kind::*;
        // What's inside is lexed as commands of its own.
        assert_eq!(
            tokens("a $(if b; then c; fi) \"$(d \"e\")\""),
            [
                ("$(", Punctuation),
                ("if", Keyword),
                (";", Punctuation),
                ("then", Keyword),
                (";", Punctuation),
                ("fi", Keyword),
                (")", Punctuation),
                ("\"", String),
                ("$(", Punctuation),
                ("\"e\"", String),
                (")", Punctuation),
                ("\"", String),
            ]
        );
        // A `)` in a case pattern or a subshell doesn't end it.
        assert_eq!(
            kinds_of("a $(case x in y) (z) ;; esac) b", Kind::Punctuation),
            ["$(", ")", "(", ")", ";;", ")"]
        );
        // Backquotes end at the next backquote that isn't escaped.
        assert_eq!(
            tokens("a `b \\` \"c`\" d"),
            [
                ("`", Punctuation),
                ("\\`", Escape),
                ("\"c", String),
                ("`", Punctuation),
                ("\" d", String)
            ]
        );
        // `${ ...; }` runs commands too.
        assert_eq!(
            tokens("a ${ b; }c ${|d;}"),
            [
                ("${", Punctuation),
                (";", Punctuation),
                ("}", Punctuation),
                ("${|", Punctuation),
                (";}", Punctuation)
            ]
        );
        assert_eq!(identifiers("a ${ b; }c"), ["a", "b"]);
        // `<((` starts arithmetic, as `$((` does.
        assert_eq!(kinds_of("a <((b + 1))", Kind::Operator), ["+"]);
        // Process substitutions can be in the middle of a word.
        assert_eq!(
            tokens("diff <(a) x>(b)"),
            [
                ("<(", Punctuation),
                (")", Punctuation),
                (">(", Punctuation),
                (")", Punctuation)
            ]
        );
    }

    #[test]
    fn arithmetic() {
        use Kind::*;
        assert_eq!(
            tokens("((i++, j = 0x1F)); x=$[2**3]"),
            [
                ("((", Punctuation),
                ("++,", Operator),
                ("=", Operator),
                ("0x1F", Number),
                ("));", Punctuation),
                ("=", Operator),
                ("$[", Punctuation),
                ("2", Number),
                ("**", Operator),
                ("3", Number),
                ("]", Punctuation),
            ]
        );
        assert_eq!(
            identifiers("((i++ + $j)); echo $((k))"),
            ["i", "j", "echo", "k"]
        );
        assert_eq!(
            kinds_of("for ((i = 0; i < 2; i++)) do :; done", Kind::Keyword),
            ["for", "do", "done"]
        );
        // `((` starts a subshell in a subshell when the inner `)` doesn't
        // have another after it.
        assert_eq!(
            tokens("((a) | b)"),
            [
                ("((", Punctuation),
                (")", Punctuation),
                ("|", Operator),
                (")", Punctuation)
            ]
        );
        assert_eq!(identifiers("((a) | b)"), ["a", "b"]);
        // `$((` is always arithmetic.
        assert_eq!(kinds_of("$((a) + (b))", Kind::Operator), ["+"]);
    }

    #[test]
    fn here_documents() {
        use Kind::*;
        let text = "cat <<EOF; cat <<'E'\"x\"\na $b \\$c \"\nEOF\n$d\nEx\ne\n";
        assert_eq!(
            tokens(text),
            [
                ("<<", Operator),
                ("EOF", Attribute),
                (";", Punctuation),
                ("<<", Operator),
                ("'E'\"x\"", Attribute),
                ("a ", String),
                ("$", Punctuation),
                (" ", String),
                ("\\$", Escape),
                ("c \"\n", String),
                ("EOF", Attribute),
                ("$d\n", String),
                ("Ex", Attribute),
            ]
        );
        assert_eq!(identifiers(text), ["cat", "cat", "b", "e"]);
        // `<<-` strips tabs. A line continuation joins lines before they
        // are compared with the delimiter, but only when it's unquoted.
        assert_eq!(
            tokens("a <<-E\n\tx\\\nE\n\tE\nb <<\\E\nx\\\nE\n"),
            [
                ("<<-", Operator),
                ("E", Attribute),
                ("\tx", String),
                ("\\\n", Escape),
                ("E\n", String),
                ("E", Attribute),
                ("<<", Operator),
                ("\\E", Attribute),
                ("x\\\n", String),
                ("E", Attribute),
            ]
        );
        // A line continuation joins the lines of a body before bash
        // compares them with the delimiter, so `E\` is the end of the body
        // when the next line is empty, but not when it's `x`.
        assert_eq!(
            tokens("a <<E\nE\\\n\nb <<E\nE\\\nx\n"),
            [
                ("<<", Operator),
                ("E", Attribute),
                ("E", Attribute),
                ("\\\n", Escape),
                ("<<", Operator),
                ("E", Attribute),
                ("E", String),
                ("\\\n", Escape),
                ("x\n", String),
            ]
        );
        // In single quotes, the delimiter keeps its line break, so no line
        // can end the body.
        assert_eq!(
            tokens("a <<'E\\\nF'\nEF\n"),
            [("<<", Operator), ("'E\\\nF'", Attribute), ("EF\n", String),]
        );
        // A here-document that never ends goes on to the end.
        assert_eq!(
            tokens("a <<E\nb"),
            [("<<", Operator), ("E", Attribute), ("b", String)]
        );
        // In a command substitution, a `)` after the delimiter ends both.
        assert_eq!(
            tokens("a $(b <<E\nc\nE)"),
            [
                ("$(", Punctuation),
                ("<<", Operator),
                ("E", Attribute),
                ("c\n", String),
                ("E", Attribute),
                (")", Punctuation),
            ]
        );
        // The line can go on over a line continuation.
        assert_eq!(
            tokens("a $(b <<E\nE\\\n)"),
            [
                ("$(", Punctuation),
                ("<<", Operator),
                ("E", Attribute),
                ("E", Attribute),
                ("\\\n", Escape),
                (")", Punctuation),
            ]
        );
        // One still open at the `)` starts on the next line, before those
        // outside the command substitution.
        assert_eq!(
            tokens("a <<A $(b <<B) c\nb\nB\na\nA\n"),
            [
                ("<<", Operator),
                ("A", Attribute),
                ("$(", Punctuation),
                ("<<", Operator),
                ("B", Attribute),
                (")", Punctuation),
                ("b\n", String),
                ("B", Attribute),
                ("a\n", String),
                ("A", Attribute),
            ]
        );
    }

    #[test]
    fn assignments() {
        use Kind::*;
        assert_eq!(
            tokens("a=1 b+=x c[i+1]=y d=(1 [k]=v) e"),
            [
                ("=", Operator),
                ("+=", Operator),
                ("[", Punctuation),
                ("+", Operator),
                ("1", Number),
                ("]", Punctuation),
                ("=", Operator),
                ("=", Operator),
                ("(", Punctuation),
                ("[", Punctuation),
                ("]", Punctuation),
                ("=", Operator),
                (")", Punctuation),
            ]
        );
        assert_eq!(
            identifiers("a=1 b+=x c[i+1]=y d=(1 [k]=v) e"),
            ["a", "b", "c", "i", "d", "k", "e"]
        );
        // Only before the command name, or after builtins that take
        // assignments. Those that declare variables take names too.
        assert_eq!(identifiers("echo a=1"), ["echo"]);
        assert_eq!(
            identifiers("local -r a=1 b; alias c=d e; export -f"),
            ["local", "a", "b", "alias", "c", "export"]
        );
        // Where a command starts, brackets after a name belong to the word
        // even without an `=`, so `a[1 2]` is the command name. They can
        // go on to the next line.
        assert_eq!(identifiers("a[1 2] b; c"), ["c"]);
        assert_eq!(identifiers("a[1\n]=2 b; c"), ["a", "b", "c"]);
        assert_eq!(identifiers("coproc a b=1"), ["a", "b"]);
    }

    #[test]
    fn redirections() {
        use Kind::*;
        assert_eq!(
            tokens("a 2>&1 >&- 3<>f &>>g {fd}>h 4 >i"),
            [
                ("2", Number),
                (">&", Operator),
                ("1", Number),
                (">&-", Operator),
                ("3", Number),
                ("<>", Operator),
                ("&>>", Operator),
                ("{", Punctuation),
                ("}", Punctuation),
                (">", Operator),
                (">", Operator),
            ]
        );
        assert_eq!(identifiers("a {fd}>h"), ["a", "fd"]);
        // A redirection can come before the command name.
        assert_eq!(identifiers(">f a=1 b c"), ["a", "b"]);
    }

    #[test]
    fn case_patterns() {
        use Kind::*;
        // `esac` ends the patterns, except after `|` or `(`.
        let text = "case $x in\n  a|b) c ;;\n  (esac|@(d)) e ;& *) ;;& esac";
        assert_eq!(
            tokens(text),
            [
                ("case", Keyword),
                ("$", Punctuation),
                ("in", Keyword),
                ("|", Operator),
                (")", Punctuation),
                (";;", Punctuation),
                ("(", Punctuation),
                ("|", Operator),
                (")", Punctuation),
                (";&", Punctuation),
                (")", Punctuation),
                (";;&", Punctuation),
                ("esac", Keyword),
            ]
        );
        assert_eq!(identifiers(text), ["x", "c", "e"]);
        assert_eq!(
            kinds_of("case a in esac", Kind::Keyword),
            ["case", "in", "esac"]
        );
    }

    #[test]
    fn conditionals() {
        use Kind::*;
        assert_eq!(
            tokens("[[ ! -f $a && (b == c* || d < e) ]] && [[ $f =~ ^(x|y)$ ]]"),
            [
                ("[[", Keyword),
                ("!", Operator),
                ("-f", Operator),
                ("$", Punctuation),
                ("&&", Operator),
                ("(", Punctuation),
                ("==", Operator),
                ("||", Operator),
                ("<", Operator),
                (")", Punctuation),
                ("]]", Keyword),
                ("&&", Operator),
                ("[[", Keyword),
                ("$", Punctuation),
                ("=~", Operator),
                ("]]", Keyword),
            ]
        );
        // A regular expression can start with `(`.
        assert_eq!(
            tokens("[[ a =~ (b|c)d ]]"),
            [("[[", Keyword), ("=~", Operator), ("]]", Keyword)]
        );
        // Operators only count where the parser expects one.
        // `==` is the operand of `-f` here.
        assert_eq!(
            kinds_of("[[ -f == ]] && [[ x == -f ]]", Kind::Operator),
            ["-f", "&&", "=="]
        );
        // `]]` outside of one is a word.
        assert_eq!(kinds_of("echo ]]", Kind::Keyword), [] as [&str; 0]);
    }

    #[test]
    fn extended_patterns() {
        use Kind::*;
        // The parentheses of a pattern belong to the word.
        assert_eq!(tokens("ls !(*.txt|\"a b\") @(x)y"), [("\"a b\"", String)]);
        // A `$` before one is just a `$`.
        assert_eq!(tokens("echo $@(a|b) $@"), [("$", Punctuation)]);
        // Only quotes nest in them, so the `)` in `${x:-)}` ends one.
        assert_eq!(
            tokens("ls @(${x:-)|y)"),
            [("|", Operator), (")", Punctuation)]
        );
        assert_eq!(identifiers("ls @(${x:-)|y)"), ["ls", "y"]);
        // Where a command starts, `!(` is `!` before a subshell.
        assert_eq!(
            tokens("!(a)"),
            [("!", Operator), ("(", Punctuation), (")", Punctuation)]
        );
    }

    #[test]
    fn functions() {
        use Kind::*;
        assert_eq!(
            tokens("f() { :; }\nfunction g { :; }"),
            [
                ("()", Punctuation),
                ("{", Punctuation),
                (";", Punctuation),
                ("}", Punctuation),
                ("function", Keyword),
                ("{", Punctuation),
                (";", Punctuation),
                ("}", Punctuation),
            ]
        );
        assert_eq!(
            identifiers("f() { :; }\nfunction g-h { :; }"),
            ["f", ":", "g-h", ":"]
        );
    }

    #[test]
    fn command_names() {
        // Names with patterns, braces or tildes in them are plain, and so
        // is `[`.
        assert_eq!(
            identifiers("./run a; *.sh; ~/x; [ -f y ]; {c,d}"),
            ["./run"]
        );
    }

    #[test]
    fn deep_nesting() {
        // Expansions nested too deeply are plain, so that the lexer
        // doesn't run out of stack.
        for open in ["$(", "\"$(", "`", "${x:-", "$((", "<(", "@("] {
            let text = open.repeat(100_000);
            let mut out = Tokens::default();
            lex(&text, &mut out);
        }
    }
}
