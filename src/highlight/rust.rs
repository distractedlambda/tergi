//! A lexer for Rust.
//!
//! It follows rustc's own lexer, `rustc_lexer`, to find where each token
//! starts and ends. That includes tokens that rustc only reads so that it
//! can report them, such as `'ab'`, because reading them differently would
//! change the colors after them. `dev/reference/rust` checks that the two
//! agree.
//!
//! Without a parser we can't know what an identifier names, so every
//! identifier gets a color from its name (see `Kind::Identifier`).

use std::ops::Range;

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
// Whitespace, as the reference defines it with Unicode's
// Pattern_White_Space.
#[logos(skip r"[ \t\n\x0B\f\r\u{85}\u{200E}\u{200F}\u{2028}\u{2029}]+")]
// Where the body of the last string or character ends in its token, at
// its closing quote or at the end of a token that has none.
#[logos(extras = usize)]
// An identifier or keyword, as the reference defines it with Unicode's
// XID_Start and XID_Continue.
#[logos(subpattern ident = r"[\p{XID_Start}_]\p{XID_Continue}*")]
// An emoji that can't be part of an identifier.
#[logos(subpattern emoji = r"[\p{Emoji}--[\p{ASCII}\p{XID_Continue}]]")]
enum Token {
    #[regex(r"//[^\n]*", allow_greedy = true)]
    #[token("/*", block_comment)]
    Comment,

    // Strings and characters that can hold escapes.
    #[regex(r#"[bc]?""#, quoted)]
    #[token("b'", single_quoted)]
    Str,

    #[regex(r##"[bc]?r#*""##, raw_string)]
    // rustc takes `r#` that isn't followed by a name or a quote as a raw
    // string that doesn't start, together with the next character, and
    // reports it. So we skip that character too.
    #[regex(r"[bc]?r#+", unstarted_raw_string)]
    RawStr,

    // A lifetime, or a character that `lifetime_or_char` turns into a
    // `Str`.
    #[token("'", lifetime_or_char)]
    Lifetime,

    #[regex(r"[0-9]", number)]
    Number,

    // Only the start of an attribute. `lex` also colors the path after it,
    // such as `derive` or `rustfmt::skip`. Finding where the attribute ends
    // would take a parser, since it can hold anything.
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

    // rustc's lexer reads an emoji, and the identifier characters and
    // emoji around it, as one invalid identifier, to report it.
    #[regex(r"(?&ident)?(?&emoji)((?&emoji)|\p{XID_Continue})*")]
    InvalidIdent,

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
    let mut closed = false;
    while i < rest.len() {
        match rest[i] {
            b'\\' => i += 2,
            b'"' => {
                i += 1;
                closed = true;
                break;
            }
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    lex.bump(i.min(rest.len()));
    lex.extras = lex.slice().len() - usize::from(closed);
    if closed {
        suffix(lex);
    }
}

/// Skip to a quote followed by as many `#`s as the string started with.
fn raw_string(lex: &mut Lexer<Token>) {
    let hashes = lex.slice().bytes().filter(|&b| b == b'#').count();
    let close = format!("\"{}", "#".repeat(hashes));
    let rest = lex.remainder();
    match rest.find(&close) {
        Some(i) => {
            lex.bump(i + close.len());
            suffix(lex);
        }
        None => lex.bump(rest.len()),
    }
}

fn unstarted_raw_string(lex: &mut Lexer<Token>) -> bool {
    lex.bump(lex.remainder().chars().next().map_or(0, char::len_utf8));
    false
}

/// Read what follows a `'` the way rustc's lexer does. It's a lifetime
/// such as `'a` or a character such as `'a'`. rustc also reads more than
/// one character between quotes as a character, so that it can report
/// it, as in `'ab'`.
fn lifetime_or_char(lex: &mut Lexer<Token>) -> Token {
    let rest = lex.remainder();
    let mut chars = rest.chars();
    let first = chars.next();
    // rustc reads a lifetime that starts with a digit or an emoji, such as
    // `'1a`, to report it.
    let invalid = first.is_some_and(|c| c.is_ascii_digit()) || emoji_len(rest) > 0;
    let can_be_lifetime = chars.next() != Some('\'') && (name_len(rest) > 0 || invalid);
    if !can_be_lifetime {
        single_quoted(lex);
        return Token::Str;
    }
    if let Some(name) = rest.strip_prefix("r#")
        && name_len(name) > 0
    {
        lex.bump(2 + name_len(name));
        return Token::Lifetime;
    }
    let first = first.map_or(0, char::len_utf8);
    let mut len = first + continue_len(&rest[first..]);
    // An emoji makes the rest of a name invalid, but rustc still reads it
    // as part of the name.
    if emoji_len(&rest[len..]) > 0 {
        loop {
            let n = continue_len(&rest[len..]) + emoji_len(&rest[len..]);
            if n == 0 {
                break;
            }
            len += n;
        }
    }
    lex.bump(len);
    if lex.remainder().starts_with('\'') {
        lex.extras = lex.slice().len();
        lex.bump(1);
        return Token::Str;
    }
    Token::Lifetime
}

/// Skip the rest of a character after its opening quote, the way rustc's
/// lexer does. One that isn't closed ends before a `/` or at the end of
/// its line.
fn single_quoted(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    let mut chars = rest.chars();
    if let (Some(first), Some('\'')) = (chars.next(), chars.next())
        && first != '\\'
    {
        lex.bump(first.len_utf8());
        lex.extras = lex.slice().len();
        lex.bump(1);
        suffix(lex);
        return;
    }
    let b = rest.as_bytes();
    let mut i = 0;
    loop {
        match b.get(i) {
            Some(b'\'') => {
                lex.bump(i);
                lex.extras = lex.slice().len();
                lex.bump(1);
                suffix(lex);
                return;
            }
            None | Some(b'/') => break,
            Some(b'\n') if b.get(i + 1) != Some(&b'\'') => break,
            // This can step into the middle of a character, but we only
            // stop at ASCII, so the end is never there.
            Some(b'\\') => i += 2,
            _ => i += 1,
        }
    }
    lex.bump(i.min(b.len()));
    lex.extras = lex.slice().len();
}

/// Skip a name right after a literal, such as `u8` in `1u8`. rustc's lexer
/// takes any name there as the literal's suffix, even on a string, and
/// only rejects the ones it doesn't know later.
fn suffix(lex: &mut Lexer<Token>) {
    lex.bump(name_len(lex.remainder()));
}

/// Find the end of a number the way rustc's lexer does, from its first
/// digit.
fn number(lex: &mut Lexer<Token>) {
    let first = lex.slice().as_bytes()[0];
    let rest = lex.remainder().as_bytes();
    // Skip digits and underscores. Returns where they end, and whether
    // there was a digit.
    let digits = |mut i: usize, hex: bool| {
        let mut any = false;
        while let Some(&b) = rest.get(i) {
            if b.is_ascii_digit() || hex && b.is_ascii_hexdigit() {
                any = true;
            } else if b != b'_' {
                break;
            }
            i += 1;
        }
        (i, any)
    };
    let mut i = 0;
    let mut fraction = true;
    match (first, rest.first()) {
        // Binary and octal numbers take any decimal digit. rustc rejects
        // the wrong ones later.
        (b'0', Some(b'b' | b'o' | b'x')) => {
            let (end, any) = digits(1, rest[0] == b'x');
            i = end;
            fraction = any;
        }
        (b'0', Some(b'.' | b'e' | b'E')) => {}
        (b'0', Some(b'0'..=b'9' | b'_')) => i = digits(0, false).0,
        (b'0', _) => fraction = false,
        _ => i = digits(0, false).0,
    }
    if fraction {
        match rest.get(i) {
            // A dot followed by another dot or a name is a range, a field
            // or a method call, as in `0..2` and `1.max(2)`.
            Some(b'.')
                if rest.get(i + 1) != Some(&b'.') && name_len(&lex.remainder()[i + 1..]) == 0 =>
            {
                i += 1;
                if rest.get(i).is_some_and(u8::is_ascii_digit) {
                    i = digits(i, false).0;
                    if matches!(rest.get(i), Some(b'e' | b'E')) {
                        i = exponent(rest, i + 1);
                    }
                }
            }
            Some(b'e' | b'E') => i = exponent(rest, i + 1),
            _ => {}
        }
    }
    lex.bump(i);
    suffix(lex);
}

/// Skip the rest of an exponent after its `e`, from `i`.
fn exponent(rest: &[u8], mut i: usize) -> usize {
    if matches!(rest.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    while rest
        .get(i)
        .is_some_and(|b| b.is_ascii_digit() || *b == b'_')
    {
        i += 1;
    }
    i
}

/// Characters that names are made of, for the callbacks.
#[derive(Logos, PartialEq)]
enum Word {
    #[regex(r"[\p{XID_Start}_]\p{XID_Continue}*")]
    Name,
    // Characters that can continue a name but not start one, such as
    // digits, and what follows them.
    #[regex(r"[\p{XID_Continue}--[\p{XID_Start}_]]\p{XID_Continue}*")]
    Rest,
    // Emoji that can't be part of a name. rustc's lexer reads them as
    // part of one, to report them.
    #[regex(r"[\p{Emoji}--[\p{ASCII}\p{XID_Continue}]]+")]
    Emoji,
}

/// The length of the name at the start of `s`, or 0 if it doesn't start
/// with one.
fn name_len(s: &str) -> usize {
    let mut lex = Word::lexer(s);
    match lex.next() {
        Some(Ok(Word::Name)) => lex.span().end,
        _ => 0,
    }
}

/// The length of the characters at the start of `s` that can continue a
/// name.
fn continue_len(s: &str) -> usize {
    let mut lex = Word::lexer(s);
    match lex.next() {
        Some(Ok(Word::Name | Word::Rest)) => lex.span().end,
        _ => 0,
    }
}

/// The length of the emoji at the start of `s`.
fn emoji_len(s: &str) -> usize {
    let mut lex = Word::lexer(s);
    match lex.next() {
        Some(Ok(Word::Emoji)) => lex.span().end,
        _ => 0,
    }
}

/// The length of the shebang line at the start of a file, such as
/// `#!/usr/bin/env run-cargo-script`, or 0 if there's none. Like rustc,
/// we read `#!` as the start of an inner attribute instead when the next
/// token is `[`, even with spaces or comments between them.
fn shebang(text: &str) -> usize {
    let Some(rest) = text.strip_prefix("#!") else {
        return 0;
    };
    let mut lex = Token::lexer(rest);
    let next = loop {
        match lex.next() {
            Some(Ok(Token::Comment)) if !is_doc_comment(lex.slice()) => {}
            next => break next.map(|_| lex.slice()),
        }
    };
    if next == Some("[") {
        return 0;
    }
    let line = rest.split('\n').next().unwrap_or(rest);
    2 + line.strip_suffix('\r').unwrap_or(line).len()
}

fn is_doc_comment(comment: &str) -> bool {
    let b = comment.as_bytes();
    match (b.get(1), b.get(2), b.get(3)) {
        (Some(b'/'), Some(b'/'), next) => next != Some(&b'/'),
        (Some(b'*'), Some(b'*'), next) => !matches!(next, Some(b'*' | b'/')),
        (_, Some(b'!'), _) => true,
        _ => false,
    }
}

/// What comes next in the path at the start of an attribute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrPath {
    Name,
    Sep,
}

/// Color a string or character literal and the escapes in it. Its body
/// ends at `close` in it.
fn push_string(text: &str, span: Range<usize>, close: usize, out: &mut Tokens) {
    let s = &text[span.clone()];
    let at = |i: usize| span.start + i;
    let mut plain = 0;
    let mut i = 0;
    while let Some(n) = s[i..close].find('\\') {
        i += n;
        let (len, valid) = escape(&s[i + 1..close]);
        if valid {
            out.push(at(plain)..at(i), Kind::String);
            out.push(at(i)..at(i + 1 + len), Kind::Escape);
            plain = i + 1 + len;
        }
        i += 1 + len;
    }
    out.push(at(plain)..span.end, Kind::String);
}

/// Read an escape in a string after its backslash, the way rustc does.
/// Returns its length and whether it's valid. One that isn't, such as
/// `\q` or `\x4g`, is plain string. rustc stops reading it after the
/// character that makes it invalid, so that character can't start
/// another escape.
fn escape(s: &str) -> (usize, bool) {
    let mut chars = s.char_indices();
    let Some((_, c)) = chars.next() else {
        return (0, false);
    };
    match c {
        // A backslash at the end of a line skips the line break.
        'n' | 'r' | 't' | '\\' | '\'' | '"' | '0' | '\n' => (1, true),
        '\r' if s[1..].starts_with('\n') => (2, true),
        'x' => {
            for _ in 0..2 {
                match chars.next() {
                    Some((_, c)) if c.is_ascii_hexdigit() => {}
                    _ => return (chars.offset(), false),
                }
            }
            (chars.offset(), true)
        }
        'u' => {
            if chars.next().map(|(_, c)| c) != Some('{') {
                return (chars.offset(), false);
            }
            let mut first = true;
            loop {
                match chars.next() {
                    Some((_, '}' | '_')) if first => return (chars.offset(), false),
                    Some((_, '}')) => return (chars.offset(), true),
                    Some((_, c)) if c == '_' || c.is_ascii_hexdigit() => first = false,
                    _ => return (chars.offset(), false),
                }
            }
        }
        _ => (c.len_utf8(), false),
    }
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    let shebang = shebang(text);
    out.push(0..shebang, Kind::Comment);
    lex.bump(shebang);
    // Whether we're in the path of an attribute, such as `#[rustfmt::skip`.
    let mut attr = None;
    while let Some(token) = lex.next() {
        let span = lex.span();
        let slice = lex.slice();
        // A comment inside an attribute's path changes nothing.
        if token == Ok(Token::Comment) {
            out.push(span, Kind::Comment);
            continue;
        }
        let path = attr.take();
        let kind = match token {
            Ok(Token::Attribute) => {
                attr = Some(AttrPath::Name);
                Kind::Attribute
            }
            Ok(Token::Ident) if path == Some(AttrPath::Name) => {
                attr = Some(AttrPath::Sep);
                Kind::Attribute
            }
            Ok(Token::Punctuation) if path == Some(AttrPath::Sep) && slice == "::" => {
                attr = Some(AttrPath::Name);
                Kind::Attribute
            }
            Ok(Token::Ident) => Kind::Identifier,
            Ok(Token::Str) => {
                push_string(text, span, lex.extras, out);
                continue;
            }
            Ok(Token::RawStr) => Kind::String,
            Ok(Token::Lifetime) => Kind::Lifetime,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Keyword) => Kind::Keyword,
            Ok(Token::Bool) => Kind::Constant,
            Ok(Token::Comment | Token::InvalidIdent) | Err(()) => continue,
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
        // A dot ends a float unless a name or another dot follows it.
        assert_eq!(
            kinds_of("[1., 2.] 3._x 4.e5 5.0.0", Kind::Number),
            ["1.", "2.", "3", "4", "5.0", "0"]
        );
        // Any name right after a number is its suffix, as in rustc.
        assert_eq!(
            kinds_of("1f16 2.5_f128 3b 0x1g 0b102 1else", Kind::Number),
            ["1f16", "2.5_f128", "3b", "0x1g", "0b102", "1else"]
        );
    }

    #[test]
    fn suffixes_on_strings() {
        assert_eq!(
            tokens(r##""a"x 'b'y r#"c"#z b"\n"w"##),
            [
                ("\"a\"x", Kind::String),
                ("'b'y", Kind::String),
                ("r#\"c\"#z", Kind::String),
                ("b\"", Kind::String),
                ("\\n", Kind::Escape),
                ("\"w", Kind::String),
            ]
        );
    }

    #[test]
    fn invalid_characters_read_like_rustc() {
        // rustc reads these as characters to report them.
        assert_eq!(
            tokens("'ab' ' 1 + 1'; '\\q';"),
            [
                ("'ab'", Kind::String),
                ("' 1 + 1'", Kind::String),
                (";", Kind::Punctuation),
                ("'\\q'", Kind::String),
                (";", Kind::Punctuation),
            ]
        );
        // One without a closing quote ends before a `/` or at the end of
        // its line.
        assert_eq!(
            tokens("'\\n // x\n'\\u{0}\n;"),
            [
                ("'", Kind::String),
                ("\\n", Kind::Escape),
                (" ", Kind::String),
                ("// x", Kind::Comment),
                ("'", Kind::String),
                ("\\u{0}", Kind::Escape),
                (";", Kind::Punctuation),
            ]
        );
        // These are lifetimes that rustc reports, not characters.
        assert_eq!(
            kinds_of("'1a '🦀 'a🦀b", Kind::Lifetime),
            ["'1a", "'🦀", "'a🦀b"]
        );
        // A raw string without a quote is an error, and so is the
        // character after `#`.
        assert_eq!(tokens("r#'a'"), [("'", Kind::String)]);
    }

    #[test]
    fn invalid_escapes_read_like_rustc() {
        // An invalid escape takes the character that makes it invalid,
        // so a backslash there doesn't start another escape.
        assert_eq!(
            tokens("\"\\u{\\\" \\x4\\n \\u{_1} \\u{1}\""),
            [
                ("\"\\u{\\\" \\x4\\n \\u{_1} ", Kind::String),
                ("\\u{1}", Kind::Escape),
                ("\"", Kind::String),
            ]
        );
        // The escapes end with the body, so the closing quote of `"\u\\"`
        // isn't escaped.
        assert_eq!(tokens(r#""\u\\" x"#), [("\"\\u\\\\\"", Kind::String)]);
    }

    #[test]
    fn shebangs() {
        assert_eq!(
            tokens("#!/usr/bin/env run-cargo-script\nfn"),
            [
                ("#!/usr/bin/env run-cargo-script", Kind::Comment),
                ("fn", Kind::Keyword),
            ]
        );
        // `#!` followed by `[` is the start of an inner attribute.
        assert_eq!(kinds_of("#! // x\n[allow(x)]", Kind::Comment), ["// x"]);
        assert_eq!(kinds_of(" #!/x", Kind::Comment), Vec::<&str>::new());
    }

    #[test]
    fn unusual_whitespace() {
        // The reference counts these as whitespace too.
        assert_eq!(
            kinds_of(
                "#[\u{b}\u{85}\u{200e}\u{200f}\u{2028}\u{2029}derive",
                Kind::Attribute
            ),
            ["#[", "derive"]
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
        // continue an identifier but not start one. An emoji is an error
        // that takes the identifiers around it along, as in rustc.
        assert_eq!(
            kinds_of("x→fn \u{301}fn 🦀fn fn🦀 fn", Kind::Keyword),
            ["fn", "fn", "fn"]
        );
    }

    #[test]
    fn attributes() {
        assert_eq!(
            kinds_of(
                "#[derive(Debug)] #![allow(x)] #[rustfmt::skip] #[ cfg(test)] #[a::]",
                Kind::Attribute
            ),
            [
                "#[derive",
                "#![allow",
                "#[rustfmt::skip",
                "#[",
                "cfg",
                "#[a::"
            ]
        );
        assert_eq!(kinds_of("#[x] y::z", Kind::Attribute), ["#[x"]);
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

    /// Every name that isn't a keyword or in an attribute's path is an
    /// identifier, one token each. That includes functions, types,
    /// macros and constants.
    #[test]
    fn marks_plain_identifiers() {
        let text = "fn f<T>(x: &str) -> Vec<u8> { foo.len(y) + r#type; Vec::<T>::new(); println!(MAX) } #[rustfmt::skip]";
        let mut out = Tokens::default();
        lex(text, &mut out);
        assert_eq!(
            out.identifiers(text),
            [
                "f", "T", "x", "str", "Vec", "u8", "foo", "len", "y", "r#type", "Vec", "T", "new",
                "println", "MAX"
            ]
        );
        assert_eq!(
            out.colored(text)
                .into_iter()
                .filter(|&(_, k)| k == Kind::Operator)
                .map(|(t, _)| t)
                .collect::<Vec<_>>(),
            ["<", ">", "&", "->", "<", ">", "+", "<", ">", "!"]
        );
    }
}
