//! A lexer for JavaScript.
//!
//! It follows the ECMAScript standard closely enough to find where
//! comments, strings, template literals and regular expressions start and
//! end, but it doesn't check that they're valid.
//!
//! A `/` can start a regular expression or divide, and only the parser
//! knows which. So we guess from the token before it, and we keep a stack
//! of the brackets we're inside of to make better guesses after `)` and
//! `}` (see `Prev`). Without a parser we also can't know what a name
//! stands for, so every name that isn't a keyword gets a color from its
//! spelling (see `Kind::Identifier`).

use logos::{Lexer, Logos};

use super::{Kind, Tokens};

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
// White space and line terminators, as the standard lists them.
#[logos(skip r"[\t\n\x0B\x0C\r \u{A0}\u{FEFF}\u{2028}\u{2029}\p{Zs}]+")]
#[logos(subpattern ident = r"[\p{ID_Start}$_][\p{ID_Continue}$\u{200C}\u{200D}]*")]
enum Token {
    #[regex(r"//[^\n\r\u{2028}\u{2029}]*", allow_greedy = true)]
    #[token("/*", block_comment)]
    Comment,

    #[token("'", quoted)]
    #[token("\"", quoted)]
    Str,

    // Only the opening backquote. `lex` reads the rest of the template,
    // because a `${` in it holds code.
    #[token("`")]
    Template,

    #[regex(r"0[xX][0-9a-fA-F_]+n?")]
    #[regex(r"0[oO][0-7_]+n?")]
    #[regex(r"0[bB][01_]+n?")]
    #[regex(r"[0-9][0-9_]*(\.[0-9_]*)?([eE][+-]?[0-9_]+)?n?")]
    #[regex(r"\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?")]
    Number,

    // A word is a keyword or a name depending on where it is, so `lex`
    // sorts them out. `#` starts the name of a private class member.
    #[regex(r"#?(?&ident)")]
    Word,

    #[token("++")]
    #[token("--")]
    Step,

    #[regex(r"[-+*/%&|^<>!=]=?")]
    #[regex(r"===|!==|\*\*=?|<<=?|>>>?=?|&&=?|\|\|=?|\?\?=?|=>|\.\.\.|~")]
    Operator,

    // Optional chaining, `?.`, joins the `.` to the `?`, but not in
    // `a?.5:1`, where `.5` is a number.
    #[token("?", optional_chain)]
    Question,

    #[token("(")]
    #[token("[")]
    #[token("{")]
    Open,

    #[token(")")]
    #[token("]")]
    #[token("}")]
    Close,

    #[token(".")]
    Dot,

    #[token(",")]
    #[token(";")]
    #[token(":")]
    Punctuation,

    #[token("@")]
    Decorator,
}

/// Skip to the end of a block comment. Block comments don't nest.
fn block_comment(lex: &mut Lexer<Token>) {
    let rest = lex.remainder();
    lex.bump(rest.find("*/").map_or(rest.len(), |i| i + 2));
}

/// Skip to the closing quote. A string can't go on to the next line
/// unless a backslash comes before the line break, so one without a
/// closing quote ends with its line.
fn quoted(lex: &mut Lexer<Token>) {
    let quote = lex.slice().as_bytes().first().copied();
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' if rest[i + 1..].starts_with(b"\r\n") => i += 3,
            b'\\' => i += 2,
            b'\n' | b'\r' => break,
            b if Some(b) == quote => {
                i += 1;
                break;
            }
            _ => i += 1,
        }
    }
    // A backslash at the very end steps one past it.
    lex.bump(i.min(rest.len()));
}

fn optional_chain(lex: &mut Lexer<Token>) {
    let rest = lex.remainder().as_bytes();
    if rest.first() == Some(&b'.') && !rest.get(1).is_some_and(u8::is_ascii_digit) {
        lex.bump(1);
    }
}

/// How a piece of a template literal ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TemplateEnd {
    /// With the closing backquote.
    Close,
    /// With `${`, which starts code.
    Code,
    /// At the end of the text, without either.
    Open,
}

/// Find where the piece of a template literal at the start of `rest`
/// ends, just after its closing backquote or `${`.
fn template_piece(rest: &str) -> (usize, TemplateEnd) {
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return (i + 1, TemplateEnd::Close),
            b'$' if b.get(i + 1) == Some(&b'{') => return (i + 2, TemplateEnd::Code),
            _ => i += 1,
        }
    }
    (b.len(), TemplateEnd::Open)
}

fn is_line_end(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Find the end of a regular expression whose body starts at the start of
/// `rest`, just after the opening `/`. It ends after its flags. A `/`
/// inside a class such as `[/]` doesn't end it. One without a closing
/// `/` ends with its line.
fn regex_len(rest: &str) -> usize {
    let mut class = false;
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((j, c)) if is_line_end(c) => return j,
                None => return rest.len(),
                Some(_) => {}
            },
            c if is_line_end(c) => return i,
            '[' => class = true,
            ']' => class = false,
            '/' if !class => {
                let flags = &rest[i + 1..];
                let n = flags
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(flags.len());
                return i + 1 + n;
            }
            _ => {}
        }
    }
    rest.len()
}

/// The pieces of a string or template literal. Anything else in one,
/// such as `\x` without two hex digits after it, is plain string.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    #[regex(r"\\x[0-9a-fA-F][0-9a-fA-F]")]
    #[regex(r"\\u[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]")]
    #[regex(r"\\u\{[0-9a-fA-F]+\}")]
    // Old octal escapes such as `\012`, which still work outside of
    // strict mode, and `\0`.
    #[regex(r"\\[0-3][0-7]?[0-7]?")]
    #[regex(r"\\[4-7][0-7]?")]
    #[regex(r"\\\r\n")]
    // A backslash before any other character stands for that character,
    // or skips it if it ends a line.
    #[regex(r"\\[^xu0-7]")]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

/// The pieces of a regular expression, where a backslash escapes any
/// character.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum RegexPart {
    #[regex(r"\\[^\n\r\u{2028}\u{2029}]")]
    Escape,
    #[regex(r"[^\\]+", allow_greedy = true)]
    Text,
}

/// What the token before the current one says about what comes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// A statement starts. That's the case at the start of the text and
    /// after `;`, the braces of a block, the `)` after the condition of
    /// `if`, `while`, `for` or `with`, `=>`, and the keywords `else`,
    /// `do`, `try` and `finally`.
    Statement,
    /// An expression starts, after most operators and keywords.
    Expression,
    /// After `:`, which can end a label or a case, or be part of an
    /// expression or an object literal.
    Colon,
    /// An operand ended, such as a name, a literal, `]`, or the `}` of an
    /// object literal. A `/` after one divides.
    Operand,
    /// After `.` or `?.`, where any word is the name of a property.
    Dot,
    /// After the `{` or a `,` of an object literal, where a word followed
    /// by `:` or `(` is the name of a property.
    Key,
}

/// The keyword that comes before the `(` of a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cond {
    /// `if`, `while` or `with`.
    Plain,
    /// `for`, where `of` is a keyword inside the parentheses.
    For,
}

/// A bracket we're inside of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Paren(Option<Cond>),
    Bracket,
    /// A brace that holds a block, a class body or an object literal. We
    /// only tell the object literals apart.
    Brace {
        object: bool,
    },
    /// The `${` of a template literal. The template goes on after the `}`
    /// that closes it.
    Template,
}

/// Words that are always keywords, except as the name of a property.
/// They're the reserved words of the standard, along with `let`,
/// `static`, `async` and `await`, which are only keywords in some places
/// but are rarely anything else.
const KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// The literals, and the global constants that Sublime Text colors as
/// constants too.
const CONSTANTS: &[&str] = &["true", "false", "null", "undefined", "NaN", "Infinity"];

/// What comes next in the name after the `@` of a decorator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecoratorPath {
    Name,
    Dot,
}

struct State<'a, 'o> {
    text: &'a str,
    out: &'o mut Tokens,
    prev: Prev,
    stack: Vec<Frame>,
    /// The keyword of a condition whose `(` comes next.
    cond: Option<Cond>,
    /// Whether we're in an `import` or `export` declaration, where `as`
    /// and `from` are keywords. It ends at the module name, or at a `;`,
    /// `=` or `(`, which only come after the part with those words.
    module: bool,
    decorator: Option<DecoratorPath>,
}

impl State<'_, '_> {
    /// Color a string literal or a piece of a template.
    fn string(&mut self, start: usize, end: usize) {
        for (part, r) in Part::lexer(&self.text[start..end]).spanned() {
            let kind = match part {
                Ok(Part::Escape) => Kind::Escape,
                _ => Kind::String,
            };
            self.out.push(start + r.start..start + r.end, kind);
        }
    }

    /// Read the rest of a piece of a template literal, starting just after
    /// its backquote or the `}` before it, and color it from `start`.
    fn template(&mut self, lex: &mut Lexer<Token>, start: usize) {
        let (len, end) = template_piece(lex.remainder());
        lex.bump(len);
        let stop = lex.span().end;
        if end == TemplateEnd::Code {
            self.string(start, stop - 2);
            self.out.push(stop - 2..stop, Kind::Punctuation);
            self.stack.push(Frame::Template);
            self.prev = Prev::Expression;
        } else {
            self.string(start, stop);
            self.prev = Prev::Operand;
        }
    }

    /// Whether a `{` here starts an object literal rather than a block.
    fn object_starts(&self) -> bool {
        match self.prev {
            Prev::Statement | Prev::Operand => false,
            Prev::Expression | Prev::Dot | Prev::Key => true,
            // A label or a case comes before a block, but inside an
            // object literal or parentheses, the `:` is part of an
            // expression.
            Prev::Colon => !matches!(
                self.stack.last(),
                None | Some(Frame::Brace { object: false })
            ),
        }
    }

    /// Pop the innermost bracket that `close` matches, along with any
    /// unclosed brackets inside it. A closing bracket that doesn't match
    /// anything changes nothing, since a hunk can start in the middle of
    /// some brackets. Only a `}` can close a template's `${`.
    fn close(&mut self, close: u8) -> Option<Frame> {
        let i = self.stack.iter().rposition(|f| match f {
            Frame::Paren(_) => close == b')',
            Frame::Bracket => close == b']',
            Frame::Brace { .. } => close == b'}',
            Frame::Template => true,
        })?;
        let frame = self.stack[i];
        if frame == Frame::Template && close != b'}' {
            return None;
        }
        self.stack.truncate(i);
        Some(frame)
    }

    fn word(&mut self, word: &str, next: &str) -> Kind {
        let prev = self.prev;
        self.prev = Prev::Operand;
        let cond = self.cond.take();
        let property =
            prev == Prev::Dot || (prev == Prev::Key && next.trim_start().starts_with([':', '(']));
        if property || word.starts_with('#') {
            return Kind::Identifier;
        }
        if CONSTANTS.contains(&word) {
            return Kind::Constant;
        }
        let keyword = KEYWORDS.contains(&word)
            || (self.module && matches!(word, "as" | "from"))
            || (word == "of"
                && prev == Prev::Operand
                && self.stack.last() == Some(&Frame::Paren(Some(Cond::For))));
        if !keyword {
            return Kind::Identifier;
        }
        self.prev = match word {
            "this" | "super" => Prev::Operand,
            "else" | "do" | "try" | "finally" => Prev::Statement,
            _ => Prev::Expression,
        };
        match word {
            "if" | "while" | "with" => self.cond = Some(Cond::Plain),
            "for" => self.cond = Some(Cond::For),
            "await" if cond == Some(Cond::For) => self.cond = cond,
            "import" | "export" => self.module = true,
            _ => {}
        }
        Kind::Keyword
    }
}

pub fn lex(text: &str, out: &mut Tokens) {
    let mut lex = Token::lexer(text);
    // A `#!` line at the very start is a comment, for scripts run as
    // commands.
    if text.starts_with("#!") {
        let end = text.find(is_line_end).unwrap_or(text.len());
        out.push(0..end, Kind::Comment);
        lex.bump(end);
    }
    let mut state = State {
        text,
        out,
        prev: Prev::Statement,
        stack: Vec::new(),
        cond: None,
        module: false,
        decorator: None,
    };
    while let Some(token) = lex.next() {
        let span = lex.span();
        let slice = lex.slice();
        if token == Ok(Token::Comment) {
            state.out.push_comment(text, span);
            continue;
        }
        let decorator = state.decorator.take();
        if token != Ok(Token::Word) && token != Ok(Token::Open) {
            state.cond = None;
        }
        let kind = match token {
            Ok(Token::Word) if decorator == Some(DecoratorPath::Name) => {
                state.decorator = Some(DecoratorPath::Dot);
                state.prev = Prev::Operand;
                Kind::Attribute
            }
            Ok(Token::Dot) if decorator == Some(DecoratorPath::Dot) => {
                state.decorator = Some(DecoratorPath::Name);
                Kind::Attribute
            }
            Ok(Token::Decorator) => {
                state.decorator = Some(DecoratorPath::Name);
                state.prev = Prev::Expression;
                Kind::Attribute
            }
            Ok(Token::Word) => state.word(slice, &text[span.end..]),
            Ok(Token::Str) => {
                state.string(span.start, span.end);
                state.prev = Prev::Operand;
                state.module = false;
                continue;
            }
            Ok(Token::Template) => {
                state.template(&mut lex, span.start);
                continue;
            }
            Ok(Token::Number) => {
                state.prev = Prev::Operand;
                Kind::Number
            }
            Ok(Token::Operator) if slice.starts_with('/') && state.prev != Prev::Operand => {
                let end = span.start + 1 + regex_len(&text[span.start + 1..]);
                lex.bump(end.saturating_sub(span.end));
                for (part, r) in RegexPart::lexer(&text[span.start..end]).spanned() {
                    let kind = match part {
                        Ok(RegexPart::Escape) => Kind::Escape,
                        _ => Kind::String,
                    };
                    state
                        .out
                        .push(span.start + r.start..span.start + r.end, kind);
                }
                state.prev = Prev::Operand;
                continue;
            }
            Ok(Token::Operator) => {
                state.prev = if slice == "=>" {
                    Prev::Statement
                } else {
                    Prev::Expression
                };
                if slice == "=" {
                    state.module = false;
                }
                Kind::Operator
            }
            // `++` and `--` come after an operand or before one, so they
            // don't change what a `/` after them means.
            Ok(Token::Step) => Kind::Operator,
            Ok(Token::Question) => {
                state.prev = if slice == "?." {
                    Prev::Dot
                } else {
                    Prev::Expression
                };
                Kind::Operator
            }
            Ok(Token::Open) => {
                let cond = state.cond.take();
                let frame = match slice {
                    "(" => {
                        state.module = false;
                        Frame::Paren(cond)
                    }
                    "[" => Frame::Bracket,
                    _ => Frame::Brace {
                        object: state.object_starts(),
                    },
                };
                state.prev = if frame == (Frame::Brace { object: true }) {
                    Prev::Key
                } else {
                    Prev::Expression
                };
                state.stack.push(frame);
                Kind::Punctuation
            }
            Ok(Token::Close) => {
                let frame = state.close(slice.as_bytes()[0]);
                if frame == Some(Frame::Template) {
                    state.out.push(span.clone(), Kind::Punctuation);
                    state.template(&mut lex, span.end);
                    continue;
                }
                state.prev = match (slice, frame) {
                    (")", Some(Frame::Paren(Some(_)))) => Prev::Statement,
                    ("}", Some(Frame::Brace { object: false }) | None) => Prev::Statement,
                    _ => Prev::Operand,
                };
                Kind::Punctuation
            }
            Ok(Token::Dot) => {
                state.prev = Prev::Dot;
                Kind::Punctuation
            }
            Ok(Token::Punctuation) => {
                state.prev = match slice {
                    ";" => {
                        state.module = false;
                        Prev::Statement
                    }
                    ":" => Prev::Colon,
                    _ if state.stack.last() == Some(&Frame::Brace { object: true }) => Prev::Key,
                    _ => Prev::Expression,
                };
                Kind::Punctuation
            }
            Ok(Token::Comment) | Err(()) => continue,
        };
        state.out.push(span, kind);
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
        assert_eq!(
            tokens("export async function f(x = 1) { return await g?.(x, null); }"),
            [
                ("export", Keyword),
                ("async", Keyword),
                ("function", Keyword),
                ("(", Punctuation),
                ("=", Operator),
                ("1", Number),
                (")", Punctuation),
                ("{", Punctuation),
                ("return", Keyword),
                ("await", Keyword),
                ("?.", Operator),
                ("(", Punctuation),
                (",", Punctuation),
                ("null", Constant),
                (");", Punctuation),
                ("}", Punctuation),
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            tokens("/* a /* b */ c; /* d"),
            [
                ("/* a /* b */", Kind::Comment),
                (";", Kind::Punctuation),
                ("/* d", Kind::Comment),
            ]
        );
        assert_eq!(
            tokens("x // y \"z\n/** doc */ w"),
            [("// y \"z", Kind::Comment), ("/** doc */", Kind::Comment)]
        );
        // A carriage return ends a line too.
        assert_eq!(
            tokens("// a\r'b'"),
            [("// a", Kind::Comment), ("'b'", Kind::String)]
        );
        assert_eq!(
            tokens("#!/usr/bin/env node\nx"),
            [("#!/usr/bin/env node", Kind::Comment)]
        );
        assert_eq!(kinds_of("x\n#!y", Kind::Comment), [] as [&str; 0]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            tokens(r#"'a\n\'b' "\x7f\u00e9\u{1F600}\q" '\012\8' '\xz'"#),
            [
                ("'a", Kind::String),
                ("\\n\\'", Kind::Escape),
                ("b'", Kind::String),
                ("\"", Kind::String),
                ("\\x7f\\u00e9\\u{1F600}\\q", Kind::Escape),
                ("\"", Kind::String),
                ("'", Kind::String),
                ("\\012\\8", Kind::Escape),
                ("'", Kind::String),
                ("'\\xz'", Kind::String),
            ]
        );
        // A string ends with its line, unless a backslash comes before
        // the line break.
        assert_eq!(
            tokens("'open\nx 'a\\\r\nb' \"c\\\nd"),
            [
                ("'open", Kind::String),
                ("'a", Kind::String),
                ("\\\r\n", Kind::Escape),
                ("b'", Kind::String),
                ("\"c", Kind::String),
                ("\\\n", Kind::Escape),
                ("d", Kind::String),
            ]
        );
        assert_eq!(tokens("'\\"), [("'\\", Kind::String)]);
        assert_eq!(tokens("'\\é"), [("'", Kind::String), ("\\é", Kind::Escape)]);
    }

    #[test]
    fn templates() {
        use Kind::*;
        assert_eq!(
            tokens("`a\\n${x + `b${{c: 1}}`}d\\${e}` / 2"),
            [
                ("`a", String),
                ("\\n", Escape),
                ("${", Punctuation),
                ("+", Operator),
                ("`b", String),
                ("${{", Punctuation),
                (":", Punctuation),
                ("1", Number),
                ("}}", Punctuation),
                ("`", String),
                ("}", Punctuation),
                ("d", String),
                ("\\$", Escape),
                ("{e}`", String),
                ("/", Operator),
                ("2", Number),
            ]
        );
        assert_eq!(
            tokens("`multi\nline` x `open"),
            [("`multi\nline`", String), ("`open", String)]
        );
        // A `)` or `]` doesn't close the code in a template.
        assert_eq!(
            tokens("`${a)]}b`"),
            [
                ("`", String),
                ("${", Punctuation),
                (")]}", Punctuation),
                ("b`", String)
            ]
        );
        // A hunk can start with the end of a template.
        assert_eq!(tokens("a}`b`"), [("}", Punctuation), ("`b`", String)]);
    }

    #[test]
    fn regular_expressions() {
        assert_eq!(
            tokens(r"x = /a\/[/\]]+/gi.test(y)"),
            [
                ("=", Kind::Operator),
                ("/a", Kind::String),
                (r"\/", Kind::Escape),
                ("[/", Kind::String),
                (r"\]", Kind::Escape),
                ("]+/gi", Kind::String),
                (".", Kind::Punctuation),
                ("(", Kind::Punctuation),
                (")", Kind::Punctuation),
            ]
        );
        // One without its closing `/` ends with its line.
        assert_eq!(
            tokens("f(/a\n/ 2)"),
            [
                ("(", Kind::Punctuation),
                ("/a", Kind::String),
                ("/", Kind::Operator),
                ("2", Kind::Number),
                (")", Kind::Punctuation),
            ]
        );
        assert_eq!(kinds_of("/=a/", Kind::String), ["/=a/"]);
    }

    /// Whether a `/` divides or starts a regular expression.
    #[test]
    fn slashes() {
        let regex = |text| {
            kinds_of(text, Kind::String)
                .iter()
                .any(|s| s.starts_with('/'))
        };
        for text in [
            "/a/",
            "x = /a/",
            "f(/a/)",
            "[/a/]",
            "return /a/",
            "typeof /a/",
            "x => /a/",
            "if (x) /a/.test(y)",
            "while (x) /a/",
            "for (;;) /a/",
            "a ? /b/ : /c/",
            "{} /a/",
            "if (x) {} /a/",
            "function f() {} /a/",
            "x;\n/a/",
            "!/a/",
            "case /a/:",
            "`${/a/}`",
            "x = ++/a/.lastIndex",
        ] {
            assert!(regex(text), "{text}");
        }
        for text in [
            "a / b /",
            "a /= b /",
            "1 / 2 /",
            "f(x) / 2 /",
            "a[0] / 2 /",
            "this / 2 /",
            "x.return / 2 /",
            "x.if / 2 /",
            "`a` / 2 /",
            "`${a}` / 2 /",
            "x = {} / 2 /",
            "x = {a: {}} / 2 /",
            "f({}) / 2 /",
            "i++ / 2 /",
            "true / 2 /",
            "#x / 2 /",
            "x = class {} / 2 /",
        ] {
            assert!(!regex(text), "{text}");
        }
        assert_eq!(kinds_of("/a/ / 2 /", Kind::String), ["/a/"]);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds_of(
                "0x1F 0o17 0B1010 1_000 1.5 .5 2e10 3.0E-5 10n 0xffn 017 08.5 1..toString() a?.5:1",
                Kind::Number
            ),
            [
                "0x1F", "0o17", "0B1010", "1_000", "1.5", ".5", "2e10", "3.0E-5", "10n", "0xffn",
                "017", "08.5", "1.", ".5", "1"
            ]
        );
        assert_eq!(kinds_of("a?.b", Kind::Operator), ["?."]);
        assert_eq!(kinds_of("[...a]", Kind::Operator), ["..."]);
    }

    #[test]
    fn keywords_and_names() {
        assert_eq!(
            tokens("this super true undefined NaN let yield foo $ _x"),
            [
                ("this", Kind::Keyword),
                ("super", Kind::Keyword),
                ("true", Kind::Constant),
                ("undefined", Kind::Constant),
                ("NaN", Kind::Constant),
                ("let", Kind::Keyword),
                ("yield", Kind::Keyword),
            ]
        );
        assert_eq!(
            identifiers("x.default; p?.delete(); a.if.new #b c.#d $e é_ifé"),
            [
                "x", "default", "p", "delete", "a", "if", "new", "#b", "c", "#d", "$e", "é_ifé"
            ]
        );
        // Keywords can be the names of properties in an object literal,
        // but not values.
        assert_eq!(
            identifiers("x = {default: a, if: b ? true : c, new() {}}"),
            ["x", "default", "a", "if", "b", "c", "new"]
        );
        assert_eq!(kinds_of("x = {a: this}", Kind::Keyword), ["this"]);
        assert_eq!(kinds_of("({ default: x })", Kind::Keyword), [] as [&str; 0]);
        assert_eq!(
            kinds_of("switch (x) { default: x }", Kind::Keyword),
            ["switch", "default"]
        );
    }

    #[test]
    fn contextual_keywords() {
        assert_eq!(
            kinds_of(
                "import x, * as y from 'z'; export { a as b } from \"c\"; as(from)",
                Kind::Keyword
            ),
            ["import", "as", "from", "export", "as", "from"]
        );
        assert_eq!(
            kinds_of("import('a').then(from)", Kind::Keyword),
            ["import"]
        );
        assert_eq!(
            kinds_of("for (const x of of(xs)) of(x)", Kind::Keyword),
            ["for", "const", "of"]
        );
        assert_eq!(
            kinds_of("for await (x of y) {}", Kind::Keyword),
            ["for", "await", "of"]
        );
    }

    #[test]
    fn decorators() {
        assert_eq!(
            kinds_of("@a.b(c) @d class E { @f.#g x; @h y; }", Kind::Attribute),
            ["@a.b", "@d", "@f.#g", "@h"]
        );
    }

    /// Every name that isn't a keyword is an identifier, one token each.
    #[test]
    fn marks_plain_identifiers() {
        assert_eq!(
            identifiers(
                "class A extends B { #x = 1; get y() { return this.#x + Math.max(z, w); } }"
            ),
            ["A", "B", "#x", "get", "y", "#x", "Math", "max", "z", "w"]
        );
    }
}
