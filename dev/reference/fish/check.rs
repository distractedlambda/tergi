//! Checks tergi's fish tokens against fish's own parser and highlighter.
//!
//! The Containerfile builds this as a binary inside fish's source, so that
//! it can use fish's crate. For each file listed in <work>/files.txt, it
//! works out which color each byte should get, and compares that with the
//! colors in tergi's tokens, which it reads from <work>/tokens.
//!
//! Most of the colors come from fish's highlighter, `highlight_shell`,
//! with I/O turned off so that it doesn't look for commands and files.
//! The highlighter gets the inside of words a bit wrong in places, for
//! example it leaves `\q` uncolored although fish reads it as an escape,
//! so where it differs from what fish does, we expect what fish does.
//! Fish's parse tree says which words are keywords, commands and
//! assignments, and which tokens are operators. Comparing stops at the
//! line of the first parse error, because tergi only needs to get back on
//! track after an error, which it may do in a different place.
//!
//! The check also compares where each identifier starts and ends, since
//! identifiers get their colors from their names.
//!
//! It can also write random texts for the check, since real files rarely
//! hold the odd corners of the syntax.
//!
//! Usage: tergi-check compare <work>
//!        tergi-check generate <directory> <cases> <seed>

use std::ops::Range;
use std::path::Path;
use std::process::ExitCode;

use fish::ast::{self, Kind as NodeKind, Node, Traversal};
use fish::highlight::{HighlightRole, HighlightSpec, highlight_shell};
use fish::operation_context::OperationContext;
use fish::parse_constants::{ParseErrorList, ParseKeyword, ParseTokenType, ParseTreeFlags};
use fish::parse_util::locate_cmdsubst_range;
use fish::prelude::*;
use fish::tokenizer::{TokFlags, Tokenizer};

/// How many disagreements to show, so that one bug doesn't bury the
/// others.
const SHOW: usize = 40;

/// The kinds that tergi's dump writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Comment,
    String,
    Escape,
    Number,
    Constant,
    Keyword,
    Type,
    Attribute,
    Lifetime,
    Operator,
    Punctuation,
    Identifier,
}

const KINDS: [Kind; 12] = [
    Kind::Comment,
    Kind::String,
    Kind::Escape,
    Kind::Number,
    Kind::Constant,
    Kind::Keyword,
    Kind::Type,
    Kind::Attribute,
    Kind::Lifetime,
    Kind::Operator,
    Kind::Punctuation,
    Kind::Identifier,
];

fn kind(name: &str) -> Kind {
    *KINDS
        .iter()
        .find(|k| format!("{k:?}") == name)
        .unwrap_or_else(|| panic!("unknown kind {name:?}"))
}

/// The colors a byte may get, as a set of bits: bit 0 for no color, and
/// bit 1 + n for the nth kind in KINDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Want(u16);

impl Want {
    const ANY: Want = Want(u16::MAX);
    const NONE: Want = Want(1);

    fn bit(k: Option<Kind>) -> u16 {
        match k {
            None => 1,
            Some(k) => 2 << KINDS.iter().position(|&x| x == k).unwrap_or(0),
        }
    }

    fn is(k: Kind) -> Want {
        Want(Want::bit(Some(k)))
    }

    /// Also allow `k`.
    fn or(self, k: Option<Kind>) -> Want {
        Want(self.0 | Want::bit(k))
    }

    fn allows(self, got: Option<Kind>) -> bool {
        self.0 & Want::bit(got) != 0
    }

    fn describe(self) -> std::string::String {
        if self == Want::ANY {
            return "anything".into();
        }
        let mut names = Vec::new();
        if self.allows(None) {
            names.push("nothing".to_string());
        }
        for k in KINDS {
            if self.allows(Some(k)) {
                names.push(format!("{k:?}"));
            }
        }
        names.join(" or ")
    }
}

/// `valid_var_name_char` in fish.
fn is_var_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// tergi's rule for which commands and names get a color from their name.
fn is_plain(w: &str) -> bool {
    !w.is_empty() && !w.bytes().any(|c| b"'\"\\$()*?[]{}~=\n".contains(&c))
}

/// What we expect of a file, by character. Fish works on characters,
/// and `bytes` turns their positions into byte offsets at the end.
struct Expected {
    want: Vec<Want>,
    /// Identifiers that tergi must have, as character ranges.
    identifiers: Vec<Range<usize>>,
    /// Where tergi may have identifiers that fish has no opinion on, such
    /// as the names that `set` and `function -a` give to variables.
    maybe_identifiers: Vec<bool>,
    /// The first character of the first parse error, if any.
    first_error: Option<usize>,
}

fn flags() -> ParseTreeFlags {
    // The flags that fish's highlighter parses with.
    ParseTreeFlags {
        continue_after_error: true,
        include_comments: true,
        accept_incomplete_tokens: true,
        leave_unterminated: true,
        show_extra_semis: true,
        ..Default::default()
    }
}

/// Map a role from fish's highlighter to the colors tergi may give.
fn from_roles(src: &wstr, roles: &[HighlightSpec], exp: &mut Expected) {
    let chars = src.as_char_slice();
    // For each slice we're in, whether it follows a `$` without a name,
    // which the highlighter colors although fish doesn't read it.
    let mut slices: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let role = roles[i].foreground;
        let next = chars.get(i + 1).copied();
        // The highlighter doesn't look inside slices, but fish reads
        // numbers, ranges, quotes and variables there.
        if !slices.is_empty() && !(c == ']' && role == HighlightRole::Operat) {
            let mut w = Want::NONE;
            for k in [
                Kind::String,
                Kind::Escape,
                Kind::Number,
                Kind::Operator,
                Kind::Punctuation,
                Kind::Identifier,
            ] {
                w = w.or(Some(k));
            }
            exp.want[i] = w;
            exp.maybe_identifiers[i] = true;
            if c == '[' && role == HighlightRole::Operat {
                slices.push(true);
            }
            i += 1;
            continue;
        }
        let w = match role {
            HighlightRole::Error => Want::ANY,
            // tergi colors a line continuation between words as an escape,
            // but this can also be part of a word that fish drops because
            // of an error.
            HighlightRole::Normal if c == '\\' && next == Some('\n') => {
                exp.want[i] = Want::NONE.or(Some(Kind::Escape));
                exp.want[i + 1] = Want::NONE.or(Some(Kind::Escape));
                i += 2;
                continue;
            }
            HighlightRole::Normal => Want::NONE,
            HighlightRole::Command
            | HighlightRole::Builtin
            | HighlightRole::Function
            | HighlightRole::Param
            | HighlightRole::Option => {
                // The highlighter leaves some escapes uncolored, but fish
                // reads every backslash outside of quotes as one.
                if c == '\\' {
                    match next {
                        Some(_) => {
                            exp.want[i] = Want::is(Kind::Escape);
                            exp.want[i + 1] = Want::is(Kind::Escape);
                            i += 2;
                            continue;
                        }
                        None => Want::NONE.or(Some(Kind::Escape)),
                    }
                } else {
                    Want::NONE
                }
            }
            HighlightRole::Keyword => Want::is(Kind::Keyword),
            HighlightRole::StatementTerminator => match c {
                ';' => Want::is(Kind::Punctuation),
                '\n' => Want::NONE,
                _ => Want::is(Kind::Operator),
            },
            HighlightRole::Comment => {
                if c.is_whitespace() {
                    Want::NONE.or(Some(Kind::Comment))
                } else {
                    Want::is(Kind::Comment)
                }
            }
            HighlightRole::Escape => {
                // The highlighter colors only the `\c` of `\cA`.
                if c == 'c' && i > 0 && chars[i - 1] == '\\' {
                    if let Some(_) = next {
                        exp.want[i] = Want::is(Kind::Escape);
                        exp.want[i + 1] = Want::is(Kind::Escape);
                        i += 2;
                        continue;
                    }
                }
                Want::is(Kind::Escape)
            }
            HighlightRole::Quote => Want::is(Kind::String),
            HighlightRole::Operat => match c {
                // A `$` only starts a variable or a command substitution
                // when a name or a `(` comes after it.
                '$' => {
                    let n = chars[i..].iter().take_while(|&&c| c == '$').count();
                    let after = &chars[i + n..];
                    // A `$(` is marked from the parse tree, since the
                    // highlighter also colors one that fish doesn't read
                    // as a command substitution.
                    let named = after.first().is_some_and(|&c| is_var_char(c))
                        || (after.starts_with(&['\\', '\n']) && after.get(2).is_some_and(|&c| is_var_char(c) || c == '['));
                    if named {
                        Want::is(Kind::Punctuation)
                    } else {
                        Want::NONE.or(Some(Kind::Punctuation)).or(Some(Kind::String))
                    }
                }
                // The highlighter colors every parenthesis in a word, but
                // only those of command substitutions mean something. The
                // parse tree marks those.
                '(' | ')' => Want::NONE.or(Some(Kind::Punctuation)),
                // tergi only reads a slice that ends on its line.
                '[' => {
                    // The `$` can come before line continuations.
                    let mut j = i;
                    while j >= 2 && chars[j - 2] == '\\' && chars[j - 1] == '\n' {
                        j -= 2;
                    }
                    let nameless = j > 0 && chars[j - 1] == '$' && roles[j - 1].foreground == HighlightRole::Error;
                    let loose = nameless || !slice_ends_on_line(chars, roles, i);
                    slices.push(loose);
                    if loose { Want::NONE.or(Some(Kind::Punctuation)).or(Some(Kind::String)) } else { Want::is(Kind::Punctuation) }
                }
                ']' => {
                    if slices.pop().unwrap_or(true) {
                        Want::NONE.or(Some(Kind::Punctuation)).or(Some(Kind::String))
                    } else {
                        Want::is(Kind::Punctuation)
                    }
                }
                // The highlighter colors every brace, but fish only expands
                // the ones with a `,` or a variable in them.
                '{' | '}' | ',' => Want::NONE.or(Some(Kind::Punctuation)),
                '*' | '?' | '~' | '%' => Want::NONE,
                '=' | '&' | '|' | '!' => Want::is(Kind::Operator),
                '\\' | '\n' => Want::is(Kind::Escape),
                c if is_var_char(c) => {
                    // A variable's name, unless it's the `self` of `%self`.
                    if i > 0 && chars[i - 1] == '%' {
                        let n = chars[i..].iter().take_while(|&&c| is_var_char(c)).count();
                        exp.want[i..i + n].fill(Want::NONE);
                        i += n;
                        continue;
                    }
                    let n = chars[i..]
                        .iter()
                        .zip(&roles[i..])
                        .take_while(|(c, r)| is_var_char(**c) && r.foreground == HighlightRole::Operat)
                        .count();
                    exp.want[i..i + n].fill(Want::is(Kind::Identifier));
                    exp.identifiers.push(i..i + n);
                    i += n;
                    continue;
                }
                _ => Want::ANY,
            },
            _ => Want::ANY,
        };
        exp.want[i] = w;
        i += 1;
    }
}

/// Whether the slice that opens at `open` closes on the same line.
fn slice_ends_on_line(chars: &[char], roles: &[HighlightSpec], open: usize) -> bool {
    let mut depth = 0;
    for (c, r) in chars[open..].iter().zip(&roles[open..]) {
        match (c, r.foreground) {
            ('\n', _) => return false,
            ('[', HighlightRole::Operat) => depth += 1,
            (']', HighlightRole::Operat) => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// The command substitutions in a word, as the ranges of their contents
/// and of the whole substitution, with its `$` if it has one, relative to
/// the word.
fn substitutions(word: &wstr) -> Vec<(Range<usize>, Range<usize>)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut quoted = false;
    let mut dollar = false;
    while let Ok(Some(cmdsub)) =
        locate_cmdsubst_range(word, &mut cursor, true, Some(&mut quoted), Some(&mut dollar))
    {
        let start = cmdsub.opening_paren_offset() - usize::from(dollar);
        out.push((cmdsub.command_range(), start..cmdsub.end()));
    }
    out
}

/// Whether the token after the word that ends at `end` goes on past the
/// end of its line, or starts on a later line, as after a line
/// continuation. When tergi decides what a word is from the token after
/// it, it only looks at that token's first line, so that the colors of a
/// line don't depend on the next.
fn next_token_crosses_line(src: &wstr, end: usize) -> bool {
    let rest = &src[end..];
    let line_end = rest.as_char_slice().iter().position(|&c| c == '\n').unwrap_or(rest.len());
    let flags = TokFlags {
        accept_unfinished: true,
        continue_after_error: true,
        ..Default::default()
    };
    match Tokenizer::new(rest, flags).next() {
        Some(tok) => tok.offset() > line_end || (tok.offset() < line_end && tok.end() > line_end),
        // The text ends after a line continuation.
        None => line_end < rest.len(),
    }
}

/// The words that fish reads differently depending on the token after
/// them.
const LOOKING_AHEAD: &[&str] = &[
    "and", "or", "time", "if", "while", "begin", "function", "switch", "for", "not", "!", "end",
    "command", "builtin", "exec",
];

/// Stop comparing at `start`.
fn stop(exp: &mut Expected, start: usize) {
    exp.first_error = Some(exp.first_error.map_or(start, |f| f.min(start)));
}

/// How deeply tergi reads command substitutions.
const MAX_DEPTH: usize = 64;

/// Add what fish's parse tree says about `src`, which starts at `base`
/// in the file and is inside `depth` command substitutions, and about the
/// command substitutions in it. Returns whether `src` has a comment with
/// a quote in it.
fn from_tree(src: &wstr, base: usize, depth: usize, exp: &mut Expected) -> bool {
    let mut errors = ParseErrorList::new();
    let tree = ast::parse(src, flags(), Some(&mut errors));
    if let Some(e) = errors.iter().map(|e| base + e.source_start).min() {
        exp.first_error = Some(exp.first_error.map_or(e, |f| f.min(e)));
    }
    let text = |r: Range<usize>| src[r].to_string();
    let fill = |exp: &mut Expected, r: Range<usize>, w: Want| {
        exp.want[base + r.start..base + r.end].fill(w);
    };
    // Recurse into the command substitutions of a word. Where fish's
    // highlighter doesn't look inside them, any color is right.
    let recurse = |exp: &mut Expected, r: Range<usize>, highlighted: bool| {
        for (inner, whole) in substitutions(&src[r.clone()]) {
            if highlighted && depth < MAX_DEPTH {
                let start = base + r.start + inner.start;
                exp.want[base + r.start + whole.start..start].fill(Want::is(Kind::Punctuation));
                if whole.end > inner.end {
                    exp.want[base + r.start + inner.end] = Want::is(Kind::Punctuation);
                }
                let contents = &src[r.start + inner.start..r.start + inner.end];
                // The highlighter reads the quotes in a comment in a
                // command substitution as quotes, and then gets the rest of
                // the word wrong. Fish doesn't, since it replaces the
                // command substitution first.
                if from_tree(contents, start, depth + 1, exp) {
                    exp.want[base + r.start + whole.end..base + r.end].fill(Want::ANY);
                }
            } else {
                // The same goes for a comment in a substitution that isn't
                // highlighted.
                let contents = src[r.start + inner.start..r.start + inner.end].to_string();
                let end = match contents.find('#') {
                    Some(h) if contents[h..].contains(['\'', '"']) => r.end,
                    _ => r.start + whole.end,
                };
                exp.want[base + r.start + whole.start..base + end].fill(Want::ANY);
            }
        }
    };
    let mut traversal = Traversal::new(tree.top());
    while let Some(node) = traversal.next() {
        // A word that fish reads by looking at the next token, where tergi
        // can't see all of it.
        let word = match node.kind() {
            NodeKind::DecoratedStatement(stmt) => stmt.command.try_source_range(),
            NodeKind::Keyword(_) | NodeKind::VariableAssignment(_) => node.try_source_range(),
            _ => None,
        };
        if let Some(w) = word.map(|w| w.as_usize())
            && (LOOKING_AHEAD.contains(&text(w.clone()).as_str())
                || matches!(node.kind(), NodeKind::VariableAssignment(_)))
            && next_token_crosses_line(src, w.end)
        {
            stop(exp, base + w.start);
        }
        // A node that the parser didn't finish has no range, but its
        // parts can still have theirs.
        let r = node.try_source_range().map_or(0..0, |r| r.as_usize());
        let leaf = matches!(
            node.kind(),
            NodeKind::Keyword(_) | NodeKind::Token(_) | NodeKind::Argument(_) | NodeKind::VariableAssignment(_)
        );
        if leaf && r.is_empty() {
            continue;
        }
        match node.kind() {
            NodeKind::Keyword(k) => {
                // Fish colors `and`, `or`, `not` and `time` like the names
                // of variables.
                let (s, e) = (base + r.start, base + r.end);
                exp.identifiers.retain(|i| i.end <= s || i.start >= e);
                // tergi doesn't read a keyword that goes on over a line
                // continuation, so that the colors of its first line
                // don't depend on the next. That changes how the rest
                // reads, so we stop comparing there.
                if text(r.clone()).contains('\n') {
                    stop(exp, s);
                }
                let w = if k.keyword() == ParseKeyword::Exclam {
                    Want::is(Kind::Operator)
                } else {
                    Want::is(Kind::Keyword)
                };
                fill(exp, r, w);
            }
            NodeKind::Token(t) => match t.token_type() {
                ParseTokenType::End => {
                    let w = if text(r.clone()) == ";" { Want::is(Kind::Punctuation) } else { Want::NONE };
                    fill(exp, r, w);
                }
                ParseTokenType::Pipe | ParseTokenType::Redirection => {
                    let s = text(r.clone());
                    let n = s.chars().take_while(|c| c.is_ascii_digit()).count();
                    fill(exp, r.start..r.start + n, Want::is(Kind::Number));
                    fill(exp, r.start + n..r.end, Want::is(Kind::Operator));
                }
                ParseTokenType::AndAnd | ParseTokenType::OrOr | ParseTokenType::Background => {
                    fill(exp, r, Want::is(Kind::Operator));
                }
                ParseTokenType::LeftBrace | ParseTokenType::RightBrace => {
                    fill(exp, r, Want::is(Kind::Punctuation));
                }
                _ => {}
            },
            NodeKind::Argument(_) => recurse(exp, r, true),
            NodeKind::DecoratedStatement(stmt) => {
                let Some(cmd) = stmt.command.try_source_range() else { continue };
                let cmd = cmd.as_usize();
                let name = text(cmd.clone());
                if is_plain(&name) {
                    fill(exp, cmd.clone(), Want::is(Kind::Identifier));
                    exp.identifiers.push(base + cmd.start..base + cmd.end);
                } else {
                    recurse(exp, cmd, false);
                }
                // tergi colors the name of the variable that `set` sets.
                if name.replace(['\\', '\'', '"'], "") == "set" {
                    for arg in stmt.args_or_redirs.iter().filter(|a| a.is_argument()) {
                        if let Some(a) = arg.argument().try_source_range() {
                            maybe_names(src, base, a.as_usize(), exp);
                        }
                    }
                }
            }
            NodeKind::VariableAssignment(_) => {
                let s = text(r.clone());
                if let Some(n) = s.find('=') {
                    let n = s[..n].chars().count();
                    fill(exp, r.start..r.start + n, Want::is(Kind::Identifier));
                    exp.identifiers.push(base + r.start..base + r.start + n);
                    fill(exp, r.start + n..r.start + n + 1, Want::is(Kind::Operator));
                }
                recurse(exp, r, true);
            }
            NodeKind::ForHeader(h) => {
                let Some(v) = h.var_name.try_source_range() else { continue };
                let v = v.as_usize();
                let s = text(v.clone());
                if !s.is_empty() && s.chars().all(is_var_char) {
                    fill(exp, v.clone(), Want::is(Kind::Identifier));
                    exp.identifiers.push(base + v.start..base + v.end);
                } else {
                    // The highlighter colors the whole variable alike, but
                    // tergi reads it as a word.
                    fill(exp, v, Want::ANY);
                }
            }
            NodeKind::FunctionHeader(h) => {
                if let Some(a) = h.first_arg.try_source_range() {
                    let a = a.as_usize();
                    let s = text(a.clone());
                    if is_plain(&s) && !s.starts_with('-') {
                        fill(exp, a.clone(), Want::is(Kind::Identifier));
                        exp.identifiers.push(base + a.start..base + a.end);
                    }
                }
                // tergi colors the names after options such as `-a`.
                for arg in h.args.iter() {
                    if let Some(a) = arg.try_source_range() {
                        maybe_names(src, base, a.as_usize(), exp);
                    }
                }
            }
            NodeKind::Redirection(redir) => {
                let (Some(oper), Some(target)) = (redir.oper.try_source_range(), redir.target.try_source_range())
                else {
                    continue;
                };
                let target = target.as_usize();
                let t = text(target.clone());
                if text(oper.as_usize()).ends_with('&') {
                    if t.chars().all(|c| c.is_ascii_digit()) {
                        fill(exp, target, Want::is(Kind::Number));
                    } else if t == "-" {
                        fill(exp, target, Want::is(Kind::Operator));
                    } else {
                        fill(exp, target, Want::ANY);
                    }
                } else if substitutions(&src[target.clone()]).is_empty() {
                    // The highlighter colors the whole target alike, but
                    // tergi reads it as a word.
                    fill(exp, target, Want::ANY);
                } else {
                    recurse(exp, target, true);
                }
            }
            _ => {}
        }
    }
    tree.extras.comments.iter().any(|c| {
        let c = &src[c.as_usize()];
        c.chars().any(|ch| ch == '\'' || ch == '"')
    })
}

/// Let tergi color the names of variables in an argument as identifiers,
/// along with a slice after them, as in `set a[1] x`.
fn maybe_names(src: &wstr, base: usize, r: Range<usize>, exp: &mut Expected) {
    let chars = &src.as_char_slice()[r.clone()];
    for (n, &c) in chars.iter().enumerate() {
        let i = base + r.start + n;
        if is_var_char(c) || c == '-' {
            exp.maybe_identifiers[i] = true;
            exp.want[i] = exp.want[i].or(Some(Kind::Identifier)).or(Some(Kind::Number));
        } else if matches!(c, '[' | ']') {
            exp.want[i] = exp.want[i].or(Some(Kind::Punctuation));
        } else if c == '.' {
            exp.want[i] = exp.want[i].or(Some(Kind::Operator));
        }
    }
}

fn expected(src: &wstr) -> Expected {
    let mut exp = Expected {
        want: vec![Want::NONE; src.len()],
        identifiers: Vec::new(),
        maybe_identifiers: vec![false; src.len()],
        first_error: None,
    };
    let mut roles = Vec::new();
    highlight_shell(src, &mut roles, &mut OperationContext::globals(), false, None);
    from_roles(src, &roles, &mut exp);
    from_tree(src, 0, 0, &mut exp);
    exp.identifiers.sort_by_key(|r| (r.start, r.end));
    exp.identifiers.dedup();
    exp
}

/// tergi's color for each byte, and its identifiers, from a .tok file.
fn read_tokens(tok: &str, len: usize) -> (Vec<Option<Kind>>, Vec<Range<usize>>) {
    let mut kinds = vec![None; len];
    let mut identifiers = Vec::new();
    for line in tok.lines() {
        let mut f = line.split(' ');
        let (Some(s), Some(e), Some(k)) = (f.next(), f.next(), f.next()) else {
            panic!("bad token line {line:?}");
        };
        let (s, e): (usize, usize) = (s.parse().expect("a start"), e.parse().expect("an end"));
        let k = kind(k);
        kinds[s..e].fill(Some(k));
        if k == Kind::Identifier {
            identifiers.push(s..e);
        }
    }
    (kinds, identifiers)
}

/// The line that byte `j` is on, as its number, the column of `j` in
/// bytes, and its text.
fn place(text: &str, j: usize) -> (usize, usize, &str) {
    let start = text[..j].rfind('\n').map_or(0, |n| n + 1);
    let end = text[j..].find('\n').map_or(text.len(), |n| j + n);
    (text[..j].matches('\n').count() + 1, j - start + 1, &text[start..end])
}

/// Compare one file. Returns the number of disagreements, and prints
/// them while fewer than SHOW have been printed in all.
fn compare_file(path: &str, text: &str, tok: &str, shown: &mut usize) -> usize {
    let src = WString::from_str(text);
    let exp = expected(&src);
    // The byte offset of each character, and of the end.
    let mut offsets: Vec<usize> = text.char_indices().map(|(n, _)| n).collect();
    offsets.push(text.len());
    // Compare up to the line of the first error.
    let limit = exp.first_error.map_or(text.len(), |e| {
        let e = offsets[e.min(src.len())];
        text[..e].rfind('\n').map_or(0, |n| n + 1)
    });
    let mut want = vec![Want::NONE; text.len()];
    let mut maybe = vec![false; text.len()];
    for (i, w) in exp.want.iter().enumerate() {
        want[offsets[i]..offsets[i + 1]].fill(*w);
        maybe[offsets[i]..offsets[i + 1]].fill(exp.maybe_identifiers[i]);
    }
    let (got, got_idents) = read_tokens(tok, text.len());
    let mut bad = 0;
    let mut j = 0;
    while j < limit {
        if want[j].allows(got[j]) {
            j += 1;
            continue;
        }
        bad += 1;
        let (line, column, line_text) = place(text, j);
        if *shown < SHOW {
            *shown += 1;
            let got = got[j].map_or("nothing".into(), |k| format!("{k:?}"));
            println!(
                "{path}:{line}:{column}: fish wants {}, tergi has {got}\n    {line_text:?}",
                want[j].describe()
            );
        }
        // One disagreement on each line is enough.
        j = text[j..].find('\n').map_or(text.len(), |n| j + n + 1);
    }
    // Identifiers where any color is right don't count.
    let before = |r: &Range<usize>| r.end <= limit && !want[r.clone()].iter().all(|&w| w == Want::ANY);
    let want_idents: Vec<Range<usize>> = exp
        .identifiers
        .iter()
        .map(|r| offsets[r.start]..offsets[r.end])
        .filter(before)
        .collect();
    let got_idents: Vec<Range<usize>> = got_idents
        .into_iter()
        .filter(before)
        .filter(|r| !(maybe[r.clone()].iter().all(|&m| m) && !want_idents.contains(r)))
        .collect();
    if want_idents != got_idents {
        bad += 1;
        if *shown < SHOW {
            *shown += 1;
            print!(
                "{path}: identifiers differ, fish has {} and tergi {}",
                want_idents.len(),
                got_idents.len()
            );
            let first = want_idents
                .iter()
                .zip(&got_idents)
                .find(|(w, g)| w != g)
                .map(|(w, g)| (Some(w), Some(g)))
                .or_else(|| {
                    let n = want_idents.len().min(got_idents.len());
                    Some((want_idents.get(n), got_idents.get(n)))
                });
            if let Some((w, g)) = first {
                let at = w.or(g).map_or(0, |r| r.start);
                let (line, _, line_text) = place(text, at);
                print!(
                    ". The first difference is on line {line}, where fish has {:?} and tergi {:?}\n    {line_text:?}",
                    w.map(|r| &text[r.clone()]),
                    g.map(|r| &text[r.clone()])
                );
            }
            println!();
        }
    }
    bad
}

/// Print what we expect of each character of a file, for debugging.
fn show(path: &Path) {
    let text = std::fs::read_to_string(path).expect("reading the file");
    let src = WString::from_str(&text);
    let mut roles = Vec::new();
    highlight_shell(&src, &mut roles, &mut OperationContext::globals(), false, None);
    let exp = expected(&src);
    for (i, c) in text.chars().enumerate() {
        println!("{i:5} {c:?} {:?} {}", roles[i].foreground, exp.want[i].describe());
    }
    println!("identifiers {:?}", exp.identifiers);
    let mut errors = ParseErrorList::new();
    let tree = ast::parse(&src, flags(), Some(&mut errors));
    println!("{}", tree.dump(&src));
    for e in errors {
        println!("error at {}: {}", e.source_start, e.text);
    }
    println!("first error {:?}", exp.first_error);
}

fn compare(work: &Path) -> ExitCode {
    let list = std::fs::read_to_string(work.join("files.txt")).expect("reading files.txt");
    let (mut files, mut bytes, mut skipped, mut bad, mut bad_files, mut shown) = (0, 0, 0, 0, 0, 0);
    let (mut errored, mut checked) = (0, 0);
    for (n, line) in list.lines().enumerate() {
        let path = line.rsplit('\t').next().unwrap_or(line);
        // tergi only lexes UTF-8, so it writes no tokens for other files.
        let Ok(text) = std::fs::read_to_string(path) else {
            skipped += 1;
            continue;
        };
        let tok = std::fs::read_to_string(work.join("tokens").join(format!("{n}.tok")))
            .unwrap_or_else(|e| panic!("reading the tokens of {path}: {e}"));
        files += 1;
        bytes += text.len();
        let src = WString::from_str(&text);
        let mut errors = ParseErrorList::new();
        ast::parse(&src, flags(), Some(&mut errors));
        if errors.is_empty() {
            checked += 1;
        } else {
            errored += 1;
        }
        let b = compare_file(path, &text, &tok, &mut shown);
        if b > 0 {
            bad += b;
            bad_files += 1;
        }
    }
    println!("{files} files ({bytes} bytes) compared, {skipped} skipped because they aren't UTF-8");
    println!("{checked} of them parse without errors, and {errored} were compared up to their first error");
    println!("{bad} disagreements in {bad_files} files");
    if bad > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// Pieces that are likely to start or end tokens, or to confuse a lexer.
const PIECES: &[&str] = &[
    " ", " ", " ", "\t", "\n", "\n", "\r\n", ";", "a", "b", "x", "echo ", "set ", "set -l ", "end",
    "if ", "else", "else if ", "for ", " in ", "while ", "function ", "begin", "switch ", "case ",
    "and ", "or ", "not ", "! ", "time ", "command ", "builtin ", "exec ", "return", "-h", "--help",
    "-x", "--", "a=", "b=1", "=", "$", "$a", "$$a", "$a[1]", "[", "]", "[1..-1]", "..", "(", ")",
    "$(", "{", "}", "{a,b}", ",", "'", "\"", "\\", "\\\n", "\\n", "\\x41", "\\cA", "\\u00e9", "\\q",
    "#", "# c", "|", "&", "&&", "||", "&|", "|&", ">", ">>", "2>", "2>&1", ">&-", "<", "<?", ">?",
    "&>", "0", "1", "2", "-", "*", "~", "%self", "é", "中", "\u{a0}", "\u{b}",
];

/// A small random number generator, so that the texts only depend on the
/// seed. This is SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn generate(dir: &Path, cases: usize, seed: u64) {
    std::fs::create_dir_all(dir).expect("creating the directory for the texts");
    let mut rng = Rng(seed);
    for i in 0..cases {
        let len = 1 + rng.below(25);
        let text: std::string::String = (0..len).map(|_| PIECES[rng.below(PIECES.len())]).collect();
        std::fs::write(dir.join(format!("{i}.fish")), text).expect("writing a text");
    }
}

fn main() -> ExitCode {
    fish::topic_monitor::init();
    fish::threads::init();
    fish::env::env_init(None, false);
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(|s| s.as_str()).collect::<Vec<_>>()[..] {
        ["compare", work] => compare(Path::new(work)),
        ["show", path] => {
            show(Path::new(path));
            ExitCode::SUCCESS
        }
        ["generate", dir, cases, seed] => {
            generate(
                Path::new(dir),
                cases.parse().expect("a number of cases"),
                seed.parse().expect("a seed"),
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: tergi-check compare <work>\n       tergi-check generate <directory> <cases> <seed>"
            );
            ExitCode::from(2)
        }
    }
}
