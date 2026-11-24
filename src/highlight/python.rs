//! A lexer for Python.
//!
//! It follows CPython's tokenizer, `Parser/lexer/lexer.c`, to find where
//! tokens start and end, but it doesn't check that they're valid. Since
//! Python 3.12, the fields of an f-string hold code that the tokenizer
//! splits into tokens like any other code. So the lexer keeps a stack of
//! the brackets and fields it's inside of, the way the tokenizer does.
//!
//! Without a parser we can't know what an identifier names, so every
//! identifier gets a color from its name (see `Kind::Identifier`). Soft
//! keywords such as `match` are only keywords in some places, and the
//! lexer guesses those from the tokens around them in the same statement.

use std::ops::Range;

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\f]+")]
#[logos(subpattern digits = r"[0-9](_?[0-9])*")]
#[logos(subpattern exponent = r"[eE][+-]?(?&digits)")]
enum Token {
    #[regex(r"\r\n|\n|\r")]
    Newline,

    // A backslash at the end of a line joins the next line to it.
    #[regex(r"\\(\r\n|\n|\r)")]
    Continuation,

    #[regex(r"#[^\r\n]*", allow_greedy = true)]
    Comment,

    // Only the prefix and the opening quotes. `lex` finds the rest.
    #[regex(r#"([rRuUbB]|[bB][rR]|[rR][bB])?("|'|"""|''')"#)]
    Str,

    // The start of an f-string, or of a t-string, which is written the
    // same way.
    #[regex(r#"([fFtT]|[fFtT][rR]|[rR][fFtT])("|'|"""|''')"#)]
    Template,

    // An exponent or a `j` needs the digits before it, so `1else` is
    // `1` and `else`.
    #[regex(r"(?&digits)(?&exponent)?[jJ]?")]
    #[regex(r"((?&digits)\.(?&digits)?|\.(?&digits))(?&exponent)?[jJ]?")]
    #[regex(r"0[xX](_?[0-9a-fA-F])+")]
    #[regex(r"0[oO](_?[0-7])+")]
    #[regex(r"0[bB](_?[01])+")]
    Number,

    #[regex(r"[\p{XID_Start}_]\p{XID_Continue}*")]
    Word,

    #[token("...")]
    Ellipsis,

    #[regex(r"[-+*/%&|^@<>=]=?|//=?|\*\*=?|<<=?|>>=?|!=|:=|->|[~!]")]
    Operator,

    #[regex(r"[(\[{]")]
    Open,

    #[regex(r"[)\]}]")]
    Close,

    #[token(":")]
    Colon,

    #[token(".")]
    Dot,

    #[regex(r"[,;]")]
    Punctuation,
}

/// The keywords that are keywords everywhere, except the constants.
const KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

/// The keywords that are constants.
const LITERALS: &[&str] = &["True", "False", "None"];

/// Built-in names that Sublime Text colors as constants too. Nobody
/// assigns to them, and `__debug__` can't be assigned to.
const CONSTANTS: &[&str] = &["NotImplemented", "Ellipsis", "__debug__"];

/// How an f-string or a t-string is quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Quote {
    quote: u8,
    triple: bool,
    raw: bool,
}

impl Quote {
    fn new(start: &str) -> Quote {
        let quotes = start.len() - start.trim_end_matches(['"', '\'']).len();
        Quote {
            quote: start.as_bytes()[start.len() - 1],
            triple: quotes == 3,
            raw: start.contains(['r', 'R']),
        }
    }

    /// The length of the closing quotes at the start of `rest`, if they
    /// are there.
    fn closes(self, rest: &[u8]) -> Option<usize> {
        let n = if self.triple { 3 } else { 1 };
        (rest.len() >= n && rest[..n].iter().all(|&b| b == self.quote)).then_some(n)
    }
}

/// What the lexer is inside of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// A bracket in code, with its opening character.
    Bracket(u8),
    /// The code in a field of an f-string, such as the `x` in `f"{x}"`.
    Field(Quote),
    /// The format spec after the `:` in a field, such as the `>3` in
    /// `f"{x:>3}"`. It's text, but a `{` in it starts another field.
    Spec(Quote),
}

/// Where the text of an f-string stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// At a `{` that starts a field.
    Field,
    /// At the `}` that ends the field that a format spec is in.
    FieldEnd,
    /// After the closing quotes, or at the end of the line or the text
    /// when the string isn't closed.
    Close,
}

/// A token, before we know what color its words get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    /// The end of a logical line, where a statement ends. Nothing gets
    /// colored here, but soft keywords only look at the tokens up to it.
    LineEnd,
    Word,
    Open,
    Close,
    Colon,
    Dot,
    Colored(Kind),
}

struct Tok {
    r: Range<usize>,
    piece: Piece,
    /// Whether the token starts a logical line, where a statement starts.
    line_start: bool,
}

struct State<'a> {
    text: &'a str,
    toks: Vec<Tok>,
    stack: Vec<Frame>,
    line_start: bool,
}

/// The pieces of a string literal. Anything else in one, such as a
/// backslash before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    // These only work in strings, not in bytes.
    #[regex(r"\\(N\{[A-Za-z0-9 -]+\}|u[0-9a-fA-F]{4}|U[0-9a-fA-F]{8})")]
    Unicode,
    #[regex(r#"\\(\r\n|[\n\r\\'"abfnrtv]|[0-7]{1,3}|x[0-9a-fA-F]{2})"#)]
    Escape,
    // A doubled brace in the text of an f-string stands for one brace.
    #[token("{{")]
    #[token("}}")]
    Brace,
    #[regex(r"[^\\{}]+", allow_greedy = true)]
    Text,
}

/// The length of the rest of a string literal that isn't an f-string,
/// up to and including its closing quotes. A string without them runs to
/// the end of its line, or of the text if it's triple-quoted.
fn string_len(rest: &str, quote: Quote) -> usize {
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += escaped_len(&rest[i + 1..]) + 1,
            b'\n' | b'\r' if !quote.triple => return i,
            _ => match quote.closes(&b[i..]) {
                Some(n) => return i + n,
                None => i += 1,
            },
        }
    }
    b.len()
}

/// The length of the character after a backslash, which the backslash
/// keeps from ending the string, even in a raw string. A line break that
/// is CR LF counts as one.
fn escaped_len(rest: &str) -> usize {
    if rest.starts_with("\r\n") {
        2
    } else {
        rest.chars().next().map_or(0, char::len_utf8)
    }
}

/// Find where the text of an f-string stops, starting at `start` in its
/// plain text, or in a format spec if `spec` is set.
fn template_text(text: &str, start: usize, quote: Quote, spec: bool) -> (usize, Stop) {
    let b = text.as_bytes();
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let rest = &text[i + 1..];
                if !quote.raw
                    && let Some(name) = rest.strip_prefix("N{")
                    && let Some(n) = name
                        .find(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | ' ' | '-'))
                    && name.as_bytes()[n] == b'}'
                {
                    // A character named in braces, as in `\N{DASH}`.
                    i += 4 + n;
                } else if rest.starts_with(['{', '}']) {
                    // A backslash can't escape a brace. The tokenizer
                    // looks at the brace on its own.
                    i += 1;
                } else {
                    i += 1 + escaped_len(rest);
                }
            }
            // A doubled brace stands for one, but not in a format spec.
            b'{' if !spec && b.get(i + 1) == Some(&b'{') => i += 2,
            b'{' => return (i, Stop::Field),
            b'}' if spec => return (i, Stop::FieldEnd),
            b'}' if b.get(i + 1) == Some(&b'}') => i += 2,
            b'\n' | b'\r' if !quote.triple => return (i, Stop::Close),
            _ => match quote.closes(&b[i..]) {
                Some(n) => return (i + n, Stop::Close),
                None => i += 1,
            },
        }
    }
    (b.len(), Stop::Close)
}

impl State<'_> {
    fn push(&mut self, r: Range<usize>, piece: Piece) {
        let line_start = !matches!(piece, Piece::LineEnd | Piece::Colored(Kind::Comment))
            && std::mem::take(&mut self.line_start);
        self.toks.push(Tok {
            r,
            piece,
            line_start,
        });
    }

    /// Color a string literal, or a piece of the text of an f-string,
    /// with its escapes.
    fn string(&mut self, r: Range<usize>, raw: bool, bytes: bool, template: bool) {
        for (part, p) in Part::lexer(&self.text[r.clone()]).spanned() {
            let kind = match part {
                Ok(Part::Escape) if !raw => Kind::Escape,
                Ok(Part::Unicode) if !raw && !bytes => Kind::Escape,
                Ok(Part::Brace) if template => Kind::Escape,
                _ => Kind::String,
            };
            self.push(r.start + p.start..r.start + p.end, Piece::Colored(kind));
        }
    }

    /// Read the text of an f-string from `start`, which is in its plain
    /// text, or in a format spec if `spec` is set. Stop where code starts
    /// again, in a field or after the string.
    fn template(&mut self, lex: &mut Lexer<Token>, start: usize, quote: Quote, spec: bool) {
        let (mut start, mut spec) = (start, spec);
        loop {
            let (end, stop) = template_text(self.text, start, quote, spec);
            self.string(start..end, quote.raw, false, true);
            lex.bump(end - lex.span().end);
            match stop {
                Stop::Field => {
                    self.push(end..end + 1, Piece::Colored(Kind::Punctuation));
                    lex.bump(1);
                    self.stack.push(Frame::Field(quote));
                    return;
                }
                // The field may have been in the format spec of another.
                Stop::FieldEnd => {
                    self.stack.pop();
                    self.push(end..end + 1, Piece::Colored(Kind::Punctuation));
                    lex.bump(1);
                    start = end + 1;
                    spec = self.stack.last() == Some(&Frame::Spec(quote));
                }
                // A string that ends inside a format spec is an error.
                // Leave all of its fields.
                Stop::Close => {
                    while spec
                        && let Some(Frame::Spec(q) | Frame::Field(q)) = self.stack.last()
                        && *q == quote
                    {
                        self.stack.pop();
                    }
                    return;
                }
            }
        }
    }

    /// Go back to the text of an f-string after one of its fields, which
    /// ended just before `start`.
    fn after_field(&mut self, lex: &mut Lexer<Token>, start: usize, quote: Quote) {
        let spec = self.stack.last() == Some(&Frame::Spec(quote));
        self.template(lex, start, quote, spec);
    }

    /// The field of an f-string whose code we're in, unless we're inside
    /// brackets in that code.
    fn field(&self) -> Option<Quote> {
        match self.stack.last() {
            Some(&Frame::Field(q)) => Some(q),
            _ => None,
        }
    }

    /// Pop the innermost frame that the bracket `close` closes, along with
    /// any unclosed brackets inside it. A bracket that doesn't match
    /// anything changes nothing, since a hunk can start in the middle of
    /// some brackets. Only a `}` can close a field.
    fn close(&mut self, close: u8) -> Option<Frame> {
        let open = match close {
            b')' => b'(',
            b']' => b'[',
            _ => b'{',
        };
        for i in (0..self.stack.len()).rev() {
            match self.stack[i] {
                Frame::Bracket(b) if b != open => {}
                Frame::Field(_) if close != b'}' => return None,
                Frame::Spec(_) => return None,
                frame => {
                    self.stack.truncate(i);
                    return Some(frame);
                }
            }
        }
        None
    }

    /// Start a format spec at `start`, just after the `:` that ends the
    /// code in the field at the top of the stack.
    fn spec(&mut self, lex: &mut Lexer<Token>, start: usize, quote: Quote) {
        self.stack.pop();
        self.stack.push(Frame::Spec(quote));
        self.push(start - 1..start, Piece::Colored(Kind::Punctuation));
        self.template(lex, start, quote, true);
    }
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut state = State {
        text,
        toks: Vec::new(),
        stack: Vec::new(),
        line_start: true,
    };
    let mut lex = Token::lexer(text);
    while let Some(token) = lex.next() {
        let span = lex.span();
        let slice = lex.slice();
        let piece = match token {
            // A line break inside brackets doesn't end the statement.
            Ok(Token::Newline) => {
                if state.stack.is_empty() {
                    state.push(span, Piece::LineEnd);
                    state.line_start = true;
                }
                continue;
            }
            Ok(Token::Continuation) => continue,
            Ok(Token::Comment) => Piece::Colored(Kind::Comment),
            Ok(Token::Str) => {
                let quote = Quote::new(slice);
                lex.bump(string_len(lex.remainder(), quote));
                let bytes = slice.contains(['b', 'B']);
                state.string(span.start..lex.span().end, quote.raw, bytes, false);
                continue;
            }
            Ok(Token::Template) => {
                state.push(span.clone(), Piece::Colored(Kind::String));
                state.template(&mut lex, span.end, Quote::new(slice), false);
                continue;
            }
            Ok(Token::Number) => Piece::Colored(Kind::Number),
            Ok(Token::Word) => Piece::Word,
            Ok(Token::Ellipsis) => Piece::Colored(Kind::Constant),
            Ok(Token::Colon) => {
                if let Some(quote) = state.field() {
                    state.spec(&mut lex, span.end, quote);
                    continue;
                }
                Piece::Colon
            }
            Ok(Token::Operator) => match (slice, state.field()) {
                // In a field, `:=` is a `:` that starts a format spec
                // with `=` in it.
                (":=", Some(quote)) => {
                    state.spec(&mut lex, span.start + 1, quote);
                    continue;
                }
                // A conversion, as in `f"{x!r}"`.
                ("!", Some(_)) => {
                    let rest = lex.remainder();
                    let n = rest
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .unwrap_or(rest.len());
                    lex.bump(n);
                    Piece::Colored(Kind::Operator)
                }
                _ => Piece::Colored(Kind::Operator),
            },
            Ok(Token::Open) => {
                state.stack.push(Frame::Bracket(slice.as_bytes()[0]));
                Piece::Open
            }
            Ok(Token::Close) => match state.close(slice.as_bytes()[0]) {
                Some(Frame::Field(quote)) => {
                    state.push(span.clone(), Piece::Colored(Kind::Punctuation));
                    state.after_field(&mut lex, span.end, quote);
                    continue;
                }
                _ => Piece::Close,
            },
            Ok(Token::Dot) => Piece::Dot,
            Ok(Token::Punctuation) => Piece::Colored(Kind::Punctuation),
            Err(()) => continue,
        };
        state.push(lex.span(), piece);
    }
    color(text, &state.toks, out);
}

/// What comes next in the name after the `@` of a decorator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecoratorPath {
    Name,
    Dot,
}

/// Give each token its color, now that we can look at the tokens after
/// it.
fn color(text: &str, toks: &[Tok], out: &mut Tokens) {
    let mut decorator = None;
    for (i, t) in toks.iter().enumerate() {
        let path = decorator.take();
        let kind = match t.piece {
            Piece::LineEnd => continue,
            // A decorator can be any expression, as in `@False or f`, so a
            // keyword ends its name.
            Piece::Word => match word(text, toks, i) {
                Kind::Identifier if path == Some(DecoratorPath::Name) => {
                    decorator = Some(DecoratorPath::Dot);
                    Kind::Attribute
                }
                kind => kind,
            },
            Piece::Dot if path == Some(DecoratorPath::Dot) => {
                decorator = Some(DecoratorPath::Name);
                Kind::Attribute
            }
            Piece::Colored(Kind::Operator) if t.line_start && &text[t.r.clone()] == "@" => {
                decorator = Some(DecoratorPath::Name);
                Kind::Attribute
            }
            Piece::Open | Piece::Close | Piece::Colon | Piece::Dot => Kind::Punctuation,
            Piece::Colored(kind) => kind,
        };
        out.push(t.r.clone(), kind);
    }
}

/// The color of the word at `toks[i]`.
fn word(text: &str, toks: &[Tok], i: usize) -> Kind {
    let t = &toks[i];
    let w = &text[t.r.clone()];
    if KEYWORDS.contains(&w) {
        return Kind::Keyword;
    }
    if LITERALS.contains(&w) {
        return Kind::Constant;
    }
    // A word after a `.` names an attribute.
    if i > 0 && toks[i - 1].piece == Piece::Dot {
        return Kind::Identifier;
    }
    if CONSTANTS.contains(&w) {
        return Kind::Constant;
    }
    let next = toks.get(i + 1);
    let next_word = next
        .filter(|n| n.piece == Piece::Word)
        .map(|n| &text[n.r.clone()]);
    let soft = match w {
        // A `match` statement's header ends with a `:`. A `case` clause
        // can have its body after the `:` on the same line.
        "match" => t.line_start && subject_follows(text, next) && colons(&toks[i + 1..]).last,
        "case" => t.line_start && subject_follows(text, next) && colons(&toks[i + 1..]).any,
        // Two names in a row are an error anywhere else, so this is a
        // type alias, as in `type X = int`.
        "type" => next_word.is_some_and(|n| !KEYWORDS.contains(&n) && !LITERALS.contains(&n)),
        "lazy" => matches!(next_word, Some("import" | "from")),
        _ => false,
    };
    if soft {
        Kind::Keyword
    } else {
        Kind::Identifier
    }
}

/// Whether `next`, the token after a `match` or `case` at the start of a
/// statement, can start the subject of a `match` statement or the
/// pattern of a `case`.
fn subject_follows(text: &str, next: Option<&Tok>) -> bool {
    let Some(next) = next else {
        return false;
    };
    let next_text = &text[next.r.clone()];
    match next.piece {
        Piece::Word => {
            !KEYWORDS.contains(&next_text) || matches!(next_text, "not" | "lambda" | "await")
        }
        Piece::Colored(Kind::String | Kind::Number | Kind::Constant) => true,
        Piece::Colored(Kind::Operator) => matches!(next_text, "-" | "+" | "~" | "*"),
        Piece::Open => true,
        _ => false,
    }
}

/// Where the colons outside of brackets are in a statement.
struct Colons {
    /// Whether there are any.
    any: bool,
    /// Whether the statement ends with one, not counting comments.
    last: bool,
}

/// The colons outside of brackets in the statement that goes on with
/// `toks`. The statement can go on over several lines.
fn colons(toks: &[Tok]) -> Colons {
    let mut depth = 0;
    let mut colon = false;
    let mut last = None;
    for t in toks {
        match t.piece {
            Piece::LineEnd => break,
            Piece::Open => depth += 1,
            Piece::Close => depth -= 1,
            Piece::Colon if depth == 0 => colon = true,
            Piece::Colored(Kind::Comment) => continue,
            _ => {}
        }
        last = Some((t.piece, depth));
    }
    Colons {
        any: colon,
        last: last == Some((Piece::Colon, 0)),
    }
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

    fn kinds_of(text: &str, kind: Kind) -> Vec<&str> {
        tokens(text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    fn identifiers(text: &str) -> Vec<&str> {
        let mut out = Tokens::default();
        lex(text, &mut out);
        out.identifiers(text)
    }

    #[test]
    fn lexes_a_function() {
        use Kind::*;
        let text = "def f(x: int = 1) -> list[str]:\n    return [x, None]  # done\n";
        assert_eq!(
            tokens(text),
            [
                ("def", Keyword),
                ("(", Punctuation),
                (":", Punctuation),
                ("=", Operator),
                ("1", Number),
                (")", Punctuation),
                ("->", Operator),
                ("[", Punctuation),
                ("]:", Punctuation),
                ("return", Keyword),
                ("[", Punctuation),
                (",", Punctuation),
                ("None", Constant),
                ("]", Punctuation),
                ("# done", Comment),
            ]
        );
        assert_eq!(identifiers(text), ["f", "x", "int", "list", "str", "x"]);
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("x = 1  # a 'b' \"\"\"\n#!shebang\r\ny"),
            [
                ("=", Kind::Operator),
                ("1", Kind::Number),
                ("# a 'b' \"\"\"", Kind::Comment),
                ("#!shebang", Kind::Comment),
            ]
        );
        assert_eq!(kinds_of("'#' # c", Kind::Comment), ["# c"]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#"'a\n\'b' "\x7f\101\N{DASH}\u00e9\U0001F600" b"\N{DASH}\u00e9\x00""#),
            [
                ("'a", Kind::String),
                ("\\n\\'", Kind::Escape),
                ("b'", Kind::String),
                ("\"", Kind::String),
                ("\\x7f\\101\\N{DASH}\\u00e9\\U0001F600", Kind::Escape),
                ("\"", Kind::String),
                ("b\"\\N{DASH}\\u00e9", Kind::String),
                ("\\x00", Kind::Escape),
                ("\"", Kind::String),
            ]
        );
        // A backslash before anything else is part of the string.
        assert_eq!(tokens(r"'\q\x1'"), [(r"'\q\x1'", Kind::String)]);
        // Raw strings have no escapes, but a backslash still keeps a
        // quote from ending one.
        assert_eq!(
            tokens(r#"r'\n\'' Rb"\\" BR'\x00'"#),
            [
                (r#"r'\n\''"#, Kind::String),
                (r#"Rb"\\""#, Kind::String),
                (r#"BR'\x00'"#, Kind::String),
            ]
        );
        assert_eq!(
            tokens("u'a' x\"b\""),
            [("u'a'", Kind::String), ("\"b\"", Kind::String)]
        );
    }

    #[test]
    fn strings_over_lines() {
        // A string that isn't triple-quoted ends with its line, unless a
        // backslash joins the next line to it.
        assert_eq!(
            tokens("'open\nx = 'a\\\nb'"),
            [
                ("'open", Kind::String),
                ("=", Kind::Operator),
                ("'a", Kind::String),
                ("\\\n", Kind::Escape),
                ("b'", Kind::String),
            ]
        );
        assert_eq!(
            tokens("\"\"\"doc\n'it' \"\"\" + '''a\r\nb''' '''open\nx"),
            [
                ("\"\"\"doc\n'it' \"\"\"", Kind::String),
                ("+", Kind::Operator),
                ("'''a\r\nb'''", Kind::String),
                ("'''open\nx", Kind::String),
            ]
        );
        assert_eq!(tokens("'''\"\"\"'''"), [("'''\"\"\"'''", Kind::String)]);
        assert_eq!(
            tokens("'' \"\""),
            [("''", Kind::String), ("\"\"", Kind::String)]
        );
    }

    #[test]
    fn f_strings() {
        use Kind::*;
        let text = r#"f"a\n{x + 1!r:>{w}} {{b}} {d["k"]=}" t'{y}'"#;
        assert_eq!(
            tokens(text),
            [
                ("f\"a", String),
                ("\\n", Escape),
                ("{", Punctuation),
                ("+", Operator),
                ("1", Number),
                ("!r", Operator),
                (":", Punctuation),
                (">", String),
                ("{", Punctuation),
                ("}}", Punctuation),
                (" ", String),
                ("{{", Escape),
                ("b", String),
                ("}}", Escape),
                (" ", String),
                ("{", Punctuation),
                ("[", Punctuation),
                ("\"k\"", String),
                ("]", Punctuation),
                ("=", Operator),
                ("}", Punctuation),
                ("\"", String),
                ("t'", String),
                ("{", Punctuation),
                ("}", Punctuation),
                ("'", String),
            ]
        );
        assert_eq!(identifiers(text), ["x", "w", "d", "y"]);
        // A format spec can hold escapes and fields, but not doubled
        // braces. In a field, `:=` starts a format spec.
        assert_eq!(
            tokens(r"f'{x:\n{y}.{z:{w}}f}' f'{x:=5}'"),
            [
                ("f'", String),
                ("{", Punctuation),
                (":", Punctuation),
                ("\\n", Escape),
                ("{", Punctuation),
                ("}", Punctuation),
                (".", String),
                ("{", Punctuation),
                (":{", Punctuation),
                ("}}", Punctuation),
                ("f", String),
                ("}", Punctuation),
                ("'", String),
                ("f'", String),
                ("{", Punctuation),
                (":", Punctuation),
                ("=5", String),
                ("}", Punctuation),
                ("'", String),
            ]
        );
        // `\N{...}` names a character, except in a raw f-string, where
        // the braces are a field.
        assert_eq!(
            tokens(r"f'\N{DASH}' rf'\N{x}\{y}'"),
            [
                ("f'", String),
                ("\\N{DASH}", Escape),
                ("'", String),
                ("rf'\\N", String),
                ("{", Punctuation),
                ("}", Punctuation),
                ("\\", String),
                ("{", Punctuation),
                ("}", Punctuation),
                ("'", String),
            ]
        );
    }

    #[test]
    fn f_strings_over_lines() {
        use Kind::*;
        // The code in a field can go on over lines, even in a string
        // that isn't triple-quoted, and hold comments.
        assert_eq!(
            tokens("f'{x # c\n+ 1}' f'a\nx'"),
            [
                ("f'", String),
                ("{", Punctuation),
                ("# c", Comment),
                ("+", Operator),
                ("1", Number),
                ("}", Punctuation),
                ("'", String),
                ("f'a", String),
                ("'", String),
            ]
        );
        assert_eq!(
            tokens("f'''a\n{x}\nb''' + 1"),
            [
                ("f'''a\n", String),
                ("{", Punctuation),
                ("}", Punctuation),
                ("\nb'''", String),
                ("+", Operator),
                ("1", Number),
            ]
        );
        // A string that ends inside a format spec leaves its fields.
        assert_eq!(
            tokens("f'{x:{y:a\n}"),
            [
                ("f'", String),
                ("{", Punctuation),
                (":{", Punctuation),
                (":", Punctuation),
                ("a", String),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "0x1F 0o17 0b1010_1010 1_000 1.5 2e10 3.0E-5 .5 1. 1e5j 2J 1.j",
                Kind::Number
            ),
            [
                "0x1F",
                "0o17",
                "0b1010_1010",
                "1_000",
                "1.5",
                "2e10",
                "3.0E-5",
                ".5",
                "1.",
                "1e5j",
                "2J",
                "1.j"
            ]
        );
        // Like the tokenizer, the lexer stops a number where it can't go
        // on, so these are a number and a keyword.
        assert_eq!(
            tokens("1if x else 0x1for"),
            [
                ("1", Kind::Number),
                ("if", Kind::Keyword),
                ("else", Kind::Keyword),
                ("0x1f", Kind::Number),
                ("or", Kind::Keyword),
            ]
        );
        assert_eq!(
            tokens("x.y 1..real"),
            [
                (".", Kind::Punctuation),
                ("1.", Kind::Number),
                (".", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn keywords_and_constants() {
        assert_eq!(
            tokens("not None is True and self.NotImplemented ... Ellipsis __debug__ print"),
            [
                ("not", Kind::Keyword),
                ("None", Kind::Constant),
                ("is", Kind::Keyword),
                ("True", Kind::Constant),
                ("and", Kind::Keyword),
                (".", Kind::Punctuation),
                ("...", Kind::Constant),
                ("Ellipsis", Kind::Constant),
                ("__debug__", Kind::Constant),
            ]
        );
        assert_eq!(
            identifiers("self.NotImplemented"),
            ["self", "NotImplemented"]
        );
        assert_eq!(kinds_of("Größe x·if _if if", Kind::Keyword), ["if"]);
    }

    #[test]
    fn match_and_case() {
        let text = "match command.split():\n    case [action, *rest]:\n        pass\n    case {'a': 1} | Point(x=0) if x:\n        pass\n    case -1 | _:\n        pass";
        assert_eq!(
            kinds_of(text, Kind::Keyword),
            [
                "match", "case", "pass", "case", "if", "pass", "case", "pass"
            ]
        );
        assert_eq!(
            kinds_of("match (a, b):\n    case (1, 2):", Kind::Keyword),
            ["match", "case"]
        );
        assert_eq!(
            kinds_of("match x:  # c\nmatch not x:", Kind::Keyword),
            ["match", "match", "not"]
        );
        // A `match` statement ends with a `:`, and a `case` has one, even
        // when brackets or a backslash carry them over several lines.
        assert_eq!(
            kinds_of(
                "match(x):\nmatch [\n  x]:\nmatch(\n  x,\n):\nmatch \\\n  x:\nmatch '''a\nb''':",
                Kind::Keyword
            ),
            ["match", "match", "match", "match", "match"]
        );
        assert_eq!(
            kinds_of(
                "case(x): pass\ncase [\n  1,  # c\n]: pass\ncase \\\n  -1:",
                Kind::Keyword
            ),
            ["case", "pass", "case", "pass", "case"]
        );
        // Otherwise brackets after the word call or index a name.
        assert_eq!(
            identifiers(
                "match(x)\nmatch (x)\nmatch[1:\n  2]\nmatch = re.match(p)\ncase.x\nif match: case + 1\nmatch, case = 1, 2\nmatch(x)  # :\ncase(\n  x)"
            ),
            [
                "match", "x", "match", "x", "match", "match", "re", "match", "p", "case", "x",
                "match", "case", "match", "case", "match", "x", "case", "x"
            ]
        );
        // An annotation isn't a `match` statement.
        assert_eq!(identifiers("match[x]: int"), ["match", "x", "int"]);
        // Only at the start of a line.
        assert_eq!(identifiers("x = match y"), ["x", "match", "y"]);
        assert_eq!(identifiers("f(\n  match x)"), ["f", "match", "x"]);
    }

    #[test]
    fn type_and_lazy() {
        assert_eq!(
            kinds_of(
                "type X = int\ntype Y[T] = list[T]; lazy import a\nlazy from b import c\ntype \\\n  Z = int\nlazy \\\n  import d",
                Kind::Keyword
            ),
            [
                "type", "type", "lazy", "import", "lazy", "from", "import", "type", "lazy",
                "import"
            ]
        );
        assert_eq!(
            identifiers(
                "type(x)\nx: type = int\ntype = 1\nfor type in x: pass\nlazy = x.type\nlazy\nimport y"
            ),
            [
                "type", "x", "x", "type", "int", "type", "type", "x", "lazy", "x", "type", "lazy",
                "y"
            ]
        );
    }

    #[test]
    fn decorators() {
        assert_eq!(
            tokens("@functools.cache\n  @a.b(c)\n@x[0].y\n@False or f\nx @ y\nx @= y"),
            [
                ("@functools.cache", Kind::Attribute),
                ("@a.b", Kind::Attribute),
                ("(", Kind::Punctuation),
                (")", Kind::Punctuation),
                ("@x", Kind::Attribute),
                ("[", Kind::Punctuation),
                ("0", Kind::Number),
                ("].", Kind::Punctuation),
                ("@", Kind::Attribute),
                ("False", Kind::Constant),
                ("or", Kind::Keyword),
                ("@", Kind::Operator),
                ("@=", Kind::Operator),
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds_of(
                "a //= b ** c; d := e -> f != g; h <<= i >= j; ~k; l ^= m",
                Kind::Operator
            ),
            ["//=", "**", ":=", "->", "!=", "<<=", ">=", "~", "^="]
        );
        assert_eq!(
            kinds_of("a[1:2, ...]; b.c", Kind::Punctuation),
            ["[", ":", ",", "];", "."]
        );
    }

    /// A backslash at the end of a line joins the next line to it, so a
    /// statement doesn't start there.
    #[test]
    fn joined_lines() {
        assert_eq!(identifiers("x = \\\n  match y"), ["x", "match", "y"]);
        assert_eq!(kinds_of("x = \\\n  1", Kind::Number), ["1"]);
        assert_eq!(kinds_of("x = 1\\\n", Kind::Punctuation), [] as [&str; 0]);
    }

    /// Fields nested deeper than Python allows don't use up the stack.
    #[test]
    fn deep_fields() {
        let n = 100_000;
        let text = format!("f'{}{}'", "{x:".repeat(n), "}".repeat(n));
        assert_eq!(kinds_of(&text, Kind::String), ["f'", "'"]);
    }
}
