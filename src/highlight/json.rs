//! A lexer for JSON.
//!
//! It follows RFC 8259, and also reads `//` and `/* */` comments, which
//! files such as `tsconfig.json` and VS Code's settings use. Keys are
//! plain, like bare keys in TOML, so that the values stand out. A string
//! is a key when a `:` follows it on the same line.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
enum Token {
    #[regex(r"//[^\n]*", allow_greedy = true)]
    #[token("/*", block_comment)]
    Comment,

    #[token("\"", string)]
    Str,

    #[regex(r"-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?")]
    Number,

    #[token("true")]
    #[token("false")]
    #[token("null")]
    Constant,

    // Anything else up to the next space, bracket or quote, such as a
    // word that isn't valid JSON. Without it, `truex` would start with a
    // colored `true`, and `01` would start with a colored `0`.
    #[regex(r#"[^ \t\r\n{}\[\],:"/]+"#, priority = 1)]
    Word,

    #[regex(r"[{}\[\],:]")]
    Punctuation,
}

/// Skip to the end of a block comment. Block comments don't nest.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(rest.find("*/").map_or(rest.len(), |i| i + 2));
}

/// Skip to the closing quote. A string can't go on to the next line, so
/// one without a closing quote ends with its line.
fn string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' if rest.get(i + 1).is_some_and(|&b| b != b'\n' && b != b'\r') => i += 2,
            b'"' => {
                i += 1;
                break;
            }
            b'\n' | b'\r' => break,
            _ => i += 1,
        }
    }
    lex.bump(i);
}

/// The pieces of a string. Anything else in one, such as a backslash
/// before a character that can't be escaped, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r#"\\(["\\/bfnrt]|u[0-9a-fA-F]{4})"#)]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    while let Some(token) = lex.next() {
        let span = lex.span();
        let kind = match token {
            Ok(Token::Comment) => {
                out.push_comment(text, span);
                continue;
            }
            Ok(Token::Str) => {
                let rest = lex.remainder().trim_start_matches([' ', '\t']);
                if rest.starts_with(':') {
                    continue;
                }
                for (part, r) in Part::lexer(lex.slice()).spanned() {
                    let kind = match part {
                        Ok(Part::Escape) => Kind::Escape,
                        _ => Kind::String,
                    };
                    out.push(span.start + r.start..span.start + r.end, kind);
                }
                continue;
            }
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Constant) => Kind::Constant,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Word) | Err(()) => continue,
        };
        out.push(span, kind);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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
    fn lexes_a_manifest() {
        use Kind::*;
        let text = r#"{
  "name": "tergi",
  "version": 1.5,
  "private": true,
  "files": ["a\tb", null],
  "scripts": {}
}"#;
        assert_eq!(
            tokens(text),
            [
                ("{", Punctuation),
                (":", Punctuation),
                ("\"tergi\"", String),
                (",", Punctuation),
                (":", Punctuation),
                ("1.5", Number),
                (",", Punctuation),
                (":", Punctuation),
                ("true", Constant),
                (",", Punctuation),
                (":", Punctuation),
                ("[", Punctuation),
                ("\"a", String),
                ("\\t", Escape),
                ("b\"", String),
                (",", Punctuation),
                ("null", Constant),
                ("],", Punctuation),
                (":", Punctuation),
                ("{}", Punctuation),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn keys_are_plain() {
        // A string is a key when a `:` follows it on its line, even
        // outside of an object, as in a hunk from the middle of one.
        assert_eq!(
            tokens("\"a\\\"b\" \t: \"c\":\"d\",\n\"e\"\n: 1"),
            [
                (":", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("\"d\"", Kind::String),
                (",", Kind::Punctuation),
                ("\"e\"", Kind::String),
                (":", Kind::Punctuation),
                ("1", Kind::Number),
            ]
        );
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#"["\"\\\/\b\f\n\r\t\u00E9\uD83D", "\x\u12\é"]"#),
            [
                ("[", Kind::Punctuation),
                ("\"", Kind::String),
                (r#"\"\\\/\b\f\n\r\t\u00E9\uD83D"#, Kind::Escape),
                ("\"", Kind::String),
                (",", Kind::Punctuation),
                (r#""\x\u12\é""#, Kind::String),
                ("]", Kind::Punctuation),
            ]
        );
        // A string ends with its line, even after a backslash.
        assert_eq!(
            tokens("[\"a\n1, \"b\\\r\n2]"),
            [
                ("[", Kind::Punctuation),
                ("\"a", Kind::String),
                ("1", Kind::Number),
                (",", Kind::Punctuation),
                ("\"b\\", Kind::String),
                ("2", Kind::Number),
                ("]", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn numbers_and_words() {
        assert_eq!(
            kinds_of("[0, -0, 12, -3.25, 1e9, 6.02E+23, 1.5e-3]", Kind::Number),
            ["0", "-0", "12", "-3.25", "1e9", "6.02E+23", "1.5e-3"]
        );
        // A word that only starts like a value is plain, and so are the
        // extra values of JSON5.
        let text = "[01, 1., .5, +1, 1e, -, truex, nulls, NaN, Infinity, 0x1F, 'a']";
        assert!(
            tokens(text).iter().all(|&(_, k)| k == Kind::Punctuation),
            "{:?}",
            tokens(text)
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("{ // a \"b\": 1\r\n  /* c\n  d */ \"e\": /* f */ 2 /* g"),
            [
                ("{", Kind::Punctuation),
                ("// a \"b\": 1", Kind::Comment),
                ("/* c", Kind::Comment),
                ("  d */", Kind::Comment),
                (":", Kind::Punctuation),
                ("/* f */", Kind::Comment),
                ("2", Kind::Number),
                ("/* g", Kind::Comment),
            ]
        );
        // A comment between a key and its `:` makes the key a string.
        assert_eq!(kinds_of("\"a\" /**/ : 1", Kind::String), ["\"a\""]);
        // A lone slash is plain.
        assert_eq!(tokens("1 / 2"), [("1", Kind::Number), ("2", Kind::Number)]);
    }

    /// A piece of JSON, along with the kind that each of its bytes should
    /// get.
    #[derive(Debug, Clone, Default)]
    struct Doc {
        text: String,
        want: Vec<Option<Kind>>,
    }

    impl Doc {
        fn push(&mut self, text: &str, kind: Option<Kind>) {
            self.text.push_str(text);
            self.want.extend(std::iter::repeat_n(kind, text.len()));
        }

        fn append(&mut self, other: &Doc) {
            self.text.push_str(&other.text);
            self.want.extend(&other.want);
        }
    }

    type Pieces = &'static [(&'static str, Option<Kind>)];

    /// One of `choices`, each made of pieces of text and their kinds.
    fn pick(choices: &'static [Pieces]) -> BoxedStrategy<Doc> {
        prop::sample::select(choices)
            .prop_map(|pieces| {
                let mut doc = Doc::default();
                for &(text, kind) in pieces {
                    doc.push(text, kind);
                }
                doc
            })
            .boxed()
    }

    /// Whitespace and comments, which can come between any two tokens.
    fn space() -> BoxedStrategy<Doc> {
        const C: Option<Kind> = Some(Kind::Comment);
        pick(&[
            &[],
            &[(" ", None)],
            &[("\n  ", None)],
            &[("\r\n\t", None)],
            &[(" ", None), ("// c \"a\": 1, /*", C), ("\n", None)],
            &[("// x", C), ("\r\n", None)],
            &[("/* [1, \"x\": */", C)],
            &[("/*\n * \"a\": 1\n */", C), (" ", None)],
        ])
    }

    /// The space between a key and its `:`.
    fn inline_space() -> BoxedStrategy<Doc> {
        pick(&[&[], &[(" ", None)], &[("\t ", None)]])
    }

    /// A string with the given kind, and escapes in it with the given
    /// kind.
    fn string(text: Option<Kind>, escape: Option<Kind>) -> BoxedStrategy<Doc> {
        let part = prop::sample::select(vec![
            ("a", text),
            ("é 中👍", text),
            (" ", text),
            ("{[:,]} ", text),
            ("// /* ", text),
            ("'", text),
            ("true", text),
            ("\\n", escape),
            ("\\\"", escape),
            ("\\\\", escape),
            ("\\/", escape),
            ("\\u00e9", escape),
            ("\\uD83D\\uDE00", escape),
        ]);
        prop::collection::vec(part, 0..5)
            .prop_map(move |parts| {
                let mut doc = Doc::default();
                doc.push("\"", text);
                for (t, k) in parts {
                    doc.push(t, k);
                }
                doc.push("\"", text);
                doc
            })
            .boxed()
    }

    fn value() -> BoxedStrategy<Doc> {
        const N: Option<Kind> = Some(Kind::Number);
        const K: Option<Kind> = Some(Kind::Constant);
        let scalar = prop_oneof![
            pick(&[
                &[("0", N)],
                &[("-0", N)],
                &[("12", N)],
                &[("-3.25", N)],
                &[("1e9", N)],
                &[("6.02E+23", N)],
                &[("1.5e-3", N)],
                &[("true", K)],
                &[("false", K)],
                &[("null", K)],
            ]),
            string(Some(Kind::String), Some(Kind::Escape)),
        ];
        scalar
            .prop_recursive(3, 16, 4, |inner| {
                let p = Some(Kind::Punctuation);
                let array = (
                    prop::collection::vec((space(), inner.clone(), space()), 0..4),
                    space(),
                )
                    .prop_map(move |(items, empty)| {
                        let mut doc = Doc::default();
                        doc.push("[", p);
                        if items.is_empty() {
                            doc.append(&empty);
                        }
                        for (i, (s1, v, s2)) in items.iter().enumerate() {
                            if i > 0 {
                                doc.push(",", p);
                            }
                            doc.append(s1);
                            doc.append(v);
                            doc.append(s2);
                        }
                        doc.push("]", p);
                        doc
                    });
                let entry = (
                    space(),
                    string(None, None),
                    inline_space(),
                    space(),
                    inner,
                    space(),
                );
                let object = (prop::collection::vec(entry, 0..4), space()).prop_map(
                    move |(entries, empty)| {
                        let mut doc = Doc::default();
                        doc.push("{", p);
                        if entries.is_empty() {
                            doc.append(&empty);
                        }
                        for (i, (s1, key, s2, s3, v, s4)) in entries.iter().enumerate() {
                            if i > 0 {
                                doc.push(",", p);
                            }
                            doc.append(s1);
                            doc.append(key);
                            doc.append(s2);
                            doc.push(":", p);
                            doc.append(s3);
                            doc.append(v);
                            doc.append(s4);
                        }
                        doc.push("}", p);
                        doc
                    },
                );
                prop_oneof![array, object]
            })
            .boxed()
    }

    /// One value, or several on lines of their own, as in JSON Lines.
    fn document() -> impl Strategy<Value = Doc> {
        prop::collection::vec((space(), value(), space()), 1..3).prop_map(|values| {
            let mut doc = Doc::default();
            for (i, (s1, v, s2)) in values.iter().enumerate() {
                if i > 0 {
                    doc.push("\n", None);
                }
                doc.append(s1);
                doc.append(v);
                doc.append(s2);
            }
            doc
        })
    }

    proptest! {
        /// Every byte of a valid document gets the kind of what it's part
        /// of. Whitespace in comments can be plain, since comments leave
        /// out the whitespace at the end of each line.
        #[test]
        fn colors_valid_documents(doc in document()) {
            let mut out = Tokens::default();
            lex(&doc.text, &mut out);
            let mut got = vec![None; doc.text.len()];
            for (r, k) in out.0 {
                got[r].fill(Some(k));
            }
            for (i, (g, w)) in got.iter().zip(&doc.want).enumerate() {
                let space = doc.text.as_bytes()[i].is_ascii_whitespace();
                let ok = g == w || (*w == Some(Kind::Comment) && g.is_none() && space);
                prop_assert!(ok, "byte {} of {:?} is {:?}, not {:?}", i, doc.text, g, w);
            }
        }
    }
}
