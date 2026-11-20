//! A lexer for C and C++.
//!
//! It follows the translation phases of the C and C++ standards as far as
//! colors need. A backslash at the end of a line joins it to the next, so a
//! `//` comment or a directive can go on over several lines. A `#` at the
//! start of a line begins a directive, and the lexer keeps track of which
//! directive it is in, because that changes how some tokens read. The code
//! between `#if 0` and its `#endif` gets the comment color.
//!
//! C and C++ share the lexer. They differ in their keywords, and only C++
//! has raw strings.

use std::ops::Range;

use logos::{Lexer, Logos};

use super::{Kind, Language, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(extras = Language)]
#[logos(skip r"[ \t\f\x0B\r]+")]
// A line splice. C++23 and the compilers allow whitespace between the
// backslash and the end of the line.
#[logos(skip r"\\[ \t\f\x0B]*\r?\n")]
// A universal character name, which can stand for a letter in a name.
#[logos(subpattern ucn = r"\\u[0-9a-fA-F]{4}|\\U[0-9a-fA-F]{8}")]
enum Token {
    // Line breaks end directives, so they aren't skipped.
    #[token("\n")]
    Newline,

    #[token("//", line_comment)]
    #[token("/*", block_comment)]
    Comment,

    #[regex(r#"(u8|[uUL])?""#, |lex| quoted(lex, b'"'))]
    #[regex(r#"(u8|[uUL])?'"#, |lex| quoted(lex, b'\''))]
    Str,

    // A raw string's delimiter has at most 16 characters, and can't hold
    // spaces, parentheses or backslashes. The standard allows quotes in
    // it, but we leave them out. In C, which has no raw strings, the `R`
    // is a name and an ordinary string follows it, and that string must
    // not end inside this token. Without a valid delimiter, the quote
    // starts an ordinary string in C++ too.
    #[regex(
        r#"(u8|[uUL])?R"[\x21\x23-\x27\x2A-\x5B\x5D-\x7E]{0,16}\("#,
        raw_string
    )]
    RawStr,

    // A preprocessing number. It takes in letters, dots, signs after an
    // exponent and digit separators, so `0x1e+1` and `1'000` are each one
    // token, as they are for the compiler. Only an ASCII letter, digit or
    // underscore can follow a digit separator.
    #[regex(r"\.?[0-9](\p{XID_Continue}|(?&ucn)|\.|'[0-9A-Za-z_]|[eEpP][+-])*")]
    Number,

    #[token("#")]
    #[token("%:")]
    Hash,

    #[token("[[")]
    Attribute,

    #[regex(r"[-+*/%^&|~!=<>?]")]
    #[regex(r"[-+*/%^&|!=<>]=")]
    #[regex(r"<<=?|>>=?|&&|\|\||\+\+|--|<=>|->\*|\.\*|##|%:%:")]
    Operator,

    // The digraphs `<:` and `:>` are left out. C++ reads `<::` as `<`
    // followed by `::` unless a `:` or `>` comes next, and splitting them
    // only changes the color of the `<`.
    #[regex(r"[()\[\]{};,:.]|::|\.\.\.|->|<%|%>")]
    Punctuation,

    // Compilers allow `$` in names.
    #[regex(r"([\p{XID_Start}_$]|(?&ucn))([\p{XID_Continue}$]|(?&ucn))*")]
    Ident,
}

/// The length of the line splice at the start of `s`, if it starts with
/// one.
fn splice(s: &[u8]) -> Option<usize> {
    let mut i = 1;
    if s.first() != Some(&b'\\') {
        return None;
    }
    while matches!(s.get(i), Some(b' ' | b'\t' | b'\x0C' | b'\x0B')) {
        i += 1;
    }
    if s.get(i) == Some(&b'\r') {
        i += 1;
    }
    (s.get(i) == Some(&b'\n')).then_some(i + 1)
}

/// Skip to the end of the line, or further while lines end in a splice.
fn line_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() && rest[i] != b'\n' {
        i += splice(&rest[i..]).unwrap_or(1);
    }
    lex.bump(i);
}

/// Skip to the end of a block comment. Block comments don't nest.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(rest.find("*/").map_or(rest.len(), |i| i + 2));
}

/// Where the string or character in `rest` ends, after its opening
/// quote. One without a closing quote ends at the end of the line.
fn quoted_end(rest: &[u8], quote: u8) -> usize {
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' => {
                // Splices are removed before escapes are read, so in
                // `\\` at the end of a line, the second backslash joins
                // the lines and the first escapes what comes after.
                if let Some(n) = splice(&rest[i..]) {
                    i += n;
                    continue;
                }
                i += 1;
                while let Some(n) = splice(&rest[i..]) {
                    i += n;
                }
                if rest.get(i) != Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    rest.len()
}

fn quoted(lex: &mut Lexer<Token>, quote: u8) {
    lex.bump(quoted_end(lex.remainder().as_bytes(), quote));
}

/// Skip to a `)` followed by the delimiter and a quote. Splices in a raw
/// string are undone, so the string ignores them. C has no raw strings, so
/// there the `R` is a name and an ordinary string follows it.
fn raw_string(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    if lex.extras == Language::C {
        lex.bump(quoted_end(rest.as_bytes(), b'"'));
        return;
    }
    let slice = lex.slice();
    let delim = &slice[slice.find('"').map_or(0, |i| i + 1)..slice.len() - 1];
    let close = format!("){delim}\"");
    lex.bump(rest.find(&close).map_or(rest.len(), |i| i + close.len()));
}

/// The pieces of a string or character literal. Anything else in one,
/// such as a backslash before a letter that can't be escaped, is plain
/// string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    // The last two are C++23's delimited escapes, such as `\x{7f}` and
    // `\N{LATIN SMALL LETTER A}`, and a line splice.
    #[regex(
        r#"\\([abfnrtv'"?\\]|[0-7][0-7]?[0-7]?|x[0-9a-fA-F]+|u[0-9a-fA-F]{4}|U[0-9a-fA-F]{8})"#
    )]
    #[regex(r#"\\([oxuN]\{[^}\n'"]*\}|[ \t\f\x0B]*\r?\n)"#)]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

/// What the rest of a directive's line means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    /// Not in a directive.
    None,
    /// After the `#`, before the directive's name.
    Name,
    /// After `#include` and the like, where `<...>` is a header name.
    Include,
    /// After `#if` and `#elif`, where `defined` is a keyword. It holds
    /// whether the condition so far is just `0`, once it has a token.
    Cond(Option<bool>),
    /// After `#error` and `#warning`, whose message is text.
    Message,
    /// Any other directive.
    Other,
}

/// Code that the preprocessor skips after an `#if 0`.
struct Skip {
    /// Where the line after the `#if 0` starts.
    start: usize,
    /// How many conditionals inside it are still open.
    depth: u32,
}

struct State<'a> {
    text: &'a str,
    lang: Language,
    out: &'a mut Tokens,
    /// Whether only whitespace and comments came before on this line, so
    /// that a `#` starts a directive.
    at_start: bool,
    directive: Directive,
    /// The `#` of the directive we're in.
    hash: Range<usize>,
    skip: Option<Skip>,
}

impl State<'_> {
    fn token(&mut self, token: Result<Token, ()>, span: Range<usize>) {
        let slice = &self.text[span.clone()];
        match token {
            Ok(Token::Newline) => {
                if self.skip.is_none() && self.directive == Directive::Cond(Some(true)) {
                    self.skip = Some(Skip {
                        start: span.end,
                        depth: 0,
                    });
                }
                self.directive = Directive::None;
                self.at_start = true;
                return;
            }
            Ok(Token::Comment) => {
                if self.skip.is_none() {
                    self.out.push_comment(self.text, span);
                }
                return;
            }
            _ => {}
        }
        if std::mem::replace(&mut self.at_start, false) && token == Ok(Token::Hash) {
            self.directive = Directive::Name;
            self.hash = span.clone();
            if self.skip.is_none() {
                self.out.push(span, Kind::Keyword);
            }
            return;
        }
        if self.directive == Directive::Name {
            // A `#` followed by something else, such as the line number
            // in `# 1 "file.c"`, isn't a named directive.
            self.directive = Directive::Other;
            if token == Ok(Token::Ident) {
                self.name(span);
                return;
            }
        }
        if self.skip.is_some() {
            return;
        }
        match self.directive {
            Directive::Message => {
                self.out.push(span, Kind::String);
                return;
            }
            Directive::Include => {
                self.directive = Directive::Other;
                // Backslashes in a header name aren't escapes.
                if token == Ok(Token::Str) {
                    self.out.push(span, Kind::String);
                    return;
                }
            }
            Directive::Cond(zero) => {
                let is_zero = token == Ok(Token::Number) && slice == "0";
                self.directive = Directive::Cond(Some(zero.is_none() && is_zero));
                if token == Ok(Token::Ident) && slice == "defined" {
                    self.out.push(span, Kind::Keyword);
                    return;
                }
            }
            _ => {}
        }
        let kind = match token {
            Ok(Token::Str) => {
                self.string(span);
                return;
            }
            Ok(Token::RawStr) if self.lang == Language::C => {
                let quote = slice.find('"').unwrap_or(0);
                self.string(span.start + quote..span.end);
                return;
            }
            Ok(Token::RawStr) => Kind::String,
            Ok(Token::Number) => Kind::Number,
            Ok(Token::Attribute) => Kind::Attribute,
            Ok(Token::Hash | Token::Operator) => Kind::Operator,
            Ok(Token::Punctuation) => Kind::Punctuation,
            Ok(Token::Ident) => word(self.lang, slice).unwrap_or(Kind::Identifier),
            Ok(Token::Newline | Token::Comment) | Err(()) => return,
        };
        self.out.push(span, kind);
    }

    /// Handle the name of a directive.
    fn name(&mut self, span: Range<usize>) {
        let name = &self.text[span.clone()];
        if let Some(skip) = &mut self.skip {
            let ends = match name {
                "if" | "ifdef" | "ifndef" => {
                    skip.depth += 1;
                    false
                }
                "endif" if skip.depth > 0 => {
                    skip.depth -= 1;
                    false
                }
                "endif" | "else" | "elif" | "elifdef" | "elifndef" => skip.depth == 0,
                _ => false,
            };
            if !ends {
                return;
            }
            self.out
                .push_comment(self.text, skip.start..self.hash.start);
            self.out.push(self.hash.clone(), Kind::Keyword);
            self.skip = None;
        }
        self.directive = match name {
            "include" | "include_next" | "import" | "embed" => Directive::Include,
            "if" | "elif" => Directive::Cond(None),
            "error" | "warning" => Directive::Message,
            "define" | "undef" | "ifdef" | "ifndef" | "elifdef" | "elifndef" | "else" | "endif"
            | "line" | "pragma" => Directive::Other,
            _ => return,
        };
        self.out.push(span, Kind::Keyword);
    }

    /// Color a string or character literal and the escapes in it.
    fn string(&mut self, span: Range<usize>) {
        for (part, r) in Part::lexer(&self.text[span.clone()]).spanned() {
            let kind = match part {
                Ok(Part::Escape) => Kind::Escape,
                _ => Kind::String,
            };
            self.out
                .push(span.start + r.start..span.start + r.end, kind);
        }
    }

    fn finish(&mut self) {
        if let Some(skip) = &self.skip {
            self.out
                .push_comment(self.text, skip.start..self.text.len());
        }
    }
}

/// The color of a word, if it's a keyword in `lang`. C23 made `true`,
/// `false` and `bool` keywords, so we color them in C too.
fn word(lang: Language, w: &str) -> Option<Kind> {
    let cpp = lang == Language::Cpp;
    match w {
        "true" | "false" | "nullptr" => Some(Kind::Constant),
        "alignas" | "alignof" | "auto" | "bool" | "break" | "case" | "char" | "const"
        | "constexpr" | "continue" | "default" | "do" | "double" | "else" | "enum" | "extern"
        | "float" | "for" | "goto" | "if" | "inline" | "int" | "long" | "register" | "return"
        | "short" | "signed" | "sizeof" | "static" | "static_assert" | "struct" | "switch"
        | "thread_local" | "typedef" | "union" | "unsigned" | "void" | "volatile" | "while"
        | "_Pragma" => Some(Kind::Keyword),
        "restrict" | "typeof" | "typeof_unqual" | "_Alignas" | "_Alignof" | "_Atomic"
        | "_BitInt" | "_Bool" | "_Complex" | "_Decimal128" | "_Decimal32" | "_Decimal64"
        | "_Generic" | "_Imaginary" | "_Noreturn" | "_Static_assert" | "_Thread_local"
            if !cpp =>
        {
            Some(Kind::Keyword)
        }
        "asm" | "catch" | "char8_t" | "char16_t" | "char32_t" | "class" | "co_await"
        | "co_return" | "co_yield" | "concept" | "const_cast" | "consteval" | "constinit"
        | "decltype" | "delete" | "dynamic_cast" | "explicit" | "export" | "friend" | "mutable"
        | "namespace" | "new" | "noexcept" | "operator" | "private" | "protected" | "public"
        | "reinterpret_cast" | "requires" | "static_cast" | "template" | "this" | "throw"
        | "try" | "typeid" | "typename" | "using" | "virtual" | "wchar_t"
            if cpp =>
        {
            Some(Kind::Keyword)
        }
        // C++ has words for some operators, such as `and` for `&&`.
        "and" | "and_eq" | "bitand" | "bitor" | "compl" | "not" | "not_eq" | "or" | "or_eq"
        | "xor" | "xor_eq"
            if cpp =>
        {
            Some(Kind::Operator)
        }
        _ => None,
    }
}

pub fn lex(text: &str, lang: Language, out: &mut Tokens) {
    let mut lex = Token::lexer_with_extras(text, lang);
    let mut state = State {
        text,
        lang,
        out,
        at_start: true,
        directive: Directive::None,
        hash: 0..0,
        skip: None,
    };
    while let Some(token) = lex.next() {
        let span = lex.span();
        // A header name such as `<stdio.h>` reads up to the `>` on the
        // same line.
        if state.skip.is_none()
            && state.directive == Directive::Include
            && lex.slice().starts_with('<')
        {
            let line = text[span.start..].split('\n').next().unwrap_or_default();
            if let Some(i) = line.find('>') {
                let end = span.start + i + 1;
                lex.bump(end.saturating_sub(span.end));
                state.out.push(span.start..end, Kind::String);
                state.directive = Directive::Other;
                continue;
            }
        }
        state.token(token, span);
    }
    state.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each token but the identifiers, as its text and kind. Tokens of
    /// the same kind with nothing between them come out as one.
    fn tokens_in(lang: Language, text: &str) -> Vec<(&str, Kind)> {
        let mut out = Tokens::default();
        lex(text, lang, &mut out);
        out.colored(text)
    }

    fn tokens(text: &str) -> Vec<(&str, Kind)> {
        tokens_in(Language::Cpp, text)
    }

    fn kinds_of(lang: Language, text: &str, kind: Kind) -> Vec<&str> {
        tokens_in(lang, text)
            .into_iter()
            .filter(|&(_, k)| k == kind)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn lexes_a_function() {
        use Kind::*;
        assert_eq!(
            tokens("template <typename T> T *f(const T &x) { return x->y + this; }"),
            [
                ("template", Keyword),
                ("<", Operator),
                ("typename", Keyword),
                (">", Operator),
                ("*", Operator),
                ("(", Punctuation),
                ("const", Keyword),
                ("&", Operator),
                (")", Punctuation),
                ("{", Punctuation),
                ("return", Keyword),
                ("->", Punctuation),
                ("+", Operator),
                ("this", Keyword),
                (";", Punctuation),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn keywords_depend_on_the_language() {
        let text = "class new this restrict _Bool bool true nullptr and int size_t";
        assert_eq!(
            kinds_of(Language::C, text, Kind::Keyword),
            ["restrict", "_Bool", "bool", "int"]
        );
        assert_eq!(
            kinds_of(Language::Cpp, text, Kind::Keyword),
            ["class", "new", "this", "bool", "int"]
        );
        assert_eq!(kinds_of(Language::C, text, Kind::Operator), [] as [&str; 0]);
        assert_eq!(kinds_of(Language::Cpp, text, Kind::Operator), ["and"]);
        for lang in [Language::C, Language::Cpp] {
            assert_eq!(kinds_of(lang, text, Kind::Constant), ["true", "nullptr"]);
        }
        assert_eq!(
            kinds_of(Language::Cpp, "interface $if ifé", Kind::Keyword),
            [] as [&str; 0]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("/* a /* b */ c */"),
            [("/* a /* b */", Kind::Comment), ("*/", Kind::Operator)]
        );
        // A backslash at the end of a line carries a line comment on to
        // the next, even with spaces after it.
        assert_eq!(
            tokens("// a \\\nb\n// c \\  \r\nd\n// e\\x\nf"),
            [
                ("// a \\", Kind::Comment),
                ("b", Kind::Comment),
                ("// c \\", Kind::Comment),
                ("d", Kind::Comment),
                ("// e\\x", Kind::Comment),
            ]
        );
        assert_eq!(tokens("/* open\n"), [("/* open", Kind::Comment)]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#""a\n\x41\101\0\u00e9\U0001F600\q" L"b" u8"c""#),
            [
                ("\"a", Kind::String),
                ("\\n\\x41\\101\\0\\u00e9\\U0001F600", Kind::Escape),
                ("\\q\"", Kind::String),
                ("L\"b\"", Kind::String),
                ("u8\"c\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens(r#""\x{7f}\o{17}\N{LATIN SMALL LETTER A}\u12""#),
            [
                ("\"", Kind::String),
                ("\\x{7f}\\o{17}\\N{LATIN SMALL LETTER A}", Kind::Escape),
                ("\\u12\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens(r"'a' '\'' L'x' u'\\' 'ab'"),
            [
                ("'a'", Kind::String),
                ("'", Kind::String),
                ("\\'", Kind::Escape),
                ("'", Kind::String),
                ("L'x'", Kind::String),
                ("u'", Kind::String),
                ("\\\\", Kind::Escape),
                ("'", Kind::String),
                ("'ab'", Kind::String),
            ]
        );
        // A string without a closing quote ends at the end of the line,
        // unless the line ends in a splice.
        assert_eq!(
            tokens("\"open\nint \"a\\\nb\""),
            [
                ("\"open", Kind::String),
                ("int", Kind::Keyword),
                ("\"a", Kind::String),
                ("\\\n", Kind::Escape),
                ("b\"", Kind::String),
            ]
        );
        assert_eq!(tokens("\"open\\"), [("\"open\\", Kind::String)]);
        // Splices are removed before escapes are read. So in `\\` at the
        // end of a line, the second backslash joins the lines and the
        // string goes on. We still color `\\` as the escape.
        assert_eq!(
            tokens("\"a\\\\\nb\" c"),
            [
                ("\"a", Kind::String),
                ("\\\\", Kind::Escape),
                ("\nb\"", Kind::String),
            ]
        );
        assert_eq!(
            tokens("\"a\\\\\\\nb\" c"),
            [
                ("\"a", Kind::String),
                ("\\\\\\\n", Kind::Escape),
                ("b\"", Kind::String),
            ]
        );
        // A name right after a string is a suffix in C++ and a macro in C.
        // Either way it's plain.
        assert_eq!(
            tokens(r#""%"PRId64 "s"sv"#),
            [("\"%\"", Kind::String), ("\"s\"", Kind::String)]
        );
    }

    #[test]
    fn raw_strings() {
        assert_eq!(
            tokens(r#"R"(a\n)" R"x(a)"b)x" u8R"--(c)-)--" LR"(d"#),
            [
                (r#"R"(a\n)""#, Kind::String),
                (r#"R"x(a)"b)x""#, Kind::String),
                (r#"u8R"--(c)-)--""#, Kind::String),
                (r#"LR"(d"#, Kind::String),
            ]
        );
        assert_eq!(
            tokens("R\"(multi\nline\\\n)\" int"),
            [
                ("R\"(multi\nline\\\n)\"", Kind::String),
                ("int", Kind::Keyword)
            ]
        );
        // Without a valid delimiter, the R is a name.
        assert_eq!(
            tokens(r#"R"a b(x)a b" R"\(x)\""#),
            [
                (r#""a b(x)a b""#, Kind::String),
                ("\"\\(x)", Kind::String),
                ("\\\"", Kind::Escape),
            ]
        );
        assert_eq!(
            tokens_in(Language::C, r#"R"(a\n)" u8R"x(b)x""#),
            [
                ("\"(a", Kind::String),
                ("\\n", Kind::Escape),
                (")\"", Kind::String),
                ("\"x(b)x\"", Kind::String),
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                Language::Cpp,
                "0x1F 1'000'000 1.5e+10f 0x1.8p-3 .5 10ULL 0b1010 12_km 0x1e+1 1..2 a.b 1-2",
                Kind::Number
            ),
            [
                "0x1F",
                "1'000'000",
                "1.5e+10f",
                "0x1.8p-3",
                ".5",
                "10ULL",
                "0b1010",
                "12_km",
                "0x1e+1",
                "1..2",
                "1",
                "2"
            ]
        );
        // A quote after a number only joins it before an ASCII digit,
        // letter or underscore.
        assert_eq!(kinds_of(Language::C, "1'é'", Kind::Number), ["1"]);
        assert_eq!(
            tokens("f(1,'a') 1'"),
            [
                ("(", Kind::Punctuation),
                ("1", Kind::Number),
                (",", Kind::Punctuation),
                ("'a'", Kind::String),
                (")", Kind::Punctuation),
                ("1", Kind::Number),
                ("'", Kind::String),
            ]
        );
    }

    #[test]
    fn operators_and_attributes() {
        assert_eq!(
            kinds_of(
                Language::Cpp,
                "a <<= b <=> c ->* d .* e ++ f ? g : h ## i",
                Kind::Operator
            ),
            ["<<=", "<=>", "->*", ".*", "++", "?", "##"]
        );
        assert_eq!(
            tokens("[[nodiscard]] a[b[0]] std::vector<::x> f(...)"),
            [
                ("[[", Kind::Attribute),
                ("]]", Kind::Punctuation),
                ("[", Kind::Punctuation),
                ("[", Kind::Punctuation),
                ("0", Kind::Number),
                ("]]", Kind::Punctuation),
                ("::", Kind::Punctuation),
                ("<", Kind::Operator),
                ("::", Kind::Punctuation),
                (">", Kind::Operator),
                ("(...)", Kind::Punctuation),
            ]
        );
    }

    #[test]
    fn directives() {
        use Kind::*;
        assert_eq!(
            tokens("#include <a\\b.h>\n  # include \"c\\d.h\" // x\n%:include<e>"),
            [
                ("#include", Keyword),
                ("<a\\b.h>", String),
                ("#", Keyword),
                ("include", Keyword),
                ("\"c\\d.h\"", String),
                ("// x", Comment),
                ("%:include", Keyword),
                ("<e>", String),
            ]
        );
        // A `#` in a macro, or after other tokens on a line, is an operator.
        assert_eq!(
            tokens("#define S(x) #x ## y\nx # y"),
            [
                ("#define", Keyword),
                ("(", Punctuation),
                (")", Punctuation),
                ("#", Operator),
                ("##", Operator),
                ("#", Operator),
            ]
        );
        assert_eq!(
            tokens("#if defined(X) && \\\n defined Y\nint defined;"),
            [
                ("#if", Keyword),
                ("defined", Keyword),
                ("(", Punctuation),
                (")", Punctuation),
                ("&&", Operator),
                ("defined", Keyword),
                ("int", Keyword),
                (";", Punctuation),
            ]
        );
        // A comment before the `#` doesn't stop it starting a directive,
        // but code before it does, even on an earlier line that a comment
        // joins to this one.
        assert_eq!(
            kinds_of(
                Language::Cpp,
                "/* a */ #pragma once\nx; /*\n*/ #define",
                Keyword
            ),
            ["#pragma"]
        );
        // The message of an `#error` is text, even with a lone quote.
        assert_eq!(
            tokens("#error don't use \"this\" // c\nint"),
            [
                ("#error", Keyword),
                ("don't use \"this\" // c", String),
                ("int", Keyword),
            ]
        );
        assert_eq!(
            tokens("# 1 \"a.c\"\n#\n#foo"),
            [
                ("#", Keyword),
                ("1", Number),
                ("\"a.c\"", String),
                ("#", Keyword),
                ("#", Keyword),
            ]
        );
    }

    #[test]
    fn if_zero_is_a_comment() {
        use Kind::*;
        let text = "#if 0 // off\n\
                    int x; /* a */ \"s\n\
                    #ifdef A\n\
                    #else\n\
                    #endif\n\
                    /* b */ #else\n\
                    int y;\n\
                    #endif\n";
        assert_eq!(
            tokens(text),
            [
                ("#if", Keyword),
                ("0", Number),
                ("// off", Comment),
                ("int x; /* a */ \"s", Comment),
                ("#ifdef A", Comment),
                ("#else", Comment),
                ("#endif", Comment),
                ("/* b */", Comment),
                ("#else", Keyword),
                ("int", Keyword),
                (";", Punctuation),
                ("#endif", Keyword),
            ]
        );
        assert_eq!(
            tokens("#if 1\n#elif 0\nx\n#endif\n#if 0 || 1\ny"),
            [
                ("#if", Keyword),
                ("1", Number),
                ("#elif", Keyword),
                ("0", Number),
                ("x", Comment),
                ("#endif", Keyword),
                ("#if", Keyword),
                ("0", Number),
                ("||", Operator),
                ("1", Number),
            ]
        );
        // A comment can hide the `#endif`, and the code runs on to the end.
        assert_eq!(
            tokens("#if 0\n/*\n#endif */\nint  \n"),
            [
                ("#if", Keyword),
                ("0", Number),
                ("/*", Comment),
                ("#endif */", Comment),
                ("int", Comment),
            ]
        );
    }

    /// Names that get no other color are identifiers, one token each,
    /// except in code that `#if 0` leaves out.
    #[test]
    fn marks_plain_identifiers() {
        let text = "#include <a.h>\n#if defined(A) && B\nint n = f(s, t);\n#endif\n#if 0\nx\n#endif\nclass C";
        let mut out = Tokens::default();
        lex(text, Language::C, &mut out);
        assert_eq!(
            out.identifiers(text),
            ["A", "B", "n", "f", "s", "t", "class", "C"]
        );
        let mut out = Tokens::default();
        lex(text, Language::Cpp, &mut out);
        assert_eq!(out.identifiers(text), ["A", "B", "n", "f", "s", "t", "C"]);
    }
}
