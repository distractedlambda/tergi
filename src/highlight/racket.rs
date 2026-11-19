//! A lexer for Racket.
//!
//! It follows what Racket's reader does with the default readtable. The
//! reader's source in `racket/src/expander/read` is the best reference,
//! because the reference manual leaves out a few details. For example, a
//! `;` comment only ends at a linefeed. Symbols are plain, even ones like
//! `define`, because only the expander knows what they mean.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
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
    // symbol or a number, or `.` on its own.
    #[regex(r#"[^\s\u{FEFF}()\[\]{}",'`;#]"#, atom)]
    Atom,

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
    lex.remainder().chars().next().is_none_or(is_delimiter)
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
    let mut quoted = false;
    let mut chars = text[start..].char_indices();
    let mut end = text.len();
    while let Some((i, c)) = chars.next() {
        if quoted {
            quoted = c != '|';
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
fn lang(lex: &mut Lexer<Token>) -> bool {
    let name = lex.slice().trim_start_matches("#lang ");
    let name = name.strip_prefix("#!").unwrap_or(name);
    !name.starts_with('/')
        && !name.ends_with('/')
        && lex
            .remainder()
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
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

/// A `#;` comment, which runs to the end of the datum after it.
struct DatumComment {
    start: usize,
    end: usize,
    /// How deep we are in the datum being commented out.
    depth: usize,
    /// How many more data the comment needs. A `#;` inside the comment
    /// takes the datum after it, so the comment needs one more.
    needs: usize,
}

impl DatumComment {
    /// Take the next token into the comment. Returns false when the token
    /// closes a list that the comment started in, which ends the comment
    /// early. The reader would complain about that.
    fn take(&mut self, token: Result<Token, ()>, end: usize) -> bool {
        match token {
            Ok(Token::Open) => self.depth += 1,
            Ok(Token::Close) => {
                if self.depth == 0 {
                    return false;
                }
                self.depth -= 1;
                if self.depth == 0 {
                    self.needs -= 1;
                }
            }
            Ok(Token::Comment | Token::Prefix | Token::Directive) => {}
            Ok(Token::DatumComment) => {
                if self.depth == 0 {
                    self.needs += 1;
                }
            }
            _ => {
                if self.depth == 0 {
                    self.needs -= 1;
                }
            }
        }
        self.end = end;
        true
    }
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    let mut comment: Option<DatumComment> = None;
    while let Some(token) = lex.next() {
        let span = lex.span();
        if let Some(c) = &mut comment {
            let taken = c.take(token, span.end);
            if taken && c.needs > 0 {
                continue;
            }
            out.push(c.start..c.end, Kind::Comment);
            comment = None;
            if taken {
                continue;
            }
        }
        let kind = match token {
            Ok(Token::Comment) => Kind::Comment,
            Ok(Token::DatumComment) => {
                comment = Some(DatumComment {
                    start: span.start,
                    end: span.end,
                    depth: 0,
                    needs: 1,
                });
                continue;
            }
            Ok(Token::Str) => {
                lex_string(lex.slice(), span.start, false, out);
                continue;
            }
            Ok(Token::Bytes) => {
                lex_string(lex.slice(), span.start, true, out);
                continue;
            }
            Ok(Token::HereStr) => Kind::String,
            Ok(Token::Char | Token::Bool) => Kind::Constant,
            // Racket's keywords, such as `#:when`, are data and not
            // reserved words. But they're called keywords and are used
            // much like keywords in other languages.
            Ok(Token::Keyword) => Kind::Keyword,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Atom) => {
                let s = lex.slice();
                if s.contains(['|', '\\']) {
                    continue;
                } else if s == "." {
                    Kind::Punctuation
                } else if is_number(s) {
                    Kind::Number
                } else {
                    continue;
                }
            }
            Ok(Token::Open | Token::Close) => Kind::Punctuation,
            Ok(Token::Prefix | Token::GraphRef) => Kind::Operator,
            Ok(Token::Directive | Token::Lang) => Kind::Attribute,
            Ok(Token::Symbol) | Err(()) => continue,
        };
        out.push(span, kind);
    }
    // A comment that the text doesn't finish runs to its end, like an
    // unfinished block comment. Then a hunk lexed on its own gets the same
    // colors as it does in the whole file, where the comment goes on.
    if let Some(c) = comment {
        out.push(c.start..text.len(), Kind::Comment);
    }
}

#[cfg(test)]
mod tests {
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
            [("#; ' ; x\n #| y |# #&z", Comment), ("1", Number)]
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
        assert_eq!(tokens("#;(a\n(b \n"), [("#;(a\n(b \n", Comment)]);
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
}
