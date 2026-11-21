//! A lexer for LLVM IR, the text form that `.ll` files hold.
//!
//! It follows LLVM's own lexer, `llvm/lib/AsmParser/LLLexer.cpp`, to find
//! where tokens start and end. Values have names with a sigil in front,
//! such as `%x` and `@f`, and those are the identifiers. Bare words are
//! keywords, types or constants. LLVM rejects any other bare word, so
//! we don't list the keywords, which change with each release.

use std::ops::Range;

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n\x00]+")]
#[logos(subpattern name = r"[-a-zA-Z$._][-a-zA-Z$._0-9]*")]
enum Token {
    #[regex(r";[^\n\r]*", allow_greedy = true)]
    Comment,

    #[token("/*", block_comment)]
    BlockComment,

    // A string reads up to the next quote, even on another line. A
    // backslash doesn't stop a quote from ending it.
    #[token("\"", quoted)]
    Str,

    // A local value or a basic block.
    #[regex(r"%((?&name)|[0-9]+)")]
    #[token("%\"", quoted)]
    Local,

    // A global value, a comdat, an attribute group or a summary entry.
    #[regex(r"@((?&name)|[0-9]+)")]
    #[token("@\"", quoted)]
    #[regex(r"\$(?&name)")]
    #[token("$\"", quoted)]
    #[regex(r"#[0-9]+")]
    #[regex(r"\^[0-9]+")]
    // To LLVM, `!0` is a `!` followed by a number, but it names a node
    // the way `!foo` does.
    #[regex(r"![0-9]+")]
    Global,

    // A metadata name such as `!dbg`. Backslashes in it start escapes.
    #[regex(r"![-a-zA-Z$._\\][-a-zA-Z$._0-9\\]*")]
    Metadata,

    // Any run of these characters followed by a colon is a label, even
    // one that starts with a digit or `-`.
    #[regex(r"[-a-zA-Z$._0-9]+:")]
    Label,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")]
    Word,

    #[regex(r"-?[0-9]+")]
    #[regex(r"[-+]?[0-9]+\.[0-9]*([eE][-+]?[0-9]+)?")]
    // A float in hexadecimal, with the bits of the value. A letter
    // after `0x`, such as the `K` in `0xK4000...`, gives its type.
    #[regex(r"0x[KLMHR]?[0-9A-Fa-f]+")]
    #[regex(r"[-+]?0x[0-9A-Fa-f]+\.[0-9A-Fa-f]*[pP][-+]?[0-9]*")]
    #[regex(r"[-+](inf|qnan)")]
    // LLVM reads a NaN's payload up to the `)`, even on another line.
    // We stop at the end of the line, so that a line's colors don't
    // depend on the lines after it.
    #[regex(r"[-+]s?nan\(0x[^)\n]*\)")]
    Number,

    #[regex(r"[=*|]")]
    Operator,

    #[regex(r"[()\[\]{}<>,:!#]|\.\.\.")]
    Punctuation,
}

/// Skip to the end of a `/*` comment, or the end of the text.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(rest.find("*/").map_or(rest.len(), |i| i + 2));
}

/// Skip to just after the next quote, or to the end of the text.
fn quoted(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(rest.find('"').map_or(rest.len(), |i| i + 1));
}

/// The pieces of a string. LLVM only knows two escapes, `\\` and a
/// backslash followed by two hex digits. Anything else is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r"\\(\\|[0-9a-fA-F][0-9a-fA-F])")]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

pub fn lex(text: &str, out: &mut Tokens) {
    lex_range(text, 0..text.len(), out);
}

/// Lex the part `range` of `text`. A label's color depends on the rest of
/// its line, so the lexer needs the whole text.
fn lex_range(text: &str, range: Range<usize>, out: &mut Tokens) {
    let base = range.start;
    let end = range.end;
    let mut lex = Token::lexer(&text[range]);
    // How many parentheses are open. Fields are always inside them, and
    // basic blocks never are.
    let mut parens = 0usize;
    while let Some(token) = lex.next() {
        let span = base + lex.span().start..base + lex.span().end;
        let kind = match token {
            Ok(Token::Comment) => Kind::Comment,
            Ok(Token::BlockComment) => {
                out.push_comment(text, span);
                continue;
            }
            // A quoted label. The colon is part of the token. LLVM allows
            // a label that goes on over lines, but it never prints one,
            // and the color of its first line would depend on the rest.
            Ok(Token::Str)
                if text[span.end..end].starts_with(':') && !text[span.clone()].contains('\n') =>
            {
                let colon = span.end;
                lex.bump(1);
                label(text, span.start..colon, parens > 0, out);
                continue;
            }
            Ok(Token::Str) => {
                string(text, span, out);
                continue;
            }
            // A basic block is a local value. Its label leaves out the
            // `%`, so the `%` doesn't get the identifier's color, and a
            // label and the uses of its block get the same color.
            Ok(Token::Local) => {
                out.push(span.start + 1..span.end, Kind::Identifier);
                continue;
            }
            Ok(Token::Global) => Kind::Identifier,
            // A node such as `!DILocation(line: 1)`. Only LLVM defines
            // these, so they are part of the language like keywords.
            Ok(Token::Metadata) if lex.remainder().starts_with('(') => Kind::Keyword,
            Ok(Token::Metadata) => Kind::Identifier,
            Ok(Token::Label) => {
                label(text, span.start..span.end - 1, parens > 0, out);
                continue;
            }
            Ok(Token::Word) => {
                word(text, span, out);
                continue;
            }
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => {
                match lex.slice() {
                    "(" => parens += 1,
                    ")" => parens = parens.saturating_sub(1),
                    _ => {}
                }
                Kind::Punctuation
            }
            Err(()) => continue,
        };
        out.push(span, kind);
    }
}

/// Color a string, and the escapes in it.
fn string(text: &str, span: Range<usize>, out: &mut Tokens) {
    for (part, r) in Part::lexer(&text[span.clone()]).spanned() {
        let kind = match part {
            Ok(Part::Escape) => Kind::Escape,
            _ => Kind::String,
        };
        out.push(span.start + r.start..span.start + r.end, kind);
    }
}

/// Color a label, whose name is `name` and which has a colon right after
/// it.
///
/// To LLVM, any name followed by a colon is a label. A basic block's label
/// starts its line. Elsewhere, it's the name of a field, as in
/// `!DILocation(line: 1)` or `memory(argmem: read)`. LLVM reads the name
/// in `memory` as a keyword, so we give a field the colors its name would
/// have without the colon.
///
/// A field can start its line too, when a node goes on over several
/// lines. But then it's inside parentheses, and a value follows it. A
/// hunk that we lex on its own may not have the line with the opening
/// parenthesis, so a label with only a comment after it is a basic block
/// even inside parentheses.
fn label(text: &str, name: Range<usize>, in_parens: bool, out: &mut Tokens) {
    let colon = name.end;
    let before = &text[..name.start];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let after = &text[colon + 1..];
    let rest = &after[..after.find('\n').unwrap_or(after.len())];
    let rest = rest.trim();
    if before[line_start..]
        .trim_start_matches([' ', '\t'])
        .is_empty()
        && (rest.is_empty() || rest.starts_with(';') || !in_parens)
    {
        out.push(name, Kind::Identifier);
    } else {
        lex_range(text, name, out);
    }
    out.push(colon..colon + 1, Kind::Punctuation);
}

/// Color a bare word, which is a keyword, a type, a constant or a number.
fn word(text: &str, span: Range<usize>, out: &mut Tokens) {
    let w = &text[span.clone()];
    let b = w.as_bytes();
    // `i32` is an integer type, and `b8` a byte type. LLVM ends the type
    // at the last digit, so `i32x` is `i32` and `x`.
    if matches!(b[0], b'i' | b'b') && b.get(1).is_some_and(u8::is_ascii_digit) {
        let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        let end = span.start + 1 + digits;
        out.push(span.start..end, Kind::Type);
        lex_range(text, end..span.end, out);
        return;
    }
    let kind = match w {
        "void" | "half" | "bfloat" | "float" | "double" | "x86_fp80" | "fp128" | "ppc_fp128"
        | "label" | "metadata" | "x86_amx" | "token" | "ptr" => Kind::Type,
        "true" | "false" | "null" | "none" | "undef" | "poison" | "zeroinitializer" => {
            Kind::Constant
        }
        // Values of enums in debug info, such as `DW_TAG_member` and
        // `DIFlagPrototyped`.
        "NoDebug"
        | "FullDebug"
        | "LineTablesOnly"
        | "DebugDirectivesOnly"
        | "GNU"
        | "Apple"
        | "None"
        | "Default"
        | "Binary"
        | "Decimal"
        | "Rational" => Kind::Constant,
        _ if ["DW_", "DIFlag", "DISPFlag", "CSK_"]
            .iter()
            .any(|p| w.starts_with(p)) =>
        {
            Kind::Constant
        }
        // An integer in hexadecimal such as `u0x1F` or `s0xFF`, or the bits
        // of a float such as `f0x3C00`.
        _ if matches!(b[0], b'u' | b's' | b'f')
            && w[1..].starts_with("0x")
            && w.len() > 3
            && b[3..].iter().all(u8::is_ascii_hexdigit) =>
        {
            Kind::Number
        }
        // LLVM prints calling conventions that have no name of their own
        // as `cc` and a number, such as `cc10`.
        _ if w.starts_with("cc") && b.get(2).is_some_and(u8::is_ascii_digit) => {
            out.push(span.start..span.start + 2, Kind::Keyword);
            lex_range(text, span.start + 2..span.end, out);
            return;
        }
        _ => Kind::Keyword,
    };
    out.push(span, kind);
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
        let text = "define dso_local i32 @f(ptr noundef %p) #0 {\n\
                    entry:\n  \
                    %0 = load i32, ptr %p, align 4, !tbaa !5\n  \
                    ret i32 %0\n\
                    }";
        assert_eq!(
            tokens(text),
            [
                ("define", Keyword),
                ("dso_local", Keyword),
                ("i32", Type),
                ("(", Punctuation),
                ("ptr", Type),
                ("noundef", Keyword),
                (")", Punctuation),
                ("{", Punctuation),
                (":", Punctuation),
                ("=", Operator),
                ("load", Keyword),
                ("i32", Type),
                (",", Punctuation),
                ("ptr", Type),
                (",", Punctuation),
                ("align", Keyword),
                ("4", Number),
                (",", Punctuation),
                ("ret", Keyword),
                ("i32", Type),
                ("}", Punctuation),
            ]
        );
        assert_eq!(
            identifiers(text),
            ["@f", "p", "#0", "entry", "0", "p", "!tbaa", "!5", "0"]
        );
    }

    #[test]
    fn marks_plain_identifiers() {
        let text = r#"@g = global %struct.S { i8 1 }, comdat($g), !dbg !0
declare void @"\01_f"(ptr %"a b", i32 %1)
^0 = module: (path: "a.o")"#;
        assert_eq!(
            identifiers(text),
            [
                "@g",
                "struct.S",
                "$g",
                "!dbg",
                "!0",
                "@\"\\01_f\"",
                "\"a b\"",
                "1",
                "^0"
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("x ; a \"b\n/* c\nd */ e /* f"),
            [
                ("x", Kind::Keyword),
                ("; a \"b", Kind::Comment),
                ("/* c", Kind::Comment),
                ("d */", Kind::Comment),
                ("e", Kind::Keyword),
                ("/* f", Kind::Comment),
            ]
        );
        // `/*` comments don't nest.
        assert_eq!(kinds_of("/* /* */ x */", Kind::Comment), ["/* /* */"]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#"c"a\0Ab\\c\n\"#),
            [
                ("c", Kind::Keyword),
                ("\"a", Kind::String),
                ("\\0A", Kind::Escape),
                ("b", Kind::String),
                ("\\\\", Kind::Escape),
                ("c\\n\\", Kind::String),
            ]
        );
        // A backslash doesn't stop a quote from ending a string, and a
        // string goes on over lines.
        assert_eq!(
            tokens("\"a\\\" x \"b\nc\""),
            [
                ("\"a\\\"", Kind::String),
                ("x", Kind::Keyword),
                ("\"b\nc\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens(r#"!{!"x", !{}}"#),
            [
                ("!{!", Kind::Punctuation),
                ("\"x\"", Kind::String),
                (",", Kind::Punctuation),
                ("!{}}", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn labels() {
        // A basic block's label gets the color that its uses get.
        let text = "bb.1:  ; preds = %0\n  br label %bb.1\n\"a b\":\n0:\n-1:";
        assert_eq!(identifiers(text), ["bb.1", "bb.1", "\"a b\"", "0", "-1"]);
        assert_eq!(kinds_of(text, Kind::Punctuation), [":", ":", ":", ":"]);
        // Fields are colored as if they had no colon.
        assert_eq!(
            tokens("!DILocation(line: 3, scope: !1)"),
            [
                ("!DILocation", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("line", Kind::Keyword),
                (":", Kind::Punctuation),
                ("3", Kind::Number),
                (",", Kind::Punctuation),
                ("scope", Kind::Keyword),
                (":", Kind::Punctuation),
                (")", Kind::Punctuation),
            ]
        );
        assert_eq!(
            tokens("\"a\nb\":"),
            [("\"a\nb\"", Kind::String), (":", Kind::Punctuation)]
        );
        assert_eq!(
            tokens("memory(argmem: read) denormal_fpenv(float: ieee)"),
            [
                ("memory", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("argmem", Kind::Keyword),
                (":", Kind::Punctuation),
                ("read", Kind::Keyword),
                (")", Kind::Punctuation),
                ("denormal_fpenv", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("float", Kind::Type),
                (":", Kind::Punctuation),
                ("ieee", Kind::Keyword),
                (")", Kind::Punctuation),
            ]
        );
        // A field that starts its line is still a field, but a label
        // with code after it on its line is a basic block.
        assert_eq!(
            identifiers("sw.default: br label %return"),
            ["sw.default", "return"]
        );
        assert_eq!(
            tokens("!DIFile(\n  file: !1,\n  \"x\": 2)"),
            [
                ("!DIFile", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("file", Kind::Keyword),
                (":", Kind::Punctuation),
                (",", Kind::Punctuation),
                ("\"x\"", Kind::String),
                (":", Kind::Punctuation),
                ("2", Kind::Number),
                (")", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn words() {
        assert_eq!(
            tokens("i1 i128 b8 <4 x float> i32x ix fastcc cc10 ccc"),
            [
                ("i1", Kind::Type),
                ("i128", Kind::Type),
                ("b8", Kind::Type),
                ("<", Kind::Punctuation),
                ("4", Kind::Number),
                ("x", Kind::Keyword),
                ("float", Kind::Type),
                (">", Kind::Punctuation),
                ("i32", Kind::Type),
                ("x", Kind::Keyword),
                ("ix", Kind::Keyword),
                ("fastcc", Kind::Keyword),
                ("cc", Kind::Keyword),
                ("10", Kind::Number),
                ("ccc", Kind::Keyword),
            ]
        );
        assert_eq!(
            kinds_of(
                "true null poison zeroinitializer DW_TAG_member DIFlagPrototyped \
                 CSK_MD5 FullDebug none None nonnull",
                Kind::Constant
            ),
            [
                "true",
                "null",
                "poison",
                "zeroinitializer",
                "DW_TAG_member",
                "DIFlagPrototyped",
                "CSK_MD5",
                "FullDebug",
                "none",
                "None"
            ]
        );
        assert_eq!(
            tokens("#dbg_value(i32 0, !1, !DIExpression(), !2)"),
            [
                ("#", Kind::Punctuation),
                ("dbg_value", Kind::Keyword),
                ("(", Kind::Punctuation),
                ("i32", Kind::Type),
                ("0", Kind::Number),
                (",", Kind::Punctuation),
                (",", Kind::Punctuation),
                ("!DIExpression", Kind::Keyword),
                ("(),", Kind::Punctuation),
                (")", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "1 -2 3.5 -4.0e+10 1.e5 +6.5 0x3FF0000000000000 0xK4000C000000000000000 \
                 0xH3C00 0x1.8p+3 -0x1.p-2 +inf -qnan +nan(0x8) u0xFFFF s0x1 f0x3C00",
                Kind::Number
            ),
            [
                "1",
                "-2",
                "3.5",
                "-4.0e+10",
                "1.e5",
                "+6.5",
                "0x3FF0000000000000",
                "0xK4000C000000000000000",
                "0xH3C00",
                "0x1.8p+3",
                "-0x1.p-2",
                "+inf",
                "-qnan",
                "+nan(0x8)",
                "u0xFFFF",
                "s0x1",
                "f0x3C00"
            ]
        );
        // A NaN's payload ends with its line.
        assert_eq!(
            tokens("-nan(0x\n1)"),
            [("1", Kind::Number), (")", Kind::Punctuation)]
        );
        // These aren't numbers to LLVM. A float needs a `.`, and an
        // exponent needs digits.
        assert_eq!(
            tokens("1e5 2.5e inf u0xZ"),
            [
                ("1", Kind::Number),
                ("e5", Kind::Keyword),
                ("2.5", Kind::Number),
                ("e", Kind::Keyword),
                ("inf", Kind::Keyword),
                ("u0xZ", Kind::Keyword),
            ]
        );
    }

    #[test]
    fn operators_and_punctuation() {
        assert_eq!(
            tokens("declare void @f(...) | i8* = :"),
            [
                ("declare", Kind::Keyword),
                ("void", Kind::Type),
                ("(...)", Kind::Punctuation),
                ("|", Kind::Operator),
                ("i8", Kind::Type),
                ("*", Kind::Operator),
                ("=", Kind::Operator),
                (":", Kind::Punctuation),
            ]
        );
    }
}
