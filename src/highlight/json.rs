//! A lexer for JSON.
//!
//! It follows RFC 8259, and also reads `//` and `/* */` comments, which
//! files such as `tsconfig.json` and VS Code's settings use. The name in
//! a key is an identifier, so it gets a color from its text, and its
//! quotes are plain. The lexer keeps a stack of the objects and arrays
//! it's inside of, so that it knows where a key comes.

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
enum Token {
    #[regex(r"//[^\n]*", allow_greedy = true)]
    #[token("/*", block_comment)]
    Comment,

    // Whether the string has its closing quote.
    #[token("\"", string)]
    Str(bool),

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
fn string(lex: &mut Lexer<Token>) -> bool {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' if rest.get(i + 1).is_some_and(|&b| b != b'\n' && b != b'\r') => i += 2,
            b'"' => {
                lex.bump(i + 1);
                return true;
            }
            b'\n' | b'\r' => break,
            _ => i += 1,
        }
    }
    lex.bump(i);
    false
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

/// The brackets the lexer is inside of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Object,
    Array,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    let mut stack = Vec::new();
    // Whether a string here is a key, as it is after `{` and after a comma
    // in an object. Comments don't change it.
    let mut key = false;
    while let Some(token) = lex.next() {
        let span = lex.span();
        if token == Ok(Token::Comment) {
            out.push(span, Kind::Comment);
            continue;
        }
        let was_key = std::mem::replace(&mut key, false);
        let kind = match token {
            Ok(Token::Str(closed)) if was_key => {
                // The quotes are plain.
                let end = span.end - usize::from(closed);
                out.push(span.start + 1..end, Kind::Identifier);
                continue;
            }
            Ok(Token::Str(_)) => {
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
            Ok(Token::Punctuation) => {
                match lex.slice() {
                    "{" => {
                        stack.push(Open::Object);
                        key = true;
                    }
                    "[" => stack.push(Open::Array),
                    "," => key = stack.last() == Some(&Open::Object),
                    ":" => {}
                    close => {
                        // A bracket that doesn't close the innermost one
                        // is an error in the file. It closes everything
                        // back to the innermost bracket of its own kind,
                        // if there is one.
                        let open = if close == "}" {
                            Open::Object
                        } else {
                            Open::Array
                        };
                        if let Some(i) = stack.iter().rposition(|&o| o == open) {
                            stack.truncate(i);
                        }
                    }
                }
                Kind::Punctuation
            }
            Ok(Token::Comment | Token::Word) | Err(()) => continue,
        };
        out.push(span, kind);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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
    fn marks_plain_identifiers() {
        // A string is a key where an object has one, after `{` and after
        // a comma. Its `:` can come on a later line. The text between its
        // quotes is an identifier, escapes and all, and the quotes are
        // plain.
        let text = "{\"a\\\"b\" \t: \"c\", \"d\"\n: 1, \"\": [\"e\", {\"f\": \"g\"}]}";
        assert_eq!(identifiers(text), ["a\\\"b", "d", "f"]);
        assert_eq!(
            tokens(text),
            [
                ("{", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("\"c\"", Kind::String),
                (",", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("1", Kind::Number),
                (",", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("[", Kind::Punctuation),
                ("\"e\"", Kind::String),
                (",", Kind::Punctuation),
                ("{", Kind::Punctuation),
                (":", Kind::Punctuation),
                ("\"g\"", Kind::String),
                ("}]}", Kind::Punctuation),
            ]
        );
        // Outside of an object, a string is a value even with a `:` after
        // it, which only a broken file has.
        assert_eq!(identifiers("\"a\": 1\n[\"b\": 2]"), [] as [&str; 0]);
        // A key without its closing quote keeps all of its text.
        assert_eq!(identifiers("{\"ab\n: 1}"), ["ab"]);
        // A bracket that closes the wrong kind closes everything back to
        // its own kind, so the keys after it are still keys.
        assert_eq!(identifiers("{\"a\": [1}, {\"b\": 2}"), ["a", "b"]);
        assert_eq!(identifiers("{\"a\": [1, \"b\"}, \"c\"]"), ["a"]);
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
                ("// a \"b\": 1\r", Kind::Comment),
                ("/* c\n  d */", Kind::Comment),
                (":", Kind::Punctuation),
                ("/* f */", Kind::Comment),
                ("2", Kind::Number),
                ("/* g", Kind::Comment),
            ]
        );
        // Comments before and after a key leave it a key.
        let text = "{ /* \"x\" */ \"a\" // y\n : 1, /**/ \"b\" /**/ : 2 }";
        assert_eq!(identifiers(text), ["a", "b"]);
        assert_eq!(kinds_of(text, Kind::String), [] as [&str; 0]);
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
            &[("// x\r", C), ("\n", None)],
            &[("/* [1, \"x\": */", C)],
            &[("/*\n * \"a\": 1\n */", C), (" ", None)],
        ])
    }

    /// A string whose quotes have the kind `quote`, whose text has the
    /// kind `text`, and whose escapes have the kind `escape`.
    fn string(quote: Option<Kind>, text: Option<Kind>, escape: Option<Kind>) -> BoxedStrategy<Doc> {
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
                doc.push("\"", quote);
                for (t, k) in parts {
                    doc.push(t, k);
                }
                doc.push("\"", quote);
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
            string(Some(Kind::String), Some(Kind::String), Some(Kind::Escape)),
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
                    string(None, Some(Kind::Identifier), Some(Kind::Identifier)),
                    space(),
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
        /// of.
        #[test]
        fn colors_valid_documents(doc in document()) {
            let mut out = Tokens::default();
            lex(&doc.text, &mut out);
            let mut got = vec![None; doc.text.len()];
            for (r, k) in out.0 {
                got[r].fill(Some(k));
            }
            for (i, (g, w)) in got.iter().zip(&doc.want).enumerate() {
                prop_assert!(g == w, "byte {} of {:?} is {:?}, not {:?}", i, doc.text, g, w);
            }
        }
    }
}
