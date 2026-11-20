//! A lexer for CMake.
//!
//! It follows CMake's source. `cmListFileLexer.in.l` splits a file into
//! comments, brackets, quoted arguments and unquoted arguments, and the
//! parser in `cmListFileCache.cxx` finds the command names among them.
//! `ExpandVariablesInStringImpl` in `cmMakefile.cxx` reads the escapes and
//! variable references inside an argument, and `cmConditionEvaluator.cxx`
//! has the words that `if` treats as operators.
//!
//! Lines that come after a line never change its colors. A variable
//! reference or generator expression has to end on the line it starts on
//! to get a color.

use std::ops::Range;

use super::{Kind, Tokens};

/// The commands that CMake runs itself and doesn't let a script define
/// again. `cmCommands.cxx` adds them with `AddFlowControlCommand` and
/// `AddUnexpectedFlowControlCommand`.
const KEYWORDS: &[&str] = &[
    "if",
    "elseif",
    "else",
    "endif",
    "foreach",
    "endforeach",
    "while",
    "endwhile",
    "function",
    "endfunction",
    "macro",
    "endmacro",
    "block",
    "endblock",
    "break",
    "continue",
    "return",
];

/// The words that `if`, `elseif` and `while` treat as operators when they
/// aren't quoted. Unlike command names, they're case sensitive.
const OPERATORS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "COMMAND",
    "DEFINED",
    "DIAGNOSTIC",
    "EXISTS",
    "IS_ABSOLUTE",
    "IS_DIRECTORY",
    "IS_EXECUTABLE",
    "IS_NEWER_THAN",
    "IS_READABLE",
    "IS_SYMLINK",
    "IS_WRITABLE",
    "POLICY",
    "RULE",
    "TARGET",
    "TEST",
    "EQUAL",
    "LESS",
    "LESS_EQUAL",
    "GREATER",
    "GREATER_EQUAL",
    "STREQUAL",
    "STRLESS",
    "STRLESS_EQUAL",
    "STRGREATER",
    "STRGREATER_EQUAL",
    "VERSION_EQUAL",
    "VERSION_LESS",
    "VERSION_LESS_EQUAL",
    "VERSION_GREATER",
    "VERSION_GREATER_EQUAL",
    "PATH_EQUAL",
    "PATH_IS_PREFIX",
    "MATCHES",
    "IN_LIST",
];

/// The arguments of a command whose `(` we've read.
struct Command {
    /// How many parentheses inside the arguments are open.
    depth: usize,
    /// Whether the command is `if`, `elseif` or `while`.
    condition: bool,
}

pub fn lex(text: &str, out: &mut Tokens) {
    let b = text.as_bytes();
    // Whether only spaces came since the last line break outside of a
    // command. A command name can only come there.
    let mut line_start = true;
    let mut command: Option<Command> = None;
    // CMake skips a byte order mark at the start of the file.
    let mut i = if text.starts_with('\u{feff}') { 3 } else { 0 };
    while i < b.len() {
        match b[i] {
            b'\n' => {
                line_start |= command.is_none();
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => {
                let end = match bracket_open(b, i + 1) {
                    Some(n) => {
                        // A bracket comment counts as something other than
                        // a space, so a command can't follow it on the
                        // same line.
                        line_start = false;
                        bracket_close(b, i + 1 + n, n)
                    }
                    None => b[i..]
                        .iter()
                        .position(|&c| c == b'\n')
                        .map_or(b.len(), |n| i + n),
                };
                out.push_comment(text, i..end);
                i = end;
            }
            b'(' => {
                out.push(i..i + 1, Kind::Punctuation);
                if let Some(c) = &mut command {
                    c.depth += 1;
                }
                line_start = false;
                i += 1;
            }
            b')' => {
                out.push(i..i + 1, Kind::Punctuation);
                match &mut command {
                    Some(c) if c.depth > 0 => c.depth -= 1,
                    _ => command = None,
                }
                line_start = false;
                i += 1;
            }
            b'"' => {
                let end = quoted(text, i, out);
                line_start = false;
                i = end;
            }
            _ => {
                if let Some(n) = bracket_open(b, i) {
                    let end = bracket_close(b, i + n, n);
                    out.push(i..end, Kind::String);
                    line_start = false;
                    i = end;
                    continue;
                }
                let mut strings = Vec::new();
                let len = unquoted(b, i, &mut strings);
                let ident = identifier(b, i);
                // On a tie, the lexer prefers an identifier.
                if ident > 0 && ident == len {
                    let word = &text[i..i + ident];
                    let open = i + ident + spaces(&b[i + ident..]);
                    if line_start && command.is_none() && b.get(open) == Some(&b'(') {
                        let lower = word.to_ascii_lowercase();
                        let kind = if KEYWORDS.contains(&lower.as_str()) {
                            Kind::Keyword
                        } else {
                            Kind::Identifier
                        };
                        out.push(i..i + ident, kind);
                        out.push(open..open + 1, Kind::Punctuation);
                        command = Some(Command {
                            depth: 0,
                            condition: matches!(lower.as_str(), "if" | "elseif" | "while"),
                        });
                        line_start = false;
                        i = open + 1;
                        continue;
                    }
                    let kind = if command.as_ref().is_some_and(|c| c.condition)
                        && OPERATORS.contains(&word)
                    {
                        Kind::Operator
                    } else {
                        Kind::Identifier
                    };
                    out.push(i..i + ident, kind);
                    line_start = false;
                    i += ident;
                    continue;
                }
                // When no argument starts here, this is either a `[` that
                // starts nothing else, which is an argument on its own, or
                // a backslash at the end of a line, which is an error.
                let end = i + len.max(1);
                let mut kinds = vec![None; end - i];
                for r in strings {
                    kinds[r.start - i..r.end - i].fill(Some(Kind::String));
                }
                value(&b[..end], i, &mut kinds);
                push_kinds(out, i, &kinds);
                line_start = false;
                i = end;
            }
        }
    }
}

/// If a bracket such as `[==[` starts at `i`, the length of its opening.
fn bracket_open(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let n = 1 + spans(&b[i + 1..], |c| c == b'=');
    (b.get(i + n) == Some(&b'[')).then_some(n + 1)
}

/// Where a bracket whose opening is `n` bytes long ends, when its contents
/// start at `i`. A bracket that never closes goes on to the end.
fn bracket_close(b: &[u8], i: usize, n: usize) -> usize {
    // The closing has a `]` and the same number of `=` as the opening.
    let mut close = vec![b'='; n];
    close[0] = b']';
    close[n - 1] = b']';
    b[i..]
        .windows(n)
        .position(|w| w == close)
        .map_or(b.len(), |p| i + p + n)
}

/// The number of bytes at the start of `b` that `f` accepts.
fn spans(b: &[u8], f: impl Fn(u8) -> bool) -> usize {
    b.iter().position(|&c| !f(c)).unwrap_or(b.len())
}

fn spaces(b: &[u8]) -> usize {
    spans(b, |c| matches!(c, b' ' | b'\t' | b'\r'))
}

/// The length of the identifier at `i`, or 0.
fn identifier(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
            1 + spans(&b[i + 1..], |c| c.is_ascii_alphanumeric() || c == b'_')
        }
        _ => 0,
    }
}

/// The length of the quoted argument that starts at `i`, and color it.
/// One that never closes goes on to the end.
fn quoted(text: &str, i: usize, out: &mut Tokens) -> usize {
    let b = text.as_bytes();
    let mut j = i + 1;
    while j < b.len() && b[j] != b'"' {
        // A backslash escapes anything but the end of the text. Before a
        // line break, it joins the lines.
        j += if b[j] == b'\\' && j + 1 < b.len() {
            1 + char_len(text, j + 1)
        } else {
            1
        };
    }
    let end = (j + 1).min(b.len());
    let mut kinds = vec![Some(Kind::String); end - i];
    value(&b[..j], i + 1, &mut kinds[1..]);
    push_kinds(out, i, &kinds);
    end
}

/// The length of the unquoted argument at `i`, or 0 if none starts there.
/// Add the parts of it in quotes to `strings`.
///
/// This follows the two rules for unquoted arguments in CMake's lexer.
/// One of them allows `$(NAME)` and text in quotes inside the argument,
/// for compatibility with old versions. That one always matches at least
/// as much as the other, so this only follows it.
fn unquoted(b: &[u8], i: usize, strings: &mut Vec<Range<usize>>) -> usize {
    let mut j = i;
    // The first part can't be in quotes, and a `[` there needs something
    // else after its `=`.
    match b.get(j) {
        Some(b'=') => j += 1,
        Some(b'[') => {
            let k = j + 1 + spans(&b[j + 1..], |c| c == b'=');
            let n = part(b, k, strings);
            if n == 0 {
                return 0;
            }
            j = k + n;
        }
        Some(b'"') | None => return 0,
        Some(_) => {
            let n = part(b, j, strings);
            if n == 0 {
                return 0;
            }
            j += n;
        }
    }
    loop {
        let n = match b.get(j) {
            Some(b'[' | b'=') => 1,
            _ => part(b, j, strings),
        };
        if n == 0 {
            return j - i;
        }
        j += n;
    }
}

/// The length of one part of an unquoted argument at `i`, or 0. A part
/// is `$(NAME)`, text in quotes, an escape, or any other character but a
/// space, a parenthesis, `#`, `[` or `=`.
fn part(b: &[u8], i: usize, strings: &mut Vec<Range<usize>>) -> usize {
    let Some(&c) = b.get(i) else {
        return 0;
    };
    match c {
        b'$' => {
            if b.get(i + 1) == Some(&b'(') {
                let n = spans(&b[i + 2..], |c| c.is_ascii_alphanumeric() || c == b'_');
                if b.get(i + 2 + n) == Some(&b')') {
                    return n + 3;
                }
            }
            1
        }
        b'"' => {
            // Inside quotes, spaces, tabs, `[` and `=` are allowed too.
            let mut j = i + 1;
            loop {
                let n = match b.get(j) {
                    Some(b'"') => {
                        strings.push(i..j + 1);
                        return j + 1 - i;
                    }
                    Some(b' ' | b'\t' | b'[' | b'=') => 1,
                    _ => part(b, j, &mut Vec::new()),
                };
                if n == 0 {
                    return 0;
                }
                j += n;
            }
        }
        // The lexer reads the backslash and one byte. The rest of the
        // character, if any, follows as other parts.
        // CMake reads a CRLF as a line feed, so a backslash before one
        // doesn't escape anything.
        b'\\' => match &b[i + 1..] {
            [] | [b'\n', ..] | [b'\r', b'\n', ..] => 0,
            _ => 2,
        },
        b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'#' | b'[' | b'=' => 0,
        _ => 1,
    }
}

/// Color the escapes, variable references and generator expressions in
/// the argument text that starts at `start` and ends at the end of `b`.
/// `kinds` holds a kind for each of its bytes, which starts as the color
/// of the argument as a whole.
fn value(b: &[u8], start: usize, kinds: &mut [Option<Kind>]) {
    let mut fill = |r: Range<usize>, kind| kinds[r.start - start..r.end - start].fill(Some(kind));
    // The `$<` of the generator expressions that are open.
    let mut open = Vec::new();
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let Some(&c) = b.get(i + 1) else {
                    break;
                };
                let end = i + 1 + utf8_len(c).min(b.len() - i - 1);
                // A backslash before a line break only shows up in quoted
                // arguments, where it joins the lines.
                if c == b'\n' {
                    open.clear();
                }
                if escape(c) {
                    fill(i..end, Kind::Escape);
                }
                i = end;
            }
            b'$' => {
                if let Some(end) = reference(b, i) {
                    fill(i..end, Kind::Attribute);
                    i = end;
                } else if b.get(i + 1) == Some(&b'<') {
                    open.push(i);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'>' => {
                if let Some(o) = open.pop() {
                    fill(o..o + 2, Kind::Punctuation);
                    fill(i..i + 1, Kind::Punctuation);
                }
                i += 1;
            }
            b'\n' => {
                open.clear();
                i += 1;
            }
            _ => i += 1,
        }
    }
}

/// Whether a backslash followed by `c` is an escape that CMake accepts.
/// Other letters and digits are an error. CMake only looks at the first
/// byte of the character after the backslash.
fn escape(c: u8) -> bool {
    matches!(c, b't' | b'n' | b'r') || !c.is_ascii_alphanumeric()
}

/// If `rest`, which follows a `$`, starts with the `{`, `ENV{` or `CACHE{`
/// of a variable reference, its length.
fn reference_open(rest: &[u8]) -> Option<usize> {
    ["{", "ENV{", "CACHE{"]
        .iter()
        .find(|p| rest.starts_with(p.as_bytes()))
        .map(|p| p.len())
}

/// If a variable reference such as `${NAME}` starts at `i` and ends on
/// the same line, where it ends.
///
/// A name can hold letters, digits, `_/.+-`, escapes, other references,
/// and a `$` that doesn't start anything. CMake also allows a line break
/// inside a quoted argument, but we don't follow it onto the next line.
fn reference(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1 + reference_open(&b[i + 1..])?;
    let name_char = |c: u8| c.is_ascii_alphanumeric() || b"_/.+-".contains(&c);
    loop {
        match *b.get(j)? {
            b'}' => return Some(j + 1),
            b'$' if reference_open(&b[j + 1..]).is_some() => j = reference(b, j)?,
            b'$' => {
                // CMake doesn't support other kinds of references, such
                // as `$FOO{x}`, and gives up when it sees one.
                let n = spans(&b[j + 1..], name_char);
                if n > 0 && b.get(j + 1 + n) == Some(&b'{') {
                    return None;
                }
                j += 1;
            }
            b'\\' => {
                // CMake escapes only the first byte of the character
                // after the backslash, and the rest of a character that
                // isn't ASCII doesn't belong in a name.
                let c = *b.get(j + 1)?;
                if c == b'\n' || !c.is_ascii() || !escape(c) {
                    return None;
                }
                j += 2;
            }
            c if name_char(c) => j += 1,
            _ => return None,
        }
    }
}

/// The length of the UTF-8 character that starts with the byte `c`.
fn utf8_len(c: u8) -> usize {
    match c {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

/// The length of the character at `i`.
fn char_len(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(1, char::len_utf8)
}

/// Add a token for each run of bytes with the same kind. `kinds` starts
/// at `start`.
fn push_kinds(out: &mut Tokens, start: usize, kinds: &[Option<Kind>]) {
    let mut i = 0;
    while i < kinds.len() {
        let n = spans_by(&kinds[i..], kinds[i]);
        if let Some(k) = kinds[i] {
            out.push(start + i..start + i + n, k);
        }
        i += n;
    }
}

fn spans_by(kinds: &[Option<Kind>], k: Option<Kind>) -> usize {
    kinds.iter().position(|&x| x != k).unwrap_or(kinds.len())
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

    #[test]
    fn lexes_a_file() {
        use Kind::*;
        let text = r#"# Build the library.
cmake_minimum_required(VERSION 3.20)
if(NOT DEFINED FOO AND (BAR OR "AND"))
  target_include_directories(foo PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include")
endif()
"#;
        assert_eq!(
            tokens(text),
            [
                ("# Build the library.", Comment),
                ("(", Punctuation),
                (")", Punctuation),
                ("if", Keyword),
                ("(", Punctuation),
                ("NOT", Operator),
                ("DEFINED", Operator),
                ("AND", Operator),
                ("(", Punctuation),
                ("OR", Operator),
                ("\"AND\"", String),
                ("))", Punctuation),
                ("(", Punctuation),
                ("\"", String),
                ("${CMAKE_CURRENT_SOURCE_DIR}", Attribute),
                ("/include\"", String),
                (")", Punctuation),
                ("endif", Keyword),
                ("()", Punctuation),
            ]
        );
    }

    #[test]
    fn command_names() {
        use Kind::*;
        // Command names are case insensitive, and only the flow control
        // ones are keywords. A name needs a `(` after it on its line.
        assert_eq!(
            tokens("  IF (x)\nElse()\nendif\nfoo()"),
            [
                ("IF", Keyword),
                ("(", Punctuation),
                (")", Punctuation),
                ("Else", Keyword),
                ("()", Punctuation),
                ("()", Punctuation),
            ]
        );
        // A command can't follow another on the same line, or a bracket
        // comment. An argument that looks like a command is plain.
        assert_eq!(
            tokens("foo() if()\n#[[c]] if()\nfoo(\nif()\n)"),
            [
                ("()", Punctuation),
                ("()", Punctuation),
                ("#[[c]]", Comment),
                ("()", Punctuation),
                ("(", Punctuation),
                ("()", Punctuation),
                (")", Punctuation),
            ]
        );
        // A line comment doesn't stop a command after it.
        assert_eq!(
            tokens("# a\nif()"),
            [("# a", Comment), ("if", Keyword), ("()", Punctuation)]
        );
    }

    #[test]
    fn condition_operators() {
        use Kind::*;
        // Only whole unquoted words in uppercase are operators, and only
        // in `if`, `elseif` and `while`.
        assert_eq!(
            tokens("while(a STREQUAL b and NOT_ [[OR]] \"OR\" ${AND})"),
            [
                ("while", Keyword),
                ("(", Punctuation),
                ("STREQUAL", Operator),
                ("[[OR]]", String),
                ("\"OR\"", String),
                ("${AND}", Attribute),
                (")", Punctuation),
            ]
        );
        assert_eq!(
            tokens("set(x AND)"),
            [("(", Punctuation), (")", Punctuation)]
        );
    }

    #[test]
    fn comments_and_brackets() {
        use Kind::*;
        // A bracket closes with as many `=` as it opened with.
        assert_eq!(
            tokens("x([=[a]]b]==]c]=]) # d\n#[==[e\n]=]f]==]"),
            [
                ("(", Punctuation),
                ("[=[a]]b]==]c]=]", String),
                (")", Punctuation),
                ("# d", Comment),
                ("#[==[e", Comment),
                ("]=]f]==]", Comment),
            ]
        );
        // `#` starts a comment even in the middle of an argument, but not
        // in a quoted one. A `#` and a space before the bracket make a
        // line comment.
        assert_eq!(
            tokens("x(a#b)\n\"#\")\n# [[c\n"),
            [
                ("(", Punctuation),
                ("#b)", Comment),
                ("\"#\"", String),
                (")", Punctuation),
                ("# [[c", Comment),
            ]
        );
        // A `[` in the middle of an argument doesn't start a bracket.
        assert_eq!(
            tokens("x(a[[b]] [x)"),
            [("(", Punctuation), (")", Punctuation)]
        );
        // Brackets and quotes that don't close go on to the end.
        assert_eq!(tokens("x([[a\n)"), [("(", Punctuation), ("[[a\n)", String)]);
        assert_eq!(tokens("x(\"a\n)"), [("(", Punctuation), ("\"a\n)", String)]);
    }

    #[test]
    fn escapes() {
        use Kind::*;
        assert_eq!(
            tokens("x(\"\\t\\\"\\;\\é\\q\" a\\nb\\ c)"),
            [
                ("(", Punctuation),
                ("\"", String),
                ("\\t\\\"\\;\\é", Escape),
                ("\\q\"", String),
                ("\\n", Escape),
                ("\\ ", Escape),
                (")", Punctuation),
            ]
        );
        // A backslash before a line break joins the lines of a quoted
        // argument.
        assert_eq!(
            tokens("x(\"a\\\nb\")"),
            [
                ("(", Punctuation),
                ("\"a", String),
                ("\\\n", Escape),
                ("b\"", String),
                (")", Punctuation),
            ]
        );
    }

    #[test]
    fn variable_references() {
        use Kind::*;
        assert_eq!(
            tokens("x(${a_${b}} $ENV{PATH}/$CACHE{c.d+-} \\${e} $f{g} [[${h}]])"),
            [
                ("(", Punctuation),
                ("${a_${b}}", Attribute),
                ("$ENV{PATH}", Attribute),
                ("$CACHE{c.d+-}", Attribute),
                ("\\$", Escape),
                ("[[${h}]]", String),
                (")", Punctuation),
            ]
        );
        // Escapes are allowed in names, but most other characters are an
        // error, and so is a reference that doesn't close.
        assert_eq!(
            tokens("x(\"${a\\;b} ${a b} ${a\\qb} ${a${b} ${a}\")"),
            [
                ("(", Punctuation),
                ("\"", String),
                ("${a\\;b}", Attribute),
                (" ${a b} ${a\\qb} ${a", String),
                ("${b}", Attribute),
                (" ", String),
                ("${a}", Attribute),
                ("\"", String),
                (")", Punctuation),
            ]
        );
        // A `$` that starts nothing is allowed in a name, but one that
        // looks like another kind of reference isn't. Only the first byte
        // after a backslash is escaped, and the rest of `é` can't be in a
        // name. Outside a name, the escape is fine.
        assert_eq!(
            tokens("x($ENV{a$b} ${a$b{c}} ${a\\é})"),
            [
                ("(", Punctuation),
                ("$ENV{a$b}", Attribute),
                ("\\é", Escape),
                (")", Punctuation)
            ]
        );
        // A reference has to end on its line.
        assert_eq!(
            tokens("x(\"${a\n}\")"),
            [
                ("(", Punctuation),
                ("\"${a\n}\"", String),
                (")", Punctuation)
            ]
        );
    }

    #[test]
    fn generator_expressions() {
        use Kind::*;
        assert_eq!(
            tokens("x($<$<CONFIG:Debug>:-g> \"$<a:b>\" a>)"),
            [
                ("($<$<", Punctuation),
                (">", Punctuation),
                (">", Punctuation),
                ("\"", String),
                ("$<", Punctuation),
                ("a:b", String),
                (">", Punctuation),
                ("\"", String),
                (")", Punctuation),
            ]
        );
        // One that doesn't close in its argument is plain, but the ones
        // inside it can close.
        assert_eq!(
            tokens("x($<a:$<b> c>)"),
            [
                ("(", Punctuation),
                ("$<", Punctuation),
                (">", Punctuation),
                (")", Punctuation)
            ]
        );
    }

    #[test]
    fn legacy_arguments() {
        use Kind::*;
        // Quotes in the middle of an unquoted argument hold spaces, and
        // `$(NAME)` holds parentheses, as in old versions of CMake.
        assert_eq!(
            tokens("x(-Da=\"b c\"${d} $(e) if\"(\")"),
            [
                ("(", Punctuation),
                ("\"b c\"", String),
                ("${d}", Attribute),
                ("\"(\"", String),
                (")", Punctuation),
            ]
        );
        // At the start of an argument, a quote starts a quoted argument.
        // After `[=`, it doesn't.
        assert_eq!(
            tokens("x(\"a\"b [=\"c d\")"),
            [
                ("(", Punctuation),
                ("\"a\"", String),
                ("\"c d\"", String),
                (")", Punctuation),
            ]
        );
    }

    /// Command names that aren't keywords, and arguments that are only a
    /// name, are identifiers. Variable references keep their own color.
    #[test]
    fn marks_plain_identifiers() {
        let text = "set(FOO bar \"baz\")\nif(NOT x STREQUAL ${FOO})\nendif()\nadd(a-b c)";
        let mut out = Tokens::default();
        lex(text, &mut out);
        assert_eq!(
            out.identifiers(text),
            ["set", "FOO", "bar", "x", "add", "c"]
        );
    }
}
