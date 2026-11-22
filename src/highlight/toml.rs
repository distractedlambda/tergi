//! A lexer for TOML.
//!
//! It follows TOML 1.1, which adds the `\e` and `\xHH` escapes and allows
//! line breaks inside inline tables. Whether a word is a key or a value
//! depends on where it is, since `true = 1` and `2024-01-01 = 1` are
//! valid keys. So the lexer keeps track of the arrays and inline tables
//! it's inside of, and whether a key or a value comes next. The names in
//! keys are identifiers, so they get colors from their text, and table
//! headers such as `[server]` get the attribute color.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\f]+")]
enum Token {
    // Line breaks end key/value pairs, so they aren't skipped.
    #[token("\n")]
    Newline,

    #[regex(r"#[^\n]*", allow_greedy = true)]
    Comment,

    #[token("\"", basic_string)]
    #[token("\"\"\"", |lex| multiline_string(lex, b'"'))]
    Basic,

    #[token("'", literal_string)]
    #[token("'''", |lex| multiline_string(lex, b'\''))]
    Literal,

    #[regex(r"[+-]?[0-9][0-9_]*(\.[0-9][0-9_]*)?([eE][+-]?[0-9][0-9_]*)?")]
    #[regex(r"0x[0-9a-fA-F][0-9a-fA-F_]*|0o[0-7][0-7_]*|0b[01][01_]*")]
    #[regex(r"[+-]?(inf|nan)")]
    // A date, maybe with a time, which can follow it after a space.
    #[regex(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}([Tt ][0-9]{2}:[0-9]{2}(:[0-9]{2}(\.[0-9]+)?)?([Zz]|[+-][0-9]{2}:[0-9]{2})?)?"
    )]
    // A time on its own. TOML 1.1 lets it leave out the seconds.
    #[regex(r"[0-9]{2}:[0-9]{2}(:[0-9]{2}(\.[0-9]+)?)?")]
    Number,

    #[token("true")]
    #[token("false")]
    Bool,

    // Anything else up to the next space or bracket, such as a bare key
    // or dotted key, or a value that isn't valid. Without it, a value
    // such as `xtrue` would end in a colored `true`.
    #[regex(r#"[^ \t\r\f\n#"'\[\]{},=]+"#, priority = 1)]
    Word,

    #[token("[")]
    Open,
    #[token("]")]
    Close,
    #[token("{")]
    OpenBrace,
    #[token("}")]
    CloseBrace,
    #[token(",")]
    Comma,
    #[token("=")]
    Equals,
}

/// Skip to the closing quote. A basic string can't go on to the next line,
/// so one without a closing quote ends at the end of the line.
fn basic_string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' if rest.get(i + 1).is_some_and(|&b| b != b'\n') => i += 2,
            b'"' => {
                i += 1;
                break;
            }
            b'\n' => break,
            _ => i += 1,
        }
    }
    lex.bump(i);
}

/// Skip to the closing quote, or to the end of the line.
fn literal_string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(
        rest.find(['\'', '\n'])
            .map_or(rest.len(), |i| i + usize::from(rest.as_bytes()[i] == b'\'')),
    );
}

/// Skip to the end of a multi-line string, which three quotes close. Up to
/// two more quotes can come before them, and those belong to the string.
/// So a run of four or five quotes ends the string too. Only basic strings
/// have escapes.
fn multiline_string(lex: &mut Lexer<Token>, quote: u8) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' if quote == b'"' => i += 2,
            b if b == quote => {
                let run = rest[i..].iter().take_while(|&&b| b == quote).count();
                i += run.min(5);
                if run >= 3 {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    lex.bump(i.min(rest.len()));
}

/// Whether the rest of the line after a word in key position makes the
/// word part of a key. Words in a hunk from the middle of a multi-line
/// array are in key position too, but they are values.
fn key_follows(rest: &str) -> bool {
    rest.trim_start_matches([' ', '\t']).starts_with(['.', '='])
}

/// The pieces of a basic string. Anything else in one, such as a backslash
/// before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    // A backslash at the end of a line in a multi-line string skips the
    // line break and the whitespace around it.
    #[regex(r#"\\([btnfre"\\]|x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4}|U[0-9a-fA-F]{8}|[ \t]*\r?\n)"#)]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

/// What comes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Key,
    Value,
    /// The rest of a table header. Its brackets say whether it's an array
    /// of tables, as in `[[bin]]`.
    Header {
        double: bool,
    },
}

/// The brackets the lexer is inside of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Array,
    Table,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    let mut expect = Expect::Key;
    let mut stack = Vec::new();
    while let Some(token) = lex.next() {
        let span = lex.span();
        let Ok(token) = token else {
            continue;
        };
        if let Expect::Header { double } = expect {
            match token {
                // A header without a closing bracket ends with its line.
                Token::Newline => expect = Expect::Key,
                Token::Comment => out.push(span, Kind::Comment),
                Token::Close => {
                    let mut end = span.end;
                    if double && lex.remainder().starts_with(']') {
                        lex.bump(1);
                        end += 1;
                    }
                    out.push(span.start..end, Kind::Attribute);
                    // Only a comment can follow on the same line.
                    expect = Expect::Value;
                }
                _ => out.push(span, Kind::Attribute),
            }
            continue;
        }
        let kind = match token {
            Token::Newline => {
                // An array can go on over several lines, and so can an
                // inline table in TOML 1.1.
                match stack.last() {
                    None => expect = Expect::Key,
                    Some(Open::Array) => expect = Expect::Value,
                    Some(Open::Table) => {}
                }
                continue;
            }
            Token::Comment => Kind::Comment,
            Token::Basic | Token::Literal
                if expect == Expect::Key
                    && !lex.slice().starts_with(r#"""""#)
                    && !lex.slice().starts_with("'''")
                    && key_follows(lex.remainder()) =>
            {
                // A quoted key ends with its quote, since one that doesn't
                // would go on to the end of its line. The quotes are plain.
                out.push(span.start + 1..span.end - 1, Kind::Identifier);
                continue;
            }
            Token::Number | Token::Bool | Token::Word
                if expect == Expect::Key
                    && (lex.slice().starts_with('.')
                        || lex.slice().ends_with('.')
                        || key_follows(lex.remainder())) =>
            {
                // A key, maybe dotted, as in `a.b.c`. A word that starts
                // or ends with a dot is part of a longer key, as in `a .b`
                // or `a."b"`.
                let mut start = span.start;
                for (i, _) in lex.slice().match_indices('.') {
                    let dot = span.start + i;
                    out.push(start..dot, Kind::Identifier);
                    out.push(dot..dot + 1, Kind::Punctuation);
                    start = dot + 1;
                }
                out.push(start..span.end, Kind::Identifier);
                continue;
            }
            Token::Basic => {
                for (part, r) in Part::lexer(lex.slice()).spanned() {
                    let kind = match part {
                        Ok(Part::Escape) => Kind::Escape,
                        _ => Kind::String,
                    };
                    out.push(span.start + r.start..span.start + r.end, kind);
                }
                continue;
            }
            Token::Literal => Kind::String,
            Token::Number => Kind::Number,
            Token::Bool => Kind::Constant,
            Token::Word => continue,
            Token::Open if expect == Expect::Key && stack.is_empty() => {
                let double = lex.remainder().starts_with('[');
                if double {
                    lex.bump(1);
                }
                out.push(span.start..lex.span().end, Kind::Attribute);
                expect = Expect::Header { double };
                continue;
            }
            Token::Open => {
                stack.push(Open::Array);
                expect = Expect::Value;
                Kind::Punctuation
            }
            Token::OpenBrace => {
                stack.push(Open::Table);
                expect = Expect::Key;
                Kind::Punctuation
            }
            Token::Close | Token::CloseBrace => {
                let open = if token == Token::Close {
                    Open::Array
                } else {
                    Open::Table
                };
                // A closing bracket without its opening one, as in a hunk
                // from the middle of an array, closes nothing.
                if stack.last() == Some(&open) {
                    stack.pop();
                    expect = Expect::Value;
                }
                Kind::Punctuation
            }
            Token::Comma => {
                match stack.last() {
                    Some(Open::Table) => expect = Expect::Key,
                    Some(Open::Array) => expect = Expect::Value,
                    None => {}
                }
                Kind::Punctuation
            }
            Token::Equals => {
                expect = Expect::Value;
                Kind::Operator
            }
        };
        out.push(span, kind);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use proptest::prelude::*;
    use toml_edit::{Item, TableLike, Value};

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
    fn lexes_a_manifest() {
        use Kind::*;
        let text = "\
[package]  # the crate
name = \"tergi\"
edition = 2024

[[bin]]
test = false
dependencies.logos = { version = \"0.16\", features = [] }
";
        assert_eq!(
            tokens(text),
            [
                ("[package]", Attribute),
                ("# the crate", Comment),
                ("=", Operator),
                ("\"tergi\"", String),
                ("=", Operator),
                ("2024", Number),
                ("[[bin]]", Attribute),
                ("=", Operator),
                ("false", Constant),
                (".", Punctuation),
                ("=", Operator),
                ("{", Punctuation),
                ("=", Operator),
                ("\"0.16\"", String),
                (",", Punctuation),
                ("=", Operator),
                ("[]", Punctuation),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn marks_plain_identifiers() {
        // Bare keys can look like values.
        let text = "true = 1\n2024-01-01 = inf\n1.5 = nan";
        assert_eq!(identifiers(text), ["true", "2024-01-01", "1", "5"]);
        assert_eq!(
            tokens(text),
            [
                ("=", Kind::Operator),
                ("1", Kind::Number),
                ("=", Kind::Operator),
                ("inf", Kind::Number),
                (".", Kind::Punctuation),
                ("=", Kind::Operator),
                ("nan", Kind::Number),
            ]
        );
        // The quotes of a quoted key are plain, and the dots between the
        // parts of a dotted key are punctuation.
        let text = r#""a\tb".'c' . d .e. "".f."g" = 1"#;
        assert_eq!(identifiers(text), ["a\\tb", "c", "d", "e", "f", "g"]);
        assert_eq!(
            tokens(text),
            [
                (".", Kind::Punctuation),
                (".", Kind::Punctuation),
                (".", Kind::Punctuation),
                (".", Kind::Punctuation),
                (".", Kind::Punctuation),
                (".", Kind::Punctuation),
                ("=", Kind::Operator),
                ("1", Kind::Number),
            ]
        );
        // Keys in inline tables are identifiers too.
        assert_eq!(identifiers("a = { b = 1, 'c' = [d] }"), ["a", "b", "c"]);
        // A hunk from the middle of an array has values where a key would
        // go. Without a `.` or `=` after them, they aren't keys.
        assert_eq!(
            tokens("  \"x\", 'y',\n  1, true, z\n]"),
            [
                ("\"x\"", Kind::String),
                (",", Kind::Punctuation),
                ("'y'", Kind::String),
                (",", Kind::Punctuation),
                ("1", Kind::Number),
                (",", Kind::Punctuation),
                ("true", Kind::Constant),
                (",", Kind::Punctuation),
                ("]", Kind::Punctuation),
            ]
        );
        assert_eq!(
            identifiers("  \"x\", 'y',\n  1, true, z\n]"),
            [] as [&str; 0]
        );
    }

    #[test]
    fn inline_tables_and_arrays() {
        // Keys in inline tables are plain, and values in arrays aren't,
        // even when they go on over several lines.
        assert_eq!(
            kinds_of(
                "a = [true, { true = false, b = [1, {c = 2}], }, [\n  3,\n  # x\n  4]]\nd = 5",
                Kind::Number
            ),
            ["1", "2", "3", "4", "5"]
        );
        assert_eq!(
            kinds_of(
                "a = [true, { true = false, b = [1, {c = 2}], }, [3, 4]]",
                Kind::Constant
            ),
            ["true", "false"]
        );
        assert_eq!(
            kinds_of("a = {\n  b = 1,\n  true = 2\n}\n", Kind::Constant),
            [] as [&str; 0]
        );
        // A bracket that starts a line inside an array is an array, not a
        // table header.
        assert_eq!(kinds_of("a = [\n[1]]", Kind::Attribute), [] as [&str; 0]);
    }

    #[test]
    fn table_headers() {
        assert_eq!(
            tokens("[ a . \"b\" ]\n[[c.d]] # e\n[[f]\n[g\nh = 1"),
            [
                ("[", Kind::Attribute),
                ("a", Kind::Attribute),
                (".", Kind::Attribute),
                ("\"b\"", Kind::Attribute),
                ("]", Kind::Attribute),
                ("[[c.d]]", Kind::Attribute),
                ("# e", Kind::Comment),
                ("[[f]", Kind::Attribute),
                ("[g", Kind::Attribute),
                ("=", Kind::Operator),
                ("1", Kind::Number),
            ]
        );
        // A hunk from the middle of an array closes brackets that it
        // didn't open, and then finds the next header.
        assert_eq!(kinds_of("  1,\n]\n[a]\n", Kind::Attribute), ["[a]"]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#"a = "x\n\"\e\x1b\u00e9\U0001F600\q""#),
            [
                ("=", Kind::Operator),
                ("\"x", Kind::String),
                ("\\n\\\"\\e\\x1b\\u00e9\\U0001F600", Kind::Escape),
                ("\\q\"", Kind::String),
            ]
        );
        // Basic and literal strings end with their line.
        assert_eq!(
            tokens("a = \"b\\\nc = 'd\ne = 1"),
            [
                ("=", Kind::Operator),
                ("\"b\\", Kind::String),
                ("=", Kind::Operator),
                ("'d", Kind::String),
                ("=", Kind::Operator),
                ("1", Kind::Number),
            ]
        );
        assert_eq!(
            tokens(r"a = 'C:\n' # c"),
            [
                ("=", Kind::Operator),
                (r"'C:\n'", Kind::String),
                ("# c", Kind::Comment),
            ]
        );
    }

    #[test]
    fn multiline_strings() {
        assert_eq!(
            tokens("a = \"\"\"x \\\n  y\"\"\"\" b"),
            [
                ("=", Kind::Operator),
                ("\"\"\"x ", Kind::String),
                ("\\\n", Kind::Escape),
                ("  y\"\"\"\"", Kind::String),
            ]
        );
        // Up to two quotes before the closing ones are part of the string.
        assert_eq!(kinds_of("a = '''x''''' b = 1", Kind::String), ["'''x'''''"]);
        assert_eq!(
            kinds_of("a = '''x\\'''\nb = \"\"\"\"\"\"", Kind::String),
            ["'''x\\'''", "\"\"\"\"\"\""]
        );
        assert_eq!(
            tokens("a = \"\"\"x\\\"\"\"\"\n# y\n"),
            [
                ("=", Kind::Operator),
                ("\"\"\"x", Kind::String),
                ("\\\"", Kind::Escape),
                ("\"\"\"", Kind::String),
                ("# y", Kind::Comment),
            ]
        );
        assert_eq!(
            tokens("a = '''open\nb = 1"),
            [("=", Kind::Operator), ("'''open\nb = 1", Kind::String),]
        );
    }

    #[test]
    fn numbers_and_dates() {
        assert_eq!(
            kinds_of(
                "a = [1_000, +1, -0.5, 6.626e-34, 1E6, 0xDEAD_beef, 0o755, 0b1101, +inf, -nan]",
                Kind::Number
            ),
            [
                "1_000",
                "+1",
                "-0.5",
                "6.626e-34",
                "1E6",
                "0xDEAD_beef",
                "0o755",
                "0b1101",
                "+inf",
                "-nan"
            ]
        );
        assert_eq!(
            kinds_of(
                "a = [1979-05-27T07:32:00Z, 1979-05-27 00:32:00.999-07:00, 1979-05-27, 07:32, 07:32:00.5]",
                Kind::Number
            ),
            [
                "1979-05-27T07:32:00Z",
                "1979-05-27 00:32:00.999-07:00",
                "1979-05-27",
                "07:32",
                "07:32:00.5"
            ]
        );
        // A word that only starts like a value is plain.
        assert_eq!(
            tokens("a = [1x, 1., truex, xtrue, infinity, 1979-05-27 x]"),
            [
                ("=", Kind::Operator),
                ("[", Kind::Punctuation),
                (",", Kind::Punctuation),
                (",", Kind::Punctuation),
                (",", Kind::Punctuation),
                (",", Kind::Punctuation),
                (",", Kind::Punctuation),
                ("1979-05-27", Kind::Number),
                ("]", Kind::Punctuation),
            ]
        );
    }

    fn pick(xs: &'static [&'static str]) -> BoxedStrategy<String> {
        prop::sample::select(xs).prop_map(str::to_string).boxed()
    }

    /// A key, maybe dotted, with parts that look like values or hold
    /// characters that mean something outside quotes.
    fn key() -> BoxedStrategy<String> {
        let part = pick(&[
            "a",
            "b-c",
            "_x",
            "-",
            "true",
            "false",
            "inf",
            "nan",
            "123",
            "0x1F",
            "1979-05-27",
            r#""a b""#,
            r#""\t\"""#,
            r#""""#,
            r##""#""##,
            r#""=""#,
            r#""]""#,
            "'q'",
            "'['",
        ]);
        (prop::collection::vec(part, 1..4), any::<bool>())
            .prop_map(|(parts, spaced)| parts.join(if spaced { " . " } else { "." }))
            .boxed()
    }

    fn value() -> BoxedStrategy<String> {
        let scalar = pick(&[
            "0",
            "+1",
            "-17",
            "1_000",
            "0xDEAD_beef",
            "0o755",
            "0b1101",
            "1.5",
            "-0.5e-3",
            "6E+2",
            "1_0.0_1",
            "inf",
            "+inf",
            "-nan",
            "true",
            "false",
            "1979-05-27T07:32:00Z",
            "1979-05-27 07:32:00.999-07:00",
            "1979-05-27t07:32Z",
            "1979-05-27",
            "07:32:00",
            "07:32",
            "00:32:00.999999",
            r#""a\n\"b""#,
            r#""\e\x41\u00e9\U0001F600""#,
            r##""#[a]""##,
            r#""'""#,
            r#""""#,
            r"'C:\x'",
            r#"'"'"#,
            "''",
            "\"\"\"\na\"b\"\"c\"\"\"",
            "\"\"\"x \\\n   y\"\"\"\"\"",
            r#""""a\"""""#,
            r#""""""""#,
            "\"\"\"#[x]\n= 1\"\"\"",
            "'''\nit's'''",
            "'''a'''''",
            "'''[x]\n'''",
        ]);
        scalar
            .prop_recursive(3, 16, 4, |inner| {
                let seps = pick(&[", ", " ,", ",\n  ", ", # c\n"]);
                let array = (
                    prop::collection::vec((inner.clone(), seps), 0..4),
                    any::<bool>(),
                )
                    .prop_map(|(items, newline)| {
                        let mut s = String::from(if newline { "[\n" } else { "[" });
                        for (v, sep) in items {
                            s.push_str(&v);
                            s.push_str(&sep);
                        }
                        s + "]"
                    });
                // TOML 1.1 allows line breaks, comments and a final comma in
                // inline tables.
                let seps = pick(&[", ", ",\n", ",\n# c\n"]);
                let table = (
                    prop::collection::vec((key(), inner, seps), 0..3),
                    any::<bool>(),
                )
                    .prop_map(|(pairs, trailing)| {
                        let n = pairs.len();
                        let mut s = String::from("{");
                        for (i, (k, v, sep)) in pairs.into_iter().enumerate() {
                            s.push_str(&format!("{k} = {v}"));
                            if i + 1 < n || trailing {
                                s.push_str(&sep);
                            }
                        }
                        s + "}"
                    });
                prop_oneof![array, table]
            })
            .boxed()
    }

    fn document() -> impl Strategy<Value = String> {
        let line = prop_oneof![
            4 => (key(), value()).prop_map(|(k, v)| format!("{k} = {v}")),
            1 => key().prop_map(|k| format!("[{k}]")),
            1 => key().prop_map(|k| format!("[[ {k} ]]")),
            1 => pick(&["# x = 1", "", "  "]),
        ];
        prop::collection::vec((line, pick(&["", " # c", "\t#[x]"])), 0..8)
            .prop_map(|lines| lines.into_iter().map(|(l, c)| l + &c + "\n").collect())
    }

    /// Check that each byte in `r` that isn't whitespace has one of the
    /// kinds in `ok`, or none if `plain` is true.
    fn check(
        kinds: &[Option<Kind>],
        text: &str,
        r: Range<usize>,
        ok: &[Kind],
        plain: bool,
    ) -> Result<(), TestCaseError> {
        for i in r.clone() {
            let fine = match kinds[i] {
                Some(k) => ok.contains(&k),
                None => plain || text.as_bytes()[i].is_ascii_whitespace(),
            };
            prop_assert!(fine, "{:?} has {:?} at {i} in\n{text}", &text[r], kinds[i]);
        }
        Ok(())
    }

    /// Check that a key is an identifier, except for its quotes if it
    /// has them, or that it's part of a table header.
    fn check_key(kinds: &[Option<Kind>], text: &str, r: Range<usize>) -> Result<(), TestCaseError> {
        let quoted = text[r.clone()].starts_with(['"', '\'']);
        for i in r.clone() {
            let want = if quoted && (i == r.start || i + 1 == r.end) {
                None
            } else {
                Some(Kind::Identifier)
            };
            let fine = kinds[i] == want
                || kinds[i] == Some(Kind::Attribute)
                || text.as_bytes()[i].is_ascii_whitespace();
            prop_assert!(fine, "{:?} has {:?} at {i} in\n{text}", &text[r], kinds[i]);
        }
        Ok(())
    }

    fn check_table(
        t: &dyn TableLike,
        kinds: &[Option<Kind>],
        text: &str,
    ) -> Result<(), TestCaseError> {
        for (name, _) in t.iter() {
            let Some((key, item)) = t.get_key_value(name) else {
                continue;
            };
            if let Some(r) = key.span() {
                check_key(kinds, text, r)?;
            }
            match item {
                Item::Value(v) => check_value(v, kinds, text)?,
                Item::Table(t) => check_table(t, kinds, text)?,
                Item::ArrayOfTables(a) => {
                    for t in a {
                        check_table(t, kinds, text)?;
                    }
                }
                Item::None => {}
            }
        }
        Ok(())
    }

    fn check_value(v: &Value, kinds: &[Option<Kind>], text: &str) -> Result<(), TestCaseError> {
        let ok: &[Kind] = match v {
            Value::String(_) => &[Kind::String, Kind::Escape],
            Value::Integer(_) | Value::Float(_) | Value::Datetime(_) => &[Kind::Number],
            Value::Boolean(_) => &[Kind::Constant],
            Value::Array(a) => {
                for v in a {
                    check_value(v, kinds, text)?;
                }
                return Ok(());
            }
            Value::InlineTable(t) => return check_table(t, kinds, text),
        };
        match v.span() {
            Some(r) => check(kinds, text, r, ok, false),
            None => Ok(()),
        }
    }

    proptest! {
        /// In documents that toml_edit can parse, values get the color of
        /// their type, and keys are identifiers.
        #[test]
        fn agrees_with_toml_edit(text in document()) {
            let Ok(doc) = toml_edit::Document::parse(text.as_str()) else {
                return Ok(());
            };
            let mut out = Tokens::default();
            lex(&text, &mut out);
            let mut kinds = vec![None; text.len()];
            for (r, k) in out.0 {
                kinds[r].fill(Some(k));
            }
            check_table(doc.as_table(), &kinds, &text)?;
        }
    }
}
