//! A lexer for Rust.
//!
//! It follows the Rust reference closely enough to find where comments,
//! strings and other tokens start and end, but it doesn't check that
//! they're valid. Identifiers are plain unless they're keywords.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n\f]+")]
// An identifier or keyword, as the reference defines it with Unicode's
// XID_Start and XID_Continue.
#[logos(subpattern ident = r"[\p{XID_Start}_]\p{XID_Continue}*")]
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

    #[regex(r"'(?&ident)")]
    #[regex(r"'r#(?&ident)")]
    Lifetime,

    #[regex(r"0x[0-9a-fA-F_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0o[0-7_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0b[01_]+([iu](8|16|32|64|128|size))?")]
    #[regex(
        r"[0-9][0-9_]*(\.[0-9][0-9_]*)?([eE][+-]?[0-9_]*[0-9][0-9_]*)?([iu](8|16|32|64|128|size)|f32|f64)?"
    )]
    Number,

    // Only the start of an attribute. Finding its end would take a parser,
    // since it can hold anything.
    #[regex(r"#!?\[")]
    Attribute,

    #[regex(r"[-+*/%^!&|<>=]=?")]
    #[regex(r"&&|\|\||<<=?|>>=?|\.\.[.=]?|->|=>|[@~?]")]
    Operator,

    #[regex(r"[()\[\]{},;:.#$]|::")]
    Punctuation,

    #[regex(r"(?&ident)")]
    // A raw identifier such as `r#type` is never a keyword.
    #[regex(r"r#(?&ident)")]
    Ident,

    // The strict keywords, except reserved ones that nobody uses yet. Logos
    // prefers the longest match, so a name that starts with a keyword is
    // still an identifier.
    #[token("Self")]
    #[token("as")]
    #[token("async")]
    #[token("await")]
    #[token("break")]
    #[token("const")]
    #[token("continue")]
    #[token("crate")]
    #[token("dyn")]
    #[token("else")]
    #[token("enum")]
    #[token("extern")]
    #[token("fn")]
    #[token("for")]
    #[token("if")]
    #[token("impl")]
    #[token("in")]
    #[token("let")]
    #[token("loop")]
    #[token("match")]
    #[token("mod")]
    #[token("move")]
    #[token("mut")]
    #[token("pub")]
    #[token("ref")]
    #[token("return")]
    #[token("self")]
    #[token("static")]
    #[token("struct")]
    #[token("super")]
    #[token("trait")]
    #[token("type")]
    #[token("unsafe")]
    #[token("use")]
    #[token("where")]
    #[token("while")]
    Keyword,

    #[token("true")]
    #[token("false")]
    Bool,
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
            Ok(Token::RawStr) => Kind::String,
            Ok(Token::Lifetime) => Kind::Lifetime,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Attribute) => Kind::Attribute,
            Ok(Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Keyword) => Kind::Keyword,
            Ok(Token::Bool) => Kind::Constant,
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
                ("<", Operator),
                ("'a", Lifetime),
                (">", Operator),
                ("(", Punctuation),
                (":", Punctuation),
                ("&", Operator),
                ("'a", Lifetime),
                (")", Punctuation),
                ("->", Operator),
                ("<", Operator),
                (">", Operator),
                ("{", Punctuation),
                (".", Punctuation),
                ("()", Punctuation),
                ("as", Keyword),
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
        assert_eq!(kinds_of("'été 'é'", Kind::Lifetime), ["'été"]);
        assert_eq!(kinds_of("'r#fn: loop {}", Kind::Lifetime), ["'r#fn"]);
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
                ("(", Kind::Punctuation),
                ("2", Kind::Number),
                (")", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            tokens("self Self MAX Foo u8 r#match fnord naïvefn x true false"),
            [
                ("self", Kind::Keyword),
                ("Self", Kind::Keyword),
                ("true", Kind::Constant),
                ("false", Kind::Constant),
            ]
        );
        assert_eq!(kinds_of("Größe x·fn _fn _ fn", Kind::Keyword), ["fn"]);
        // Characters that can't be part of an identifier are errors, so
        // they don't hide a keyword after them. A combining mark can
        // continue an identifier but not start one.
        assert_eq!(
            kinds_of("🦀fn x→fn \u{301}fn", Kind::Keyword),
            ["fn", "fn", "fn"]
        );
        assert_eq!(
            tokens("println!(x)"),
            [
                ("!", Kind::Operator),
                ("(", Kind::Punctuation),
                (")", Kind::Punctuation)
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
            ["#[", "#![", "#[", "#["]
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
