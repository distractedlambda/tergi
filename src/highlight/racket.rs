//! A lexer for Racket and Scribble.
//!
//! It follows what Racket's reader does with the default readtable. The
//! reader's source in `racket/src/expander/read` is the best reference,
//! because the reference manual leaves out a few details. For example, a
//! `;` comment only ends at a linefeed. Symbols are plain, even ones like
//! `define`, because only the expander knows what they mean.
//!
//! Files in `#lang at-exp` and the Scribble languages also use the
//! @-reader from `scribble/reader.rkt` in the `at-exp-lib` package. Its
//! @-forms mix Racket data with text, and nest in each other, so the
//! lexer keeps a stack of what it's inside of (`Frame`).

use logos::{Lexer, Logos};

use super::{Kind, Language, Tokens, shrubbery};

/// How to lex the next Racket token.
#[derive(Debug, Clone, Copy, Default)]
struct Mode {
    /// Whether `|` ends a symbol. The @-reader reads the command of an
    /// @-form with a readtable where it does, so that `@foo|{text}|` has
    /// text after the command. Escapes such as `@|x|` use that readtable
    /// too (see `Data::bars`).
    bar_ends: bool,
}

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(extras = Mode)]
// The reader skips a byte order mark the same way it skips whitespace.
#[logos(skip r"[\s\u{FEFF}]+")]
enum Token {
    #[regex(r";[^\n]*", allow_greedy = true)]
    #[token("#|", block_comment)]
    #[regex(r"#![ /]", unix_comment)]
    Comment,

    // Comments out the datum after it.
    #[token("#;")]
    DatumComment,

    #[token("\"", string)]
    #[regex(r#"#[rp]x""#, string)]
    Str,

    #[token("#\"", string)]
    #[regex(r##"#[rp]x#""##, string)]
    Bytes,

    #[token("#<<", here_string)]
    HereStr,

    #[token("#\\", character)]
    Char,

    #[regex(r"#[tTfF]|#true|#false", delimited)]
    Bool,

    #[token("#:", atom)]
    Keyword,

    // A radix or exactness prefix, which makes the rest a number or an
    // error.
    #[regex(r"#[eEiIbBoOdDxX]", atom)]
    Number,

    #[token("#%", atom)]
    Symbol,

    // Anything else that starts with neither a delimiter nor `#` is a
    // symbol or a number, or `.` on its own. So is `@` when the @-reader
    // is off.
    #[regex(r#"[^\s\u{FEFF}()\[\]{}",'`;#@]"#, atom)]
    Atom,

    // Starts an @-form when the @-reader is on. `@` doesn't end a symbol,
    // so `a@b` is one symbol either way.
    #[token("@")]
    At,

    // Lists, vectors, flvectors, fxvectors, prefab structs and hash tables.
    #[regex(r"[(\[{]")]
    #[regex(r"#[0-9]*[(\[{]")]
    #[regex(r"#[fF][lx][0-9]*[(\[{]")]
    #[regex(r"#s[(\[{]")]
    // The reader takes `eq`, `eqv` and `alw` in more combinations than it
    // documents, such as `#hasheqeq(`.
    #[regex(r"#[hH][aA][sS][hH]([aA][lL][wW])?([eE][qQ][vV]?)*[(\[{]")]
    Open,

    #[regex(r"[)\]}]")]
    Close,

    // Quotes, boxes and graph labels, which each apply to the datum
    // after them.
    #[regex(r"[',`]|,@|#[',`]|#,@|#&|#[0-9]+=")]
    Prefix,

    #[regex(r"#[0-9]+#")]
    GraphRef,

    // `#cs` and `#ci` read the next datum with case sensitivity on or off,
    // and `#reader` reads a module path and then lets that module read.
    #[regex(r"#[cC][sSiI]|#reader")]
    Directive,

    #[regex(r"#lang [a-zA-Z0-9+_/-]+", lang)]
    #[regex(r"#![a-zA-Z0-9+_-][a-zA-Z0-9+_/-]*", lang)]
    Lang,
}

/// Whether `c` ends a symbol, number or other sequence of characters.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || "()[]{}\",'`;\u{FEFF}".contains(c)
}

/// Skip to the end of a block comment. Block comments nest. A comment
/// without an end runs to the end of the text.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut depth = 0;
    // The reader forgets the previous character after it uses it, so `#|#`
    // opens a comment and then sees a `#` that doesn't close anything.
    let mut prev = None;
    let mut i = 0;
    while i < rest.len() {
        let c = rest[i];
        i += 1;
        match (prev, c) {
            (Some(b'#'), b'|') => {
                depth += 1;
                prev = None;
            }
            (Some(b'|'), b'#') => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                prev = None;
            }
            _ => prev = Some(c),
        }
    }
    lex.bump(i);
}

/// Skip to the end of a line comment that started with `#! ` or `#!/`.
/// A backslash at the end of a line continues it onto the next.
fn unix_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'\n' && (i == 0 || rest[i - 1] != b'\\') {
            break;
        }
        i += 1;
    }
    lex.bump(i);
}

/// Skip to the closing quote. A string without one runs to the end.
fn string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' => i += 2,
            b'"' => {
                i += 1;
                break;
            }
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    lex.bump(i.min(rest.len()));
}

/// Skip a here string. The rest of the `#<<` line is the terminator, and
/// the string ends at the first line after it that holds just the
/// terminator. A carriage return is part of a line like any other
/// character, so this works with CRLF line ends too.
fn here_string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    let Some(nl) = rest.find('\n') else {
        lex.bump(rest.len());
        return;
    };
    let terminator = &rest[..nl];
    let mut start = nl + 1;
    let end = loop {
        let line_end = rest[start..].find('\n').map_or(rest.len(), |i| start + i);
        if &rest[start..line_end] == terminator {
            break line_end;
        }
        if line_end == rest.len() {
            break rest.len();
        }
        start = line_end + 1;
    };
    lex.bump(end);
}

/// Skip the rest of a character after `#\`.
fn character(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    let mut chars = rest.chars();
    let Some(c) = chars.next() else {
        return;
    };
    let second = chars.next();
    let is_octal = |c: char| c.is_digit(8);
    let len = if is_octal(c) && second.is_some_and(is_octal) {
        // Two octal digits commit the reader to a third character, which
        // had better be an octal digit too.
        2 + chars.next().map_or(0, char::len_utf8)
    } else if c == 'u' || c == 'U' {
        let max = if c == 'u' { 4 } else { 8 };
        1 + rest[1..]
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_hexdigit)
            .count()
    } else if c.is_alphabetic() && second.is_some_and(char::is_alphabetic) {
        // A name such as `newline`. Any run of letters is read as a name,
        // and one that isn't a name is an error.
        rest.find(|c: char| !c.is_alphabetic())
            .unwrap_or(rest.len())
    } else {
        c.len_utf8()
    };
    lex.bump(len);
}

/// Check that a delimiter or the end of the text comes next.
fn delimited(lex: &mut Lexer<Token>) -> bool {
    let bar_ends = lex.extras.bar_ends;
    lex.remainder()
        .chars()
        .next()
        .is_none_or(|c| is_delimiter(c) || c == '|' && bar_ends)
}

/// Skip to the end of a symbol, number or keyword. `|` quotes everything
/// up to the next `|`, and a backslash quotes the character after it.
fn atom(lex: &mut Lexer<Token>) {
    // The first character of an atom may already be in the slice, and it
    // could be one that quotes, so scan from the start of the slice.
    let start = match lex.slice().as_bytes() {
        [b'#', ..] => lex.span().end,
        _ => lex.span().start,
    };
    let text = lex.source();
    let bar_ends = lex.extras.bar_ends;
    if bar_ends && lex.slice() == "|" {
        // When `|` ends symbols, a symbol that starts with one runs to
        // the next, and a backslash doesn't quote in it.
        let rest = lex.remainder();
        lex.bump(rest.find('|').map_or(rest.len(), |i| i + 1));
        return;
    }
    let mut quoted = false;
    let mut chars = text[start..].char_indices();
    let mut end = text.len();
    while let Some((i, c)) = chars.next() {
        if quoted {
            quoted = c != '|';
        } else if c == '|' && bar_ends {
            end = start + i;
            break;
        } else if c == '|' {
            quoted = true;
        } else if c == '\\' {
            chars.next();
        } else if is_delimiter(c) {
            end = start + i;
            break;
        }
    }
    lex.bump(end - lex.span().end);
}

/// Check that a language name ends with whitespace or the end of the
/// text, and doesn't start or end with `/`.
///
/// `at-exp` reads the name of another language after it, on the same
/// line, so that name is part of the token too.
fn lang(lex: &mut Lexer<Token>) -> bool {
    let name = lang_name(lex.slice());
    let rest = lex.remainder();
    let ok = !name.starts_with('/')
        && !name.ends_with('/')
        && rest.chars().next().is_none_or(char::is_whitespace);
    if ok && name == "at-exp" {
        // This follows the pattern `^[ \t]+(.*?)(?=\s|$)` in
        // `make-meta-reader` from `syntax/module-reader`.
        let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        if spaces > 0 {
            let more = rest[spaces..]
                .find(char::is_whitespace)
                .unwrap_or(rest.len() - spaces);
            lex.bump(spaces + more);
        }
    }
    ok
}

/// Whether we know that a language reads its modules with Racket's
/// default reader. A file in a language we don't know is read the way
/// its name suggests.
fn default_reader(name: &str) -> bool {
    const LANGUAGES: &[&str] = &[
        "deinprogramm",
        "frtime",
        "htdp",
        "info",
        "lazy",
        "mzscheme",
        "plai",
        "plait",
        "r5rs",
        "r6rs",
        "racket",
        "rosette",
        "s-exp",
        "scheme",
        "setup/infotab",
        "slideshow",
        "swindle",
        "typed",
        "web-server",
    ];
    LANGUAGES.iter().any(|base| {
        name.strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// The name in a `#lang` or `#!` line.
fn lang_name(token: &str) -> &str {
    token
        .strip_prefix("#lang ")
        .or_else(|| token.strip_prefix("#!"))
        .unwrap_or(token)
}

/// Whether the reader reads an atom as a number. It has no `#` prefix and
/// no quoting with `|` or a backslash.
///
/// This follows `string->number` in the reader's `number.rkt`, but only
/// checks the syntax. An atom that has the syntax of a number but no
/// value, such as `1/0`, is an error and not a symbol, so it counts as a
/// number here.
fn is_number(s: &str) -> bool {
    /// What we're reading. Each state is a function in `number.rkt`.
    #[derive(Clone, Copy)]
    enum State {
        /// The start of the number, or of the angle after `@`.
        Start,
        /// After a sign.
        Signed,
        Integer,
        /// After a `.`, and maybe digits after it.
        Decimal {
            digits: bool,
        },
        /// After digits and a `#` that stands for an unknown digit.
        Approx {
            dot: bool,
        },
        /// After an exponent mark.
        Exponent,
        /// After an exponent mark and its sign or digits.
        ExponentDigits {
            digits: bool,
        },
        /// After a `/`, and maybe digits after it.
        Rational {
            digits: bool,
        },
        /// After a denominator and a `#`.
        DenomApprox,
        /// After `inf.0`, `nan.0` and the like.
        Special,
    }
    use State::*;

    impl State {
        /// Whether what we've read so far is a whole number, or a whole
        /// part of a complex number.
        fn complete(self) -> bool {
            match self {
                Start | Signed | Exponent => false,
                Decimal { digits } | ExponentDigits { digits } | Rational { digits } => digits,
                Integer | Approx { .. } | DenomApprox | Special => true,
            }
        }
    }

    /// What came before the part we're reading.
    #[derive(Clone, Copy, PartialEq)]
    enum First {
        Nothing,
        /// Just a sign, so a final `i` makes an imaginary number.
        Sign,
        /// The real part of a complex number.
        Real,
        /// The magnitude of a complex number in polar form.
        Magnitude,
    }

    let s = s.as_bytes();
    let mut state = Start;
    let mut first = First::Nothing;
    // Whether the part we're reading is an extflonum, such as `1t0`.
    // Extflonums can't be parts of complex numbers.
    let mut ext = false;
    let is_exp_mark = |c: u8| b"eEdDlLfFsStT".contains(&c);
    let mut i = 0;
    loop {
        let Some(&c) = s.get(i) else {
            // The end of the atom.
            return state.complete()
                && match first {
                    First::Nothing | First::Sign => true,
                    First::Magnitude => !ext,
                    First::Real => false,
                };
        };
        i += 1;
        let digit = c.is_ascii_digit();
        if state.complete() {
            match c {
                b'i' | b'I' => {
                    return i == s.len() && !ext && matches!(first, First::Sign | First::Real);
                }
                b'+' | b'-' | b'@' => {
                    if ext || matches!(first, First::Real | First::Magnitude) {
                        return false;
                    }
                    (first, state) = if c == b'@' {
                        (First::Magnitude, Start)
                    } else {
                        (First::Real, Signed)
                    };
                    continue;
                }
                _ if is_exp_mark(c) && !matches!(state, ExponentDigits { .. } | Special) => {
                    ext = matches!(c, b't' | b'T');
                    state = Exponent;
                    continue;
                }
                _ => {}
            }
        }
        state = match state {
            Start => match c {
                _ if digit => Integer,
                b'+' | b'-' => {
                    if first == First::Nothing {
                        first = First::Sign;
                    }
                    Signed
                }
                b'.' => Decimal { digits: false },
                _ => return false,
            },
            Signed => match c {
                _ if digit => Integer,
                b'.' => Decimal { digits: false },
                b'i' | b'I' if i == s.len() => {
                    // An imaginary unit, as in `+i` or `1-i`.
                    return matches!(first, First::Sign | First::Real);
                }
                b'i' | b'I' | b'n' | b'N' => {
                    let rest = &s[i - 1..];
                    let special = rest.len() >= 5
                        && (rest[..4].eq_ignore_ascii_case(b"inf.")
                            || rest[..4].eq_ignore_ascii_case(b"nan."))
                        && b"0fFtT".contains(&rest[4]);
                    if !special {
                        return false;
                    }
                    ext = matches!(rest[4], b't' | b'T');
                    i += 4;
                    Special
                }
                _ => return false,
            },
            Integer => match c {
                _ if digit => Integer,
                b'.' => Decimal { digits: true },
                b'/' => Rational { digits: false },
                b'#' => Approx { dot: false },
                _ => return false,
            },
            Decimal { digits } => match c {
                _ if digit => Decimal { digits: true },
                b'#' if digits => Approx { dot: true },
                _ => return false,
            },
            Approx { dot } => match c {
                b'#' => Approx { dot },
                b'.' if !dot => Approx { dot: true },
                b'/' if !dot => Rational { digits: false },
                _ => return false,
            },
            Exponent => match c {
                _ if digit => ExponentDigits { digits: true },
                b'+' | b'-' => ExponentDigits { digits: false },
                _ => return false,
            },
            ExponentDigits { .. } => match c {
                _ if digit => ExponentDigits { digits: true },
                _ => return false,
            },
            Rational { digits } => match c {
                _ if digit => Rational { digits: true },
                b'#' if digits => DenomApprox,
                _ => return false,
            },
            DenomApprox => match c {
                b'#' => DenomApprox,
                _ => return false,
            },
            Special => return false,
        };
    }
}

/// The pieces of a string. Anything else in one, such as a backslash
/// before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r#"\\([abtnvfre\\"'\n]|\r\n?|[0-7]{1,3}|x[0-9a-fA-F]{1,2})"#)]
    Escape,
    // Byte strings don't take these.
    #[regex(r"\\(u[0-9a-fA-F]{1,4}|U[0-9a-fA-F]{1,8})")]
    Unicode,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

fn lex_string(text: &str, start: usize, bytes: bool, out: &mut Tokens) {
    for (part, r) in Part::lexer(text).spanned() {
        let kind = match part {
            Ok(Part::Escape) => Kind::Escape,
            Ok(Part::Unicode) if !bytes => Kind::Escape,
            _ => Kind::String,
        };
        out.push(start + r.start..start + r.end, kind);
    }
}

/// The delimiters around the text of an @-form, such as `{` and `}`, or
/// `|<{` and `}>|`.
#[derive(Debug)]
struct Delims {
    open: String,
    close: String,
    /// What has to come before an `@` for it to start an @-form inside the
    /// text. For `|<{` it's `|<`, so `|<@foo` is an @-form and `@foo` is
    /// text.
    prefix: String,
}

impl Delims {
    /// The opening delimiter at the start of `text`, if there is one.
    ///
    /// This follows `str:lines-begin*` in the @-reader. Between `|` and
    /// `{` may come any ASCII characters except letters, digits, `@`, `\`,
    /// `{`, and whitespace other than a vertical tab. The closing
    /// delimiter is the opening one backwards, with brackets turned
    /// around.
    fn at(text: &str) -> Option<Delims> {
        let b = text.as_bytes();
        let n = match b.first()? {
            b'{' => 0,
            b'|' => {
                let ok = |c: &&u8| {
                    **c < 0x7f && !c.is_ascii_alphanumeric() && !b" \t\r\n\x0c@\\{".contains(c)
                };
                let n = 1 + b[1..].iter().take_while(ok).count();
                if b.get(n) != Some(&b'{') {
                    return None;
                }
                n
            }
            _ => return None,
        };
        let open = &text[..=n];
        let close = open
            .chars()
            .rev()
            .map(|c| match c {
                '(' => ')',
                ')' => '(',
                '[' => ']',
                ']' => '[',
                '{' => '}',
                '}' => '{',
                '<' => '>',
                '>' => '<',
                c => c,
            })
            .collect();
        Some(Delims {
            open: open.to_string(),
            close,
            prefix: text[..n].to_string(),
        })
    }
}

/// What comes next in an @-form.
#[derive(Debug, Clone, Copy)]
enum Next {
    /// Data in brackets, then text in braces. Either may be missing.
    Data,
    /// Text in braces, or nothing.
    Body,
    /// Nothing. The @-form has ended.
    Done,
}

/// What ends a run of Racket data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The end of the text. A closing parenthesis that doesn't match
    /// anything is skipped.
    Text,
    /// The `]` after the data in an @-form, as in `@foo[1 2]`.
    Bracket,
    /// The `|` that ends an escape, as in `@|x|`.
    Bar,
    /// One datum, which the `#;` at `start` comments out.
    ///
    /// `items` says whether a comment counts as that datum. The @-reader
    /// reads the data in `@foo[...]` and `@|...|` with
    /// `read-syntax/recursive`, which returns comments instead of
    /// skipping them. So a `#;` there takes the next comment as its
    /// datum, such as `; c` in `@foo[#; ; c\n a]`. Inside parentheses,
    /// or after a prefix such as `'`, it doesn't.
    Comment { start: usize, items: bool },
    /// One datum, which is the command of an @-form, such as `foo` in
    /// `@foo{text}`.
    Command,
    /// The `}` after Racket data in shrubbery notation, as in `#{x}`.
    /// The `}` isn't part of the data.
    Brace,
}

/// Racket data, up to its `End`.
#[derive(Debug, Clone, Copy)]
struct Data {
    end: End,
    /// How many parentheses are open.
    depth: usize,
    /// Whether `|` ends a symbol. The @-reader reads an escape such as
    /// `@|x|` with a readtable where it does, and everything inside uses
    /// that readtable too. The exception is that the Scribble languages
    /// read data in brackets with the usual readtable again.
    bars: bool,
    /// Whether a prefix such as `'` is waiting for its datum.
    prefixed: bool,
}

impl Data {
    fn new(end: End, bars: bool) -> Data {
        Data {
            end,
            depth: 0,
            bars,
            prefixed: false,
        }
    }

    /// Whether this ends after one datum.
    fn one(&self) -> bool {
        matches!(self.end, End::Comment { .. } | End::Command)
    }
}

/// Something that the lexer is inside of.
#[derive(Debug)]
enum Frame {
    Data(Data),
    /// An @-form, between its parts.
    At {
        next: Next,
        bars: bool,
    },
    /// Text, in the body of an @-form, or in a whole Scribble document.
    /// A document has no delimiters, and braces in it are plain text.
    Text {
        delims: Option<Delims>,
        /// How many opening delimiters inside the text haven't been
        /// closed yet. Those are text too.
        depth: usize,
        /// Where the `@;` starts, if this is a comment such as
        /// `@;{text}`.
        comment: Option<usize>,
        bars: bool,
    },
}

impl Frame {
    fn text(delims: Option<Delims>, comment: Option<usize>, bars: bool) -> Frame {
        Frame::Text {
            delims,
            depth: 0,
            comment,
            bars,
        }
    }

    /// Where the comment starts, if this is one.
    fn comment(&self) -> Option<usize> {
        match *self {
            Frame::Data(Data {
                end: End::Comment { start, .. },
                ..
            }) => Some(start),
            Frame::Text { comment, .. } => comment,
            _ => None,
        }
    }

    /// Whether `|` ends a symbol in Racket data here.
    fn bars(&self) -> bool {
        match *self {
            Frame::Data(Data { bars, .. }) | Frame::At { bars, .. } | Frame::Text { bars, .. } => {
                bars
            }
        }
    }
}

struct Lex<'a> {
    text: &'a str,
    out: &'a mut Tokens,
    /// What we're inside of. The first frame is the whole file, and it
    /// never ends.
    stack: Vec<Frame>,
    /// Where we are, just after what we read last.
    pos: usize,
    /// Whether `@` starts an @-form outside of text.
    at: bool,
    /// Whether the file is `#lang at-exp`, whose data in brackets keep
    /// the readtable of an escape around them.
    at_exp: bool,
    /// Whether we haven't read anything other than comments yet, so a
    /// `#lang` line can still choose the reader.
    first: bool,
    /// How many frames on the stack are comments. Nothing inside a
    /// comment gets a color of its own.
    comments: usize,
    /// Set when a `#lang` line chooses a language that uses shrubbery
    /// notation. It says whether the language reads text.
    shrubbery: Option<bool>,
    /// The language that the file's name gives. It chooses the reader
    /// when the `#lang` line names a language we don't know.
    file: Language,
}

impl Lex<'_> {
    fn emit(&mut self, r: std::ops::Range<usize>, kind: Kind) {
        if self.comments == 0 {
            self.out.push(r, kind);
        }
    }

    fn push(&mut self, frame: Frame) {
        if frame.comment().is_some() {
            self.comments += 1;
        }
        self.stack.push(frame);
    }

    /// End the innermost frame where we are.
    fn pop(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if let Some(start) = frame.comment() {
            self.comments -= 1;
            if self.comments == 0 {
                self.comment(start..self.pos);
            }
            self.commented();
        }
        if let Frame::At { .. } = frame {
            self.datum();
        }
    }

    fn data(&mut self) -> Option<&mut Data> {
        match self.stack.last_mut() {
            Some(Frame::Data(d)) => Some(d),
            _ => None,
        }
    }

    /// Note that we've read a whole datum.
    fn datum(&mut self) {
        if self.data().is_some_and(|d| d.depth == 0 && d.one()) {
            self.pop();
        }
    }

    /// Note that we've read a whole comment.
    fn commented(&mut self) {
        if self.data().is_some_and(|d| {
            d.depth == 0 && !d.prefixed && matches!(d.end, End::Comment { items: true, .. })
        }) {
            self.pop();
        }
    }

    fn bars(&self) -> bool {
        self.stack.last().is_some_and(Frame::bars)
    }

    fn run(&mut self) {
        loop {
            let more = match self.stack.last() {
                None => false,
                Some(&Frame::At { next, .. }) => {
                    self.at_next(next);
                    true
                }
                Some(Frame::Text { .. }) => self.text(),
                Some(Frame::Data(_)) => self.racket(),
            };
            if !more {
                break;
            }
        }
        // A comment that the text doesn't finish runs to its end, like an
        // unfinished block comment. Then a hunk lexed on its own gets the
        // same colors as it does in the whole file, where the comment goes
        // on.
        if let Some(start) = self.stack.iter().find_map(Frame::comment) {
            self.comment(start..self.text.len());
        }
    }

    /// Color a `#;` or `@;{` comment, but not the whitespace at the end of
    /// each of its lines. Whether a comment covers the whitespace at the
    /// end of a line can depend on what comes on the next line, as in
    /// `#;` followed by a `)` that ends it, and colors shouldn't.
    fn comment(&mut self, r: std::ops::Range<usize>) {
        self.out.push_comment(self.text, r);
    }

    /// Read the next Racket token. Returns false at the end of the text.
    fn racket(&mut self) -> bool {
        let text = self.text;
        let Some(&mut Data {
            end,
            depth,
            bars,
            prefixed,
        }) = self.data()
        else {
            return false;
        };
        let command = end == End::Command;
        // The reader looks for the `|` that ends an escape between data,
        // and not after a prefix, which reads a datum of its own.
        if end == End::Bar && depth == 0 && !prefixed {
            let rest = &text[self.pos..];
            let space = rest.len()
                - rest
                    .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
                    .len();
            if rest[space..].starts_with('|') {
                let p = self.pos + space;
                self.emit(p..p + 1, Kind::Punctuation);
                self.pos = p + 1;
                self.pop();
                return true;
            }
        }

        let mode = Mode {
            bar_ends: bars || command && depth == 0,
        };
        let mut lex = Token::lexer_with_extras(&text[self.pos..], mode);
        let Some(mut token) = lex.next() else {
            return false;
        };
        if token == Ok(Token::At) && !self.at {
            atom(&mut lex);
            token = Ok(Token::Atom);
        }
        let before = self.pos;
        let span = before + lex.span().start..before + lex.span().end;
        let slice = lex.slice();
        self.pos = span.end;

        if self.first && token != Ok(Token::Comment) {
            self.first = false;
            if token == Ok(Token::Lang) && self.stack.len() == 1 {
                self.choose_reader(lang_name(slice));
            }
        }
        match token {
            Ok(Token::Comment | Token::DatumComment) => {}
            Ok(Token::At) if text[span.end..].starts_with(';') => {}
            Ok(Token::Prefix | Token::Directive) => {
                if let Some(d) = self.data() {
                    d.prefixed = true;
                }
            }
            _ => {
                if let Some(d) = self.data() {
                    d.prefixed = false;
                }
            }
        }

        // The name of a command, such as `foo` in `@foo{text}`, gets the
        // color of the `@`.
        let name = if command && depth == 0 {
            Some(Kind::Keyword)
        } else {
            None
        };
        let kind = match token {
            Ok(Token::Comment) => {
                self.emit(span, Kind::Comment);
                self.commented();
                return true;
            }
            Ok(Token::DatumComment) => {
                let items = depth == 0
                    && !prefixed
                    && match end {
                        End::Bracket | End::Bar => true,
                        End::Comment { items, .. } => items,
                        End::Text | End::Command | End::Brace => false,
                    };
                let start = span.start;
                self.push(Frame::Data(Data::new(End::Comment { start, items }, bars)));
                return true;
            }
            Ok(Token::Prefix) => {
                self.emit(span, Kind::Operator);
                return true;
            }
            Ok(Token::Directive) => {
                self.emit(span, Kind::Attribute);
                return true;
            }
            Ok(Token::Open) => {
                self.emit(span, Kind::Punctuation);
                if let Some(d) = self.data() {
                    d.depth += 1;
                }
                return true;
            }
            Ok(Token::Close) => {
                if depth > 0 {
                    self.emit(span, Kind::Punctuation);
                    if let Some(d) = self.data() {
                        d.depth -= 1;
                    }
                    self.datum();
                } else if end == End::Text || end == End::Brace && slice != "}" {
                    self.emit(span, Kind::Punctuation);
                } else if end == End::Bracket && slice == "]" {
                    self.emit(span, Kind::Punctuation);
                    self.pop();
                } else {
                    // This closes something further out, which ends this
                    // frame first. The reader would complain about that.
                    self.pos = before;
                    self.pop();
                }
                return true;
            }
            Ok(Token::At) => {
                self.at_form(span.start, span.start, false);
                return true;
            }
            Ok(Token::Str | Token::Bytes) => {
                if self.comments == 0 {
                    lex_string(slice, span.start, token == Ok(Token::Bytes), self.out);
                }
                None
            }
            Ok(Token::HereStr) => Some(Kind::String),
            Ok(Token::Char | Token::Bool) => Some(Kind::Constant),
            // Racket's keywords, such as `#:when`, are data and not
            // reserved words. But they're called keywords and are used
            // much like keywords in other languages.
            Ok(Token::Keyword) => Some(Kind::Keyword),
            Ok(Token::Number) => Some(Kind::Number),
            Ok(Token::Atom) => {
                if slice.contains(['|', '\\']) {
                    name
                } else if slice == "." {
                    Some(Kind::Punctuation)
                } else if is_number(slice) {
                    Some(Kind::Number)
                } else {
                    name
                }
            }
            Ok(Token::Symbol) => name,
            Ok(Token::GraphRef) => Some(Kind::Operator),
            Ok(Token::Lang) => Some(Kind::Attribute),
            Err(()) => None,
        };
        if let Some(kind) = kind {
            self.emit(span, kind);
        }
        if let Some(text_mode) = self.shrubbery {
            shrubbery::lex(text, self.pos, text_mode, self.out);
            self.stack.clear();
            self.pos = text.len();
            return false;
        }
        self.datum();
        true
    }

    /// Choose the reader for the rest of the file from the name of its
    /// language.
    fn choose_reader(&mut self, lang: &str) {
        self.shrubbery = shrubbery::reader(lang);
        let mut words = lang.split_whitespace();
        let mut name = words.next().unwrap_or("");
        self.at_exp = name == "at-exp";
        if self.at_exp {
            name = words.next().unwrap_or("");
        }
        self.at = self.at_exp;
        // Every language in `scribble-lib` and its friends reads the whole
        // module as text, with `read-syntax-inside`.
        let scribble = name.starts_with("scribble/");
        let known = self.shrubbery.is_some() || self.at_exp || scribble || default_reader(name);
        if scribble || !known && self.file == Language::Scribble {
            self.at = true;
            self.stack[0] = Frame::text(None, None, false);
        } else if !known && self.file == Language::Rhombus {
            self.shrubbery = Some(false);
        }
    }

    /// Read an @-form or an `@;` comment, after the `@` at `at`. The
    /// color of the `@` starts at `start`, which comes before it when the
    /// text needs a prefix such as `|<` before it.
    ///
    /// This follows `dispatcher` in the @-reader.
    fn at_form(&mut self, at: usize, start: usize, in_text: bool) {
        let text = self.text;
        let bars = self.bars();
        let after = at + 1;
        if let Some(rest) = text[after..].strip_prefix(';') {
            let p = after + 1;
            if let Some(d) = Delims::at(rest) {
                self.pos = p + d.open.len();
                self.push(Frame::text(Some(d), Some(start), bars));
            } else {
                // The reader also skips the linefeed and the spaces and
                // tabs after it, which makes no difference to the colors.
                let end = rest.find('\n').map_or(text.len(), |i| p + i);
                self.emit(start..end, Kind::Comment);
                self.pos = end;
                self.commented();
            }
            return;
        }

        self.emit(start..after, Kind::Keyword);
        let mut p = after;
        while let Some(n) = ["#,@", ",@", "#'", "#`", "#,", "'", "`", ","]
            .iter()
            .find(|q| text[p..].starts_with(**q))
            .map(|q| q.len())
        {
            self.emit(p..p + n, Kind::Operator);
            p += n;
        }
        self.pos = p;
        let rest = &text[p..];
        let escape = rest.starts_with('|');
        if rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\x0B', '\x0C', '\r']) {
            // The reader complains about whitespace here.
            self.push(Frame::At {
                next: Next::Done,
                bars,
            });
        } else if let Some(d) = Delims::at(rest).filter(|_| !(in_text && escape && p == after)) {
            // Outside of text, the reader tries `@|{` as text before it
            // tries it as an escape. Inside text, it doesn't.
            self.emit(p..p + d.open.len(), Kind::Punctuation);
            self.pos = p + d.open.len();
            self.push(Frame::At {
                next: Next::Done,
                bars,
            });
            self.push(Frame::text(Some(d), None, bars));
        } else if rest.starts_with('[') {
            self.emit(p..p + 1, Kind::Punctuation);
            self.pos = p + 1;
            self.push(Frame::At {
                next: Next::Body,
                bars,
            });
            self.push(Frame::Data(Data::new(End::Bracket, bars && self.at_exp)));
        } else if escape {
            self.emit(p..p + 1, Kind::Punctuation);
            self.pos = p + 1;
            self.push(Frame::At {
                next: Next::Done,
                bars,
            });
            self.push(Frame::Data(Data::new(End::Bar, true)));
        } else {
            self.push(Frame::At {
                next: Next::Data,
                bars,
            });
            self.push(Frame::Data(Data::new(End::Command, bars)));
        }
    }

    /// Read the next part of an @-form, right after the previous one.
    fn at_next(&mut self, next: Next) {
        let p = self.pos;
        let rest = &self.text[p..];
        let bars = self.bars();
        let set_next = |lex: &mut Self, n: Next| {
            if let Some(Frame::At { next, .. }) = lex.stack.last_mut() {
                *next = n;
            }
        };
        match next {
            Next::Data if rest.starts_with('[') => {
                self.emit(p..p + 1, Kind::Punctuation);
                self.pos = p + 1;
                set_next(self, Next::Body);
                self.push(Frame::Data(Data::new(End::Bracket, bars && self.at_exp)));
            }
            Next::Data | Next::Body => {
                set_next(self, Next::Done);
                if let Some(d) = Delims::at(rest) {
                    self.emit(p..p + d.open.len(), Kind::Punctuation);
                    self.pos = p + d.open.len();
                    self.push(Frame::text(Some(d), None, bars));
                }
            }
            Next::Done => self.pop(),
        }
    }

    /// Read text, up to the next thing that isn't plain text. Returns
    /// false at the end of the text.
    ///
    /// This follows `get-lines*` in the @-reader.
    fn text(&mut self) -> bool {
        let text = self.text;
        let p = self.pos;
        let rest = &text[p..];
        if rest.is_empty() {
            return false;
        }
        let Some(Frame::Text { delims, depth, .. }) = self.stack.last_mut() else {
            return false;
        };
        let mut command = rest.starts_with('@').then_some(0);
        if let Some(d) = delims {
            if rest.starts_with(&d.open) {
                *depth += 1;
                self.pos += d.open.len();
                return true;
            }
            if rest.starts_with(&d.close) {
                let n = d.close.len();
                self.pos += n;
                if *depth > 0 {
                    *depth -= 1;
                } else {
                    self.emit(p..p + n, Kind::Punctuation);
                    self.pop();
                }
                return true;
            }
            let n = d.prefix.len();
            command = (rest.starts_with(&d.prefix) && rest[n..].starts_with('@')).then_some(n);
        }
        if let Some(n) = command {
            self.at_form(p + n, p, true);
            return true;
        }
        // Everything up to a character that could start one of the above
        // is plain text.
        self.pos += rest
            .char_indices()
            .skip(1)
            .find(|&(_, c)| matches!(c, '@' | '{' | '}' | '|'))
            .map_or(rest.len(), |(i, _)| i);
        true
    }
}

/// Lex a file of Racket code, Scribble text, or Rhombus code, as `lang`
/// says. A `#lang` line at the start chooses the reader either way.
pub fn lex(text: &str, lang: Language, out: &mut Tokens) {
    let mode = Mode::default();
    // Without a `#lang` line, the text is a piece from the middle of a
    // file, and its name tells us how to read it. Comments can come before
    // the `#lang` line, as `read-language` skips them.
    let lang_line = Token::lexer_with_extras(text, mode).find(|t| *t != Ok(Token::Comment))
        == Some(Ok(Token::Lang));
    if lang == Language::Rhombus && !lang_line {
        shrubbery::lex(text, 0, false, out);
        return;
    }
    let scribble = lang == Language::Scribble;
    let text_first = scribble && !lang_line;
    let base = if text_first {
        Frame::text(None, None, false)
    } else {
        Frame::Data(Data::new(End::Text, false))
    };
    let mut lex = Lex {
        text,
        out,
        stack: vec![base],
        pos: 0,
        at: scribble,
        at_exp: false,
        first: !text_first,
        comments: 0,
        shrubbery: None,
        file: lang,
    };
    lex.run();
}

/// Lex the Racket data in `#{...}` in shrubbery notation, from `start` up
/// to the `}` that ends it. Returns where the last token before the `}`
/// ends.
pub fn lex_braced(text: &str, start: usize, out: &mut Tokens) -> usize {
    let mut lex = Lex {
        text,
        out,
        stack: vec![Frame::Data(Data::new(End::Brace, false))],
        pos: start,
        at: false,
        at_exp: false,
        first: false,
        comments: 0,
        shrubbery: None,
        file: Language::Racket,
    };
    lex.run();
    lex.pos
}

/// The length of the symbol at the very start of `text`, if one is there.
pub fn symbol_len(text: &str) -> Option<usize> {
    let mut lex = Token::lexer_with_extras(text, Mode::default());
    let token = lex.next()?;
    let slice = lex.slice();
    let symbol = match token {
        Ok(Token::Symbol) => true,
        Ok(Token::Atom) => slice != "." && (slice.contains(['|', '\\']) || !is_number(slice)),
        _ => false,
    };
    (symbol && lex.span().start == 0).then_some(slice.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each token as its text and kind. Tokens of the same kind with
    /// nothing between them come out as one.
    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, Language::Racket, &mut out);
        out.0.iter().map(|(r, k)| (&text[r.clone()], *k)).collect()
    }

    /// Like `tokens`, for a Scribble file.
    fn scribble(text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, Language::Scribble, &mut out);
        out.0.iter().map(|(r, k)| (&text[r.clone()], *k)).collect()
    }

    fn kinds_of(text: &str, kind: Kind) -> Vec<&str> {
        only(tokens(text), kind)
    }

    fn only(tokens: Vec<(&str, Kind)>, kind: Kind) -> Vec<&str> {
        tokens
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_a_definition() {
        use Kind::*;
        assert_eq!(
            tokens("#lang racket/base\n(define (f x #:y [y 1.5]) ; add\n  (+ x y \"!\"))"),
            [
                ("#lang racket/base", Attribute),
                ("(", Punctuation),
                ("(", Punctuation),
                ("#:y", Keyword),
                ("[", Punctuation),
                ("1.5", Number),
                ("])", Punctuation),
                ("; add", Comment),
                ("(", Punctuation),
                ("\"!\"", String),
                ("))", Punctuation),
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("#| a #| b |# c |# d #| e"),
            [
                ("#| a #| b |# c |#", Kind::Comment),
                ("#| e", Kind::Comment)
            ]
        );
        // `#|#` opens a comment and doesn't close it, and `#||#` is a
        // whole comment.
        assert_eq!(tokens("#|# x"), [("#|# x", Kind::Comment)]);
        assert_eq!(tokens("#||# x"), [("#||#", Kind::Comment)]);
        // Only a linefeed ends a `;` comment.
        assert_eq!(tokens("; a\rb\nc"), [("; a\rb", Kind::Comment)]);
        assert_eq!(
            tokens("#!/bin/sh\nx #! a \\\nb\nc"),
            [("#!/bin/sh", Kind::Comment), ("#! a \\\nb", Kind::Comment)]
        );
    }

    #[test]
    fn datum_comments() {
        use Kind::*;
        assert_eq!(
            tokens("#;(a (b) \"c\") 1"),
            [("#;(a (b) \"c\")", Comment), ("1", Number)]
        );
        // A `#;` inside the comment takes another datum.
        assert_eq!(tokens("#; #; a b c"), [("#; #; a b", Comment)]);
        // Prefixes and comments don't count as data.
        assert_eq!(
            tokens("#; ' ; x\n #| y |# #&z 1"),
            [
                ("#; ' ; x", Comment),
                (" #| y |# #&z", Comment),
                ("1", Number)
            ]
        );
        assert_eq!(
            tokens("'#;a 1"),
            [("'", Operator), ("#;a", Comment), ("1", Number)]
        );
        // A list that ends first ends the comment, and one that the text
        // doesn't finish keeps it going to the end.
        assert_eq!(
            tokens("(1 #;)"),
            [
                ("(", Punctuation),
                ("1", Number),
                ("#;", Comment),
                (")", Punctuation)
            ]
        );
        assert_eq!(tokens("#;(a\n(b \n"), [("#;(a", Comment), ("(b", Comment)]);
        // Whether a comment covers the whitespace at the end of a line
        // would depend on the next line, so it never does.
        assert_eq!(tokens("#;\r\n)"), [("#;", Comment), (")", Punctuation)]);
        assert_eq!(tokens("#;\r\n"), [("#;", Comment)]);
    }

    #[test]
    fn strings_and_escapes() {
        use Kind::*;
        assert_eq!(
            tokens(r#""a\n\"b" "\x41\101\u3bb\U1F600\uD83D\uDE00" "\q""#),
            [
                ("\"a", String),
                ("\\n\\\"", Escape),
                ("b\"", String),
                ("\"", String),
                ("\\x41\\101\\u3bb\\U1F600\\uD83D\\uDE00", Escape),
                ("\"", String),
                ("\"\\q\"", String),
            ]
        );
        assert_eq!(
            tokens("\"a\\\r\nb\""),
            [("\"a", String), ("\\\r\n", Escape), ("b\"", String)]
        );
        // Byte strings don't take `\u` or `\U`.
        assert_eq!(
            tokens(r#"#"a\n\u3bb""#),
            [("#\"a", String), ("\\n", Escape), ("\\u3bb\"", String)]
        );
        assert_eq!(
            kinds_of(r#"#rx"a|b" #px#"\\d+" #rx x"#, String),
            ["#rx\"a|b\"", "#px#\"", "d+\""]
        );
        assert_eq!(tokens("\"open\\"), [("\"open\\", String)]);
    }

    #[test]
    fn here_strings() {
        assert_eq!(
            tokens("#<<END\n\"a\" ; b\nEND \nEND\n1"),
            [
                ("#<<END\n\"a\" ; b\nEND \nEND", Kind::String),
                ("1", Kind::Number)
            ]
        );
        assert_eq!(
            tokens("#<<E\r\nx\r\nE\r\n1"),
            [("#<<E\r\nx\r\nE\r", Kind::String), ("1", Kind::Number)]
        );
        assert_eq!(tokens("#<<E\nE"), [("#<<E\nE", Kind::String)]);
        assert_eq!(tokens("#<<E\nx\n"), [("#<<E\nx\n", Kind::String)]);
    }

    #[test]
    fn characters() {
        assert_eq!(
            kinds_of(
                r"#\a #\space #\NewLine #\( #\; #\101 #\u3BB #\U1F600 #\λ #\ ",
                Kind::Constant
            ),
            [
                r"#\a",
                r"#\space",
                r"#\NewLine",
                r"#\(",
                r"#\;",
                r"#\101",
                r"#\u3BB",
                r"#\U1F600",
                r"#\λ",
                r"#\ ",
            ]
        );
        // A character is only as long as the reader takes it to be.
        assert_eq!(
            tokens(r"#\x41 #\uq #\a1"),
            [
                (r"#\x", Kind::Constant),
                ("41", Kind::Number),
                (r"#\u", Kind::Constant),
                (r"#\a", Kind::Constant),
                ("1", Kind::Number),
            ]
        );
    }

    #[test]
    fn constants_and_keywords() {
        assert_eq!(
            kinds_of("#t #f #true #false #T #F #tru #t1", Kind::Constant),
            ["#t", "#f", "#true", "#false", "#T", "#F"]
        );
        assert_eq!(
            kinds_of(r"#:a #:|b c| #:d\ e #:1 #: x", Kind::Keyword),
            ["#:a", "#:|b c|", r"#:d\ e", "#:1", "#:"]
        );
    }

    #[test]
    fn symbols() {
        // None of these are numbers.
        assert_eq!(
            tokens(r"abc a#b |1| 1\2 #%app ... -> 1+ + a.b e inf +inf.0x |a;b| a\;b λ"),
            []
        );
        assert_eq!(
            tokens("(a . b)"),
            [
                ("(", Kind::Punctuation),
                (".", Kind::Punctuation),
                (")", Kind::Punctuation)
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "1 -2 1/2 .5 1. 1e10 1#.# 1+2i -i +inf.0 -nan.f +inf.t 1@2 1/2+3/4i 1t0 \
                 #x1F #e1.5 #b101 #o7 #d1 #i3 #x#e1f #E#X10 #xyz",
                Kind::Number
            ),
            [
                "1", "-2", "1/2", ".5", "1.", "1e10", "1#.#", "1+2i", "-i", "+inf.0", "-nan.f",
                "+inf.t", "1@2", "1/2+3/4i", "1t0", "#x1F", "#e1.5", "#b101", "#o7", "#d1", "#i3",
                "#x#e1f", "#E#X10", "#xyz",
            ]
        );
        // `1/0` has the syntax of a number but no value, which the reader
        // reports as an error instead of reading a symbol.
        assert!(is_number("1/0"));
        for s in [
            "1/", "/2", "1//2", "2..", "1e", "1e+", ".e1", "1#2", "+", "-", "i", "1i", "1+2",
            "1+2i+3i", "1t0+1i", "1+1t0i", "1@2t0", "1@+i", "+inf.0e1", "1e2e3", "1.5/2",
        ] {
            assert!(!is_number(s), "{s}");
        }
    }

    #[test]
    fn compound_data() {
        use Kind::*;
        assert_eq!(
            kinds_of(
                "#(1) #3[1] #fl(1.0) #Fx{1} #s(p 1) #hash() #HashEqV[] #hashalw{} #hasheqeq()",
                Punctuation
            ),
            [
                "#(",
                ")",
                "#3[",
                "]",
                "#fl(",
                ")",
                "#Fx{",
                "}",
                "#s(",
                ")",
                "#hash()",
                "#HashEqV[]",
                "#hashalw{}",
                "#hasheqeq()",
            ]
        );
        assert_eq!(
            kinds_of("'a `(,b ,@c) #'d #`(#,e #,@f) #&g #0=(h . #0#)", Operator),
            [
                "'", "`", ",", ",@", "#'", "#`", "#,", "#,@", "#&", "#0=", "#0#"
            ]
        );
    }

    #[test]
    fn directives() {
        use Kind::*;
        assert_eq!(
            kinds_of(
                "#lang typed/racket\n#!r6rs\n#reader scribble/reader #cs a #CI b",
                Attribute
            ),
            ["#lang typed/racket", "#!r6rs", "#reader", "#cs", "#CI"]
        );
        // `#lang` takes exactly one space, and a name that ends with
        // whitespace and doesn't start or end with `/`.
        assert_eq!(
            kinds_of("#lang  x\n#lang /x\n#lang x/\n#lang x(", Attribute),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn lexes_a_scribble_document() {
        use Kind::*;
        assert_eq!(
            scribble(
                "#lang scribble/manual\n\
                 @title{Hello @bold{world}}\n\
                 @; a comment\n\
                 Text with (parens), 1, #t and \"quotes\".\n\
                 @racket[(define x 1)]\n"
            ),
            [
                ("#lang scribble/manual", Attribute),
                ("@title", Keyword),
                ("{", Punctuation),
                ("@bold", Keyword),
                ("{", Punctuation),
                ("}}", Punctuation),
                ("@; a comment", Comment),
                ("@racket", Keyword),
                ("[(", Punctuation),
                ("1", Number),
                (")]", Punctuation),
            ]
        );
        // Without a `#lang` line, a Scribble file is text from the start,
        // because it's a piece from the middle of a document.
        assert_eq!(
            scribble("(define x 1) @f[2]"),
            [
                ("@f", Keyword),
                ("[", Punctuation),
                ("2", Number),
                ("]", Punctuation)
            ]
        );
        // So is a Racket file in a Scribble language.
        assert_eq!(
            tokens("#lang scribble/text\n(1) @f{}"),
            [
                ("#lang scribble/text", Attribute),
                ("@f", Keyword),
                ("{}", Punctuation)
            ]
        );
        // And a Scribble file in another language isn't.
        assert_eq!(
            scribble("#lang racket\n(1) @f"),
            [
                ("#lang racket", Attribute),
                ("(", Punctuation),
                ("1", Number),
                (")", Punctuation)
            ]
        );
        // Comments can come before the `#lang` line.
        assert_eq!(
            scribble("; c\n#| d |#\n#lang racket\n(1) @f"),
            [
                ("; c", Comment),
                ("#| d |#", Comment),
                ("#lang racket", Attribute),
                ("(", Punctuation),
                ("1", Number),
                (")", Punctuation)
            ]
        );
        // But a language we don't know is read the way the file's name
        // suggests.
        assert_eq!(
            scribble("#lang my/lang\n(1) @f"),
            [("#lang my/lang", Attribute), ("@f", Keyword)]
        );
        assert_eq!(
            tokens("#lang my/lang\n(1) @f"),
            [
                ("#lang my/lang", Attribute),
                ("(", Punctuation),
                ("1", Number),
                (")", Punctuation)
            ]
        );
    }

    #[test]
    fn at_exp() {
        use Kind::*;
        assert_eq!(
            tokens("#lang at-exp racket/base\n(define s @string-append{a @b c 1})"),
            [
                ("#lang at-exp racket/base", Attribute),
                ("(", Punctuation),
                ("@string-append", Keyword),
                ("{", Punctuation),
                ("@b", Keyword),
                ("})", Punctuation),
            ]
        );
        // `@` doesn't end a symbol, so these are one symbol each, or a
        // number in polar form.
        assert_eq!(
            tokens("#lang at-exp racket\na@b x@ 1@2"),
            [("#lang at-exp racket", Attribute), ("1@2", Number)]
        );
        // Without `at-exp`, `@` starts a symbol like any other letter.
        assert_eq!(
            tokens("#lang racket\n@f{1}"),
            [
                ("#lang racket", Attribute),
                ("{", Punctuation),
                ("1", Number),
                ("}", Punctuation)
            ]
        );
    }

    #[test]
    fn at_forms() {
        use Kind::*;
        // A command, data in brackets and text in braces, each optional.
        assert_eq!(
            scribble("@f[1]{x} @[2]{y} @{z} @(g 3)[4] @\"s\""),
            [
                ("@f", Keyword),
                ("[", Punctuation),
                ("1", Number),
                ("]{", Punctuation),
                ("}", Punctuation),
                ("@", Keyword),
                ("[", Punctuation),
                ("2", Number),
                ("]{", Punctuation),
                ("}", Punctuation),
                ("@", Keyword),
                ("{", Punctuation),
                ("}", Punctuation),
                ("@", Keyword),
                ("(", Punctuation),
                ("3", Number),
                (")[", Punctuation),
                ("4", Number),
                ("]", Punctuation),
                ("@", Keyword),
                ("\"s\"", String),
            ]
        );
        // The parts must follow each other without spaces.
        assert_eq!(scribble("@f [1] {2}"), [("@f", Keyword)]);
        // Braces in the text nest, and are text.
        assert_eq!(
            scribble("@f{a {b} c} {d}"),
            [("@f", Keyword), ("{", Punctuation), ("}", Punctuation)]
        );
        // Prefixes go between the `@` and the command.
        assert_eq!(
            scribble("@'f{x} @#`(g)"),
            [
                ("@", Keyword),
                ("'", Operator),
                ("f", Keyword),
                ("{", Punctuation),
                ("}", Punctuation),
                ("@", Keyword),
                ("#`", Operator),
                ("(", Punctuation),
                (")", Punctuation),
            ]
        );
        // `|` ends the command, so `@f|{x}|` has text after it. But inside
        // parentheses `|` quotes as usual.
        assert_eq!(
            scribble("@f|{x}| @(g |a b|1)"),
            [
                ("@f", Keyword),
                ("|{", Punctuation),
                ("}|", Punctuation),
                ("@", Keyword),
                ("(", Punctuation),
                (")", Punctuation),
            ]
        );
    }

    #[test]
    fn other_delimiters() {
        use Kind::*;
        // Inside `|<{` and `}>|`, only `|<@` starts an @-form, and `{` and
        // `}` are text.
        assert_eq!(
            scribble("@f|<{a @b{1} } |<@c{2}}>| 3"),
            [
                ("@f", Keyword),
                ("|<{", Punctuation),
                ("|<@c", Keyword),
                ("{", Punctuation),
                ("}}>|", Punctuation),
            ]
        );
        // The opening delimiter nests.
        assert_eq!(
            scribble("@f|({ |({ })| })| @x"),
            [
                ("@f", Keyword),
                ("|({", Punctuation),
                ("})|", Punctuation),
                ("@x", Keyword),
            ]
        );
        // Letters can't go between `|` and `{`.
        assert_eq!(scribble("@f|a{x}"), [("@f", Keyword)]);
    }

    #[test]
    fn at_comments() {
        use Kind::*;
        assert_eq!(
            scribble("a @;{b @c{d} {e}} f @; g @h{\ni"),
            [("@;{b @c{d} {e}}", Comment), ("@; g @h{", Comment)]
        );
        assert_eq!(
            scribble("@f{@;|<{ } }>| x}"),
            [
                ("@f", Keyword),
                ("{", Punctuation),
                ("@;|<{ } }>|", Comment),
                ("}", Punctuation)
            ]
        );
        // A `#;` covers a whole @-form.
        assert_eq!(
            tokens("#lang at-exp racket\n#;@f[1]{x} 2"),
            [
                ("#lang at-exp racket", Attribute),
                ("#;@f[1]{x}", Comment),
                ("2", Number)
            ]
        );
        // In the data of an @-form, a `#;` takes the next comment as its
        // datum. Inside parentheses, it skips comments.
        let numbers = |text| only(scribble(text), Number);
        assert_eq!(numbers("@f[#; ; a\n 1 (#; ; b\n 2 3)]"), ["1", "3"]);
        assert_eq!(numbers("@f[#; @;{a} 1 2]"), ["1", "2"]);
        assert_eq!(numbers("@f[#; #; 1 2 3]"), ["2", "3"]);
        // But not after a prefix, which reads its own datum.
        assert_eq!(numbers("@f['#; ; a\n 1 2]"), ["2"]);
    }

    #[test]
    fn escapes() {
        use Kind::*;
        assert_eq!(
            scribble("@f{a @|(g 1)| b @|x 2|}"),
            [
                ("@f", Keyword),
                ("{", Punctuation),
                ("@", Keyword),
                ("|(", Punctuation),
                ("1", Number),
                (")|", Punctuation),
                ("@", Keyword),
                ("|", Punctuation),
                ("2", Number),
                ("|}", Punctuation),
            ]
        );
        // In an escape, `|` ends symbols at any depth, and after a prefix
        // it starts a symbol instead of ending the escape.
        assert_eq!(
            scribble("@|(a|1|) '|b| 2|"),
            [
                ("@", Keyword),
                ("|(", Punctuation),
                (")", Punctuation),
                ("'", Operator),
                ("2", Number),
                ("|", Punctuation),
            ]
        );
        // Outside of text, `@|{` starts text in braces. Inside text, it
        // starts an escape that holds a list in braces.
        assert_eq!(
            tokens("#lang at-exp racket\n@|{1}|"),
            [
                ("#lang at-exp racket", Attribute),
                ("@", Keyword),
                ("|{", Punctuation),
                ("}|", Punctuation),
            ]
        );
        assert_eq!(
            scribble("@|{1}|"),
            [
                ("@", Keyword),
                ("|{", Punctuation),
                ("1", Number),
                ("}|", Punctuation),
            ]
        );
    }
}
