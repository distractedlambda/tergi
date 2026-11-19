//! A lexer for Rust.
//!
//! It follows the Rust reference closely enough to find where comments,
//! strings and other tokens start and end, but it doesn't check that
//! they're valid. Identifiers are only ASCII. Other characters outside
//! strings and comments are left plain.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n\f]+")]
enum Token {
    #[regex(r"//[^\n]*", allow_greedy = true)]
    #[token("/*", block_comment)]
    Comment,

    // Strings and characters that can hold escapes.
    #[regex(r#"[bc]?""#, quoted)]
    #[regex(r#"b?'([^'\\\n\r\t]|\\([nrt\\0'"]|x[0-9a-fA-F][0-9a-fA-F]|u\{[0-9a-fA-F_]+\}))'"#)]
    Str,

    #[regex(r##"[bc]?r#*""##, raw_string)]
    RawStr,

    #[regex(r"'[a-zA-Z_][a-zA-Z0-9_]*")]
    Lifetime,

    #[regex(r"0x[0-9a-fA-F_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0o[0-7_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0b[01_]+([iu](8|16|32|64|128|size))?")]
    #[regex(
        r"[0-9][0-9_]*(\.[0-9][0-9_]*)?([eE][+-]?[0-9_]*[0-9][0-9_]*)?([iu](8|16|32|64|128|size)|f32|f64)?"
    )]
    Number,

    // Only the start of an attribute, up to its path, such as `#[derive`.
    // Finding its end would take a parser, since it can hold anything.
    #[regex(r"#!?\[[ \t]*[a-zA-Z_][a-zA-Z0-9_]*(::[a-zA-Z_][a-zA-Z0-9_]*)*")]
    Attribute,

    #[regex(r"[-+*/%^!&|<>=]=?")]
    #[regex(r"&&|\|\||<<=?|>>=?|\.\.[.=]?|->|=>|[@~?]")]
    Operator,

    #[regex(r"[()\[\]{},;:.#$]|::")]
    Punctuation,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", word)]
    #[regex(r"r#[a-zA-Z_][a-zA-Z0-9_]*", raw_word)]
    Ident,

    /// `fn` and the name after it. A generic function's name isn't
    /// followed by `(`, so `classify` can't tell it's a function.
    #[regex(r"fn[ \t\r\n]+(r#)?[a-zA-Z_][a-zA-Z0-9_]*")]
    FnName,

    // `word` turns identifiers into these.
    Keyword,
    Constant,
    Type,
    Function,
    Macro,
}

/// Skip to the end of a block comment. Block comments nest.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut depth = 1;
    let mut i = 0;
    while i < rest.len() {
        match (rest[i], rest.get(i + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    lex.bump(i);
}

/// Skip to the closing quote. A string without one runs to the end.
fn quoted(lex: &mut Lexer<Token>) {
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

/// Skip to a quote followed by as many `#`s as the string started with.
fn raw_string(lex: &mut Lexer<Token>) {
    let hashes = lex.slice().bytes().filter(|&b| b == b'#').count();
    let close = format!("\"{}", "#".repeat(hashes));
    let rest = lex.remainder();
    lex.bump(rest.find(&close).map_or(rest.len(), |i| i + close.len()));
}

fn word(lex: &mut Lexer<Token>) -> Token {
    let word = lex.slice();
    if KEYWORDS.contains(&word) {
        Token::Keyword
    } else if word == "true" || word == "false" {
        Token::Constant
    } else {
        classify(word, lex.remainder())
    }
}

/// A raw identifier such as `r#type` is never a keyword.
fn raw_word(lex: &mut Lexer<Token>) -> Token {
    classify(&lex.slice()[2..], lex.remainder())
}

/// Tell what an identifier that isn't a keyword is, from how it's spelled
/// and what comes right after it.
fn classify(word: &str, after: &str) -> Token {
    if TYPES.contains(&word) {
        return Token::Type;
    }
    if after.starts_with('!') && !after.starts_with("!=") {
        return Token::Macro;
    }
    if word.starts_with(|c: char| c.is_ascii_uppercase()) {
        // `T` is a type parameter, while `MAX` and `MAX_LEN` are constants.
        return if word.len() > 1 && is_upper(word) {
            Token::Constant
        } else {
            Token::Type
        };
    }
    if after.starts_with('(') || after.starts_with("::<") {
        return Token::Function;
    }
    Token::Ident
}

fn is_upper(word: &str) -> bool {
    !word.bytes().any(|b| b.is_ascii_lowercase())
}

/// The strict keywords, which can't be identifiers, except reserved ones
/// that nobody uses yet.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "type", "unsafe", "use", "where",
    "while",
];

const TYPES: &[&str] = &[
    "Self", "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
    "u16", "u32", "u64", "u128", "usize",
];

/// The pieces of a string literal. Anything else in one, such as a
/// backslash before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r#"\\([nrt\\0'"]|x[0-9a-fA-F][0-9a-fA-F]|u\{[0-9a-fA-F_]*\}|\r?\n)"#)]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    while let Some(token) = lex.next() {
        let span = lex.span();
        let kind = match token {
            Ok(Token::Comment) => Kind::Comment,
            Ok(Token::Str) => {
                for (part, r) in Part::lexer(lex.slice()).spanned() {
                    let kind = match part {
                        Ok(Part::Escape) => Kind::Escape,
                        _ => Kind::String,
                    };
                    out.push(span.start + r.start..span.start + r.end, kind);
                }
                continue;
            }
            Ok(Token::FnName) => {
                let name = lex.slice().trim_start_matches("fn").trim_start();
                out.push(span.start..span.start + 2, Kind::Keyword);
                out.push(span.end - name.len()..span.end, Kind::Function);
                continue;
            }
            Ok(Token::RawStr) => Kind::String,
            Ok(Token::Lifetime) => Kind::Lifetime,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Attribute) => Kind::Attribute,
            Ok(Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Keyword) => Kind::Keyword,
            Ok(Token::Constant) => Kind::Constant,
            Ok(Token::Type) => Kind::Type,
            Ok(Token::Function) => Kind::Function,
            Ok(Token::Macro) => Kind::Macro,
            Ok(Token::Ident) | Err(()) => continue,
        };
        out.push(span, kind);
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
    fn lexes_a_function() {
        use Kind::*;
        assert_eq!(
            tokens("pub fn f<'a>(x: &'a str) -> Vec<u8> { x.len() as u8 }"),
            [
                ("pub", Keyword),
                ("fn", Keyword),
                ("f", Function),
                ("<", Operator),
                ("'a", Lifetime),
                (">", Operator),
                ("(", Punctuation),
                (":", Punctuation),
                ("&", Operator),
                ("'a", Lifetime),
                ("str", Type),
                (")", Punctuation),
                ("->", Operator),
                ("Vec", Type),
                ("<", Operator),
                ("u8", Type),
                (">", Operator),
                ("{", Punctuation),
                (".", Punctuation),
                ("len", Function),
                ("()", Punctuation),
                ("as", Keyword),
                ("u8", Type),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn comments_nest() {
        assert_eq!(
            tokens("/* a /* b */ c */ d /* e"),
            [
                ("/* a /* b */ c */", Kind::Comment),
                ("/* e", Kind::Comment)
            ]
        );
        assert_eq!(
            tokens("x // y \"z\n/// doc\nw"),
            [("// y \"z", Kind::Comment), ("/// doc", Kind::Comment)]
        );
        assert_eq!(tokens("/*/ x"), [("/*/ x", Kind::Comment)]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#""a\n\"b" b"\x7f" c"\u{1F600}""#),
            [
                ("\"a", Kind::String),
                ("\\n\\\"", Kind::Escape),
                ("b\"", Kind::String),
                ("b\"", Kind::String),
                ("\\x7f", Kind::Escape),
                ("\"", Kind::String),
                ("c\"", Kind::String),
                ("\\u{1F600}", Kind::Escape),
                ("\"", Kind::String),
            ]
        );
        // A backslash before a newline skips it, and one before anything
        // else that can't be escaped is just part of the string.
        assert_eq!(
            tokens("\"a\\\n  b\\q\""),
            [
                ("\"a", Kind::String),
                ("\\\n", Kind::Escape),
                ("  b\\q\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens("\"multi\nline\" x"),
            [("\"multi\nline\"", Kind::String)]
        );
        assert_eq!(tokens("\"open\\"), [("\"open\\", Kind::String)]);
    }

    #[test]
    fn raw_strings() {
        assert_eq!(
            tokens(r###"r"a\n" br#"b"c"# cr##"d"#"## r#"open"###),
            [
                (r#"r"a\n""#, Kind::String),
                (r###"br#"b"c"#"###, Kind::String),
                (r###"cr##"d"#"##"###, Kind::String),
                (r##"r#"open"##, Kind::String),
            ]
        );
    }

    #[test]
    fn characters_and_lifetimes() {
        assert_eq!(
            tokens(r"'a' 'a '\'' b'\\' '中' 'static '_"),
            [
                ("'a'", Kind::String),
                ("'a", Kind::Lifetime),
                ("'", Kind::String),
                ("\\'", Kind::Escape),
                ("'", Kind::String),
                ("b'", Kind::String),
                ("\\\\", Kind::Escape),
                ("'", Kind::String),
                ("'中'", Kind::String),
                ("'static", Kind::Lifetime),
                ("'_", Kind::Lifetime),
            ]
        );
        assert_eq!(kinds_of("x: &'a T", Kind::Lifetime), ["'a"]);
        assert_eq!(kinds_of("'outer: loop {}", Kind::Lifetime), ["'outer"]);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "0x1F 0xffu8 0o17 0b1010_1010 1_000 1.5 2e10 3.0E-5f64 7usize 1..2 x.0",
                Kind::Number
            ),
            [
                "0x1F",
                "0xffu8",
                "0o17",
                "0b1010_1010",
                "1_000",
                "1.5",
                "2e10",
                "3.0E-5f64",
                "7usize",
                "1",
                "2",
                "0"
            ]
        );
        assert_eq!(
            tokens("1.max(2)"),
            [
                ("1", Kind::Number),
                (".", Kind::Punctuation),
                ("max", Kind::Function),
                ("(", Kind::Punctuation),
                ("2", Kind::Number),
                (")", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            tokens("self Self MAX T Foo r#match r#Type x"),
            [
                ("self", Kind::Keyword),
                ("Self", Kind::Type),
                ("MAX", Kind::Constant),
                ("T", Kind::Type),
                ("Foo", Kind::Type),
                ("r#Type", Kind::Type),
            ]
        );
        assert_eq!(
            kinds_of("println!(x) x != y x!=y", Kind::Macro),
            ["println"]
        );
        assert_eq!(
            kinds_of("a.collect::<Vec<_>>() f (x) Some(x)", Kind::Function),
            ["collect"]
        );
        assert_eq!(kinds_of("true false", Kind::Constant), ["true", "false"]);
        assert_eq!(kinds_of("x.unwrap_or(y)", Kind::Function), ["unwrap_or"]);
        assert_eq!(kinds_of("macro_rules! m", Kind::Macro), ["macro_rules"]);
        assert_eq!(
            tokens("fn\n  r#try<T> fn(u8) fnord"),
            [
                ("fn", Kind::Keyword),
                ("r#try", Kind::Function),
                ("<", Kind::Operator),
                ("T", Kind::Type),
                (">", Kind::Operator),
                ("fn", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("u8", Kind::Type),
                (")", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn attributes() {
        assert_eq!(
            kinds_of(
                "#[derive(Debug)] #![allow(x)] #[rustfmt::skip] #[ cfg(test)]",
                Kind::Attribute
            ),
            ["#[derive", "#![allow", "#[rustfmt::skip", "#[ cfg"]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds_of(
                "a += b && c || !d; x..=y; e >>= 1; f -> g => h?",
                Kind::Operator
            ),
            ["+=", "&&", "||", "!", "..=", ">>=", "->", "=>", "?"]
        );
    }
}
