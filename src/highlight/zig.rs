//! A lexer for Zig.
//!
//! It follows the tokenizer in Zig's standard library,
//! `lib/std/zig/tokenizer.zig`, to find where tokens start and end, but
//! it doesn't check that they're valid.
//!
//! Without a parser we can't know what an identifier names, so every
//! identifier gets a color from its name (see `Kind::Identifier`).

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
enum Token {
    // Doc comments, which start with `///` or `//!`, are tokens to Zig,
    // but they get the same color.
    #[regex(r"//[^\n]*", allow_greedy = true)]
    Comment,

    #[token("\"", |lex| quoted(lex, b'"'))]
    #[token("'", |lex| quoted(lex, b'\''))]
    Str,

    // Each line of a multiline string starts with `\\`. It has no escapes.
    #[regex(r"\\\\[^\n]*", allow_greedy = true)]
    MultilineStr,

    // An identifier such as `@"with spaces"`, which is never a keyword.
    #[token("@\"", |lex| quoted(lex, b'"'))]
    QuotedIdent,

    // A builtin function such as `@import`.
    #[regex(r"@[a-zA-Z_][a-zA-Z0-9_]*")]
    Builtin,

    // The tokenizer takes in any letters, digits and underscores after
    // the first digit, along with a sign after an exponent's letter and
    // one period. The compiler checks the number later. A period must
    // have a letter or digit after it, so `0..2` is `0`, `..` and `2`.
    #[regex(r"[0-9]([0-9a-zA-Z_]|[eEpP][+-])*(\.([0-9a-zA-Z_]|[eEpP][+-])+)?")]
    Number,

    #[regex(r"[!=%^/&<>]=?")]
    #[regex(r"\|[|=]?")]
    #[regex(r"[+*-][%|]?=?")]
    #[regex(r"<<\|?=?|>>=?")]
    #[regex(r"=>|->|\+\+|\.\*|\.\.\.?|[?~]")]
    Operator,

    #[regex(r"[()\[\]{},;:.]")]
    Punctuation,

    // Identifiers are ASCII only.
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")]
    Ident,

    // Logos prefers the longest match, so a name that starts with a
    // keyword is still an identifier.
    #[token("addrspace")]
    #[token("align")]
    #[token("allowzero")]
    #[token("and")]
    #[token("anyframe")]
    #[token("anytype")]
    #[token("asm")]
    #[token("break")]
    #[token("callconv")]
    #[token("catch")]
    #[token("comptime")]
    #[token("const")]
    #[token("continue")]
    #[token("defer")]
    #[token("else")]
    #[token("enum")]
    #[token("errdefer")]
    #[token("error")]
    #[token("export")]
    #[token("extern")]
    #[token("fn")]
    #[token("for")]
    #[token("if")]
    #[token("inline")]
    #[token("noalias")]
    #[token("noinline")]
    #[token("nosuspend")]
    #[token("opaque")]
    #[token("or")]
    #[token("orelse")]
    #[token("packed")]
    #[token("pub")]
    #[token("resume")]
    #[token("return")]
    #[token("linksection")]
    #[token("struct")]
    #[token("suspend")]
    #[token("switch")]
    #[token("test")]
    #[token("threadlocal")]
    #[token("try")]
    #[token("union")]
    #[token("unreachable")]
    #[token("var")]
    #[token("volatile")]
    #[token("while")]
    Keyword,

    // These are identifiers to the tokenizer, but no declaration can
    // have these names.
    #[token("true")]
    #[token("false")]
    #[token("null")]
    #[token("undefined")]
    Constant,
}

/// Skip to the closing `quote`. Strings can't go on to the next line, so
/// one without its closing quote ends with its line.
fn quoted(lex: &mut Lexer<Token>, quote: u8) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\n' => break,
            b'\\' if rest.get(i + 1) == Some(&b'\n') => i += 1,
            b'\\' => i += 2,
            b if b == quote => {
                i += 1;
                break;
            }
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    lex.bump(i.min(rest.len()));
}

/// The pieces of a string or character literal. Anything else in one,
/// such as a backslash before a character that can't be escaped, is
/// plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r#"\\([nrt\\'"]|x[0-9a-fA-F][0-9a-fA-F]|u\{[0-9a-fA-F]+\})"#)]
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
            Ok(Token::MultilineStr) => Kind::String,
            Ok(Token::QuotedIdent | Token::Ident) => Kind::Identifier,
            // Builtins are part of the language, and nobody can define
            // new ones, so they get the keyword color.
            Ok(Token::Builtin | Token::Keyword) => Kind::Keyword,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Constant) => Kind::Constant,
            Err(()) => continue,
        };
        out.push(span, kind);
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
        let text = "pub fn f(x: ?*const u8) !void { return x.?.* orelse error.Nope; }";
        assert_eq!(
            tokens(text),
            [
                ("pub", Keyword),
                ("fn", Keyword),
                ("(", Punctuation),
                (":", Punctuation),
                ("?*", Operator),
                ("const", Keyword),
                (")", Punctuation),
                ("!", Operator),
                ("{", Punctuation),
                ("return", Keyword),
                (".", Punctuation),
                ("?.*", Operator),
                ("orelse", Keyword),
                ("error", Keyword),
                (".", Punctuation),
                (";", Punctuation),
                ("}", Punctuation),
            ]
        );
        assert_eq!(identifiers(text), ["f", "x", "u8", "void", "x", "Nope"]);
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("x // y \"z\n/// doc\n//! top\n//// four\ny"),
            [
                ("// y \"z", Kind::Comment),
                ("/// doc", Kind::Comment),
                ("//! top", Kind::Comment),
                ("//// four", Kind::Comment),
            ]
        );
        // Zig has no block comments.
        assert_eq!(kinds_of("a /* b */", Kind::Comment), [] as [&str; 0]);
        assert_eq!(kinds_of("a /= b", Kind::Operator), ["/="]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#""a\n\"b" "\x7f\u{1F600}" "\q\u{}\x1""#),
            [
                ("\"a", Kind::String),
                ("\\n\\\"", Kind::Escape),
                ("b\"", Kind::String),
                ("\"", Kind::String),
                ("\\x7f\\u{1F600}", Kind::Escape),
                ("\"", Kind::String),
                ("\"\\q\\u{}\\x1\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens(r"'a' '\'' '\\' '中'"),
            [
                ("'a'", Kind::String),
                ("'", Kind::String),
                ("\\'", Kind::Escape),
                ("'", Kind::String),
                ("'", Kind::String),
                ("\\\\", Kind::Escape),
                ("'", Kind::String),
                ("'中'", Kind::String),
            ]
        );
        // A string without its closing quote ends with its line, even
        // after a backslash.
        assert_eq!(
            tokens("\"open\nx = 'a\\\ny\""),
            [
                ("\"open", Kind::String),
                ("=", Kind::Operator),
                ("'a\\", Kind::String),
                ("\"", Kind::String),
            ]
        );
        assert_eq!(tokens("\"open\\"), [("\"open\\", Kind::String)]);
    }

    #[test]
    fn multiline_strings() {
        assert_eq!(
            tokens("const s =\n    \\\\a \"b\\n // c\n    \\\\\n;"),
            [
                ("const", Kind::Keyword),
                ("=", Kind::Operator),
                ("\\\\a \"b\\n // c", Kind::String),
                ("\\\\", Kind::String),
                (";", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn builtins_and_quoted_identifiers() {
        assert_eq!(
            tokens(r#"@import("std") @as(u8, x) @"a b"(@"\"")"#),
            [
                ("@import", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("\"std\"", Kind::String),
                (")", Kind::Punctuation),
                ("@as", Kind::Keyword),
                ("(", Kind::Punctuation),
                (",", Kind::Punctuation),
                (")", Kind::Punctuation),
                ("(", Kind::Punctuation),
                (")", Kind::Punctuation),
            ]
        );
        assert_eq!(
            identifiers(r#"@"a b" @"fn" @"\"" @"open"#),
            [r#"@"a b""#, r#"@"fn""#, r#"@"\"""#, r#"@"open"#]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "0x1F 0o17 0b1010_1010 1_000 1.5 2e10 3.0E-5 0x1.8p+3 0x1p-2 1..2 x.0 0..",
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
                "0x1.8p+3",
                "0x1p-2",
                "1",
                "2",
                "0",
                "0"
            ]
        );
        // The tokenizer leaves checking numbers to the compiler, so it
        // reads all of these as one number each.
        assert_eq!(
            kinds_of("1abc 0xe+1 1.2e+3x 1._", Kind::Number),
            ["1abc", "0xe+1", "1.2e+3x", "1._"]
        );
        assert_eq!(
            tokens("1.2.3 1.+2"),
            [
                ("1.2", Kind::Number),
                (".", Kind::Punctuation),
                ("3", Kind::Number),
                ("1", Kind::Number),
                (".", Kind::Punctuation),
                ("+", Kind::Operator),
                ("2", Kind::Number),
            ]
        );
    }

    #[test]
    fn keywords_and_constants() {
        assert_eq!(
            tokens("const constant fnord true undefined nullable null comptime_int"),
            [
                ("const", Kind::Keyword),
                ("true", Kind::Constant),
                ("undefined", Kind::Constant),
                ("null", Kind::Constant),
            ]
        );
        // These were keywords in older versions of Zig, and are names now.
        assert_eq!(
            identifiers("group.async(io, f); future.await(io); usingnamespace"),
            [
                "group",
                "async",
                "io",
                "f",
                "future",
                "await",
                "io",
                "usingnamespace"
            ]
        );
        // Identifiers are ASCII only, so other characters don't hide a
        // keyword after them.
        assert_eq!(kinds_of("éfn x·fn", Kind::Keyword), ["fn", "fn"]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds_of(
                "a +%= b; c <<|= d; e -| f; g ** h ++ i; j => k; l -> m; n... o..p",
                Kind::Operator
            ),
            ["+%=", "<<|=", "-|", "**", "++", "=>", "->", "...", ".."]
        );
        assert_eq!(
            tokens("x.y .{ .a = 1 }"),
            [
                (".", Kind::Punctuation),
                (".{", Kind::Punctuation),
                (".", Kind::Punctuation),
                ("=", Kind::Operator),
                ("1", Kind::Number),
                ("}", Kind::Punctuation),
            ]
        );
    }
}
