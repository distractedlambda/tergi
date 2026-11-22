//! Checks tergi's YAML colors against the events of saphyr-parser.
//!
//! It does what the property test in src/highlight/yaml.rs does, but on
//! the files listed in <work>/files.txt, with tergi's tokens read from
//! <work>/tokens. Files that saphyr-parser doesn't read without an error
//! are skipped. In the others, plain keys must be plain, other plain
//! scalars must have the color of their type in YAML 1.2's core schema,
//! other scalars must have the string or escape color, aliases must have
//! the attribute color, and nothing outside of a scalar may be a string,
//! a number or a constant.
//!
//! Usage: yaml-check <work>

use std::ops::Range;
use std::path::Path;
use std::process::ExitCode;
use std::sync::LazyLock;

use regex::Regex;
use saphyr_parser::{Event, Parser, ScalarStyle};

/// How many disagreements to show.
const SHOW: usize = 15;

/// The kinds that tergi's dump writes. Only some of them matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Escape,
    Number,
    Constant,
    Attribute,
    Other,
}

fn kind(name: &str) -> Kind {
    match name {
        "String" => Kind::String,
        "Escape" => Kind::Escape,
        "Number" => Kind::Number,
        "Constant" => Kind::Constant,
        "Attribute" => Kind::Attribute,
        _ => Kind::Other,
    }
}

/// The color of a plain scalar that isn't a key, from the core schema in
/// section 10.3.2 of the YAML 1.2.2 spec. This is written separately from
/// tergi's own version so that a mistake there shows up.
fn resolve(s: &str) -> Kind {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"^(?:[-+]?[0-9]+|0o[0-7]+|0x[0-9a-fA-F]+",
            r"|[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?",
            r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$",
        ))
        .expect("a valid pattern")
    });
    match s {
        "null" | "Null" | "NULL" | "~" | "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => {
            Kind::Constant
        }
        _ if NUMBER.is_match(s) => Kind::Number,
        _ => Kind::String,
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Where byte `j` of `text` is, as a line number and that line's text.
fn place(text: &str, j: usize) -> String {
    let start = text[..j].rfind('\n').map_or(0, |n| n + 1);
    let end = text[j..].find('\n').map_or(text.len(), |n| j + n);
    let line = text[..j].matches('\n').count() + 1;
    let column = j - start + 1;
    format!("line {line}, byte {column}: {:?}", &text[start..end])
}

/// Check the colors of a scalar against what saphyr-parser read.
fn check_scalar(
    text: &str,
    kinds: &[Option<Kind>],
    covered: &mut [bool],
    mut r: Range<usize>,
    value: &str,
    style: ScalarStyle,
    key: bool,
) -> Result<(), String> {
    let b = text.as_bytes();
    // The span starts with the scalar's anchor and tag, if it has them.
    let mut i = r.start;
    while i < r.end && matches!(b[i], b'&' | b'!') {
        while i < r.end && !is_space(b[i]) {
            i += 1;
        }
        while i < r.end && (is_space(b[i]) || b[i] == b'#') {
            if b[i] == b'#' {
                while i < r.end && b[i] != b'\n' {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }
    // And a block scalar's span starts with its header when it has an
    // anchor or a tag.
    if matches!(style, ScalarStyle::Literal | ScalarStyle::Folded)
        && i < r.end
        && matches!(b[i], b'|' | b'>')
    {
        i = text[i..r.end].find('\n').map_or(r.end, |n| i + n + 1);
    }
    // An empty node is a plain scalar whose span holds whatever comes
    // after it, such as a comma.
    if style == ScalarStyle::Plain
        && !text[i..r.end].starts_with(value.chars().next().unwrap_or(' '))
    {
        return Ok(());
    }
    // A quoted scalar's span goes on past its closing quote, over spaces
    // and comments.
    if let Some(q) = match style {
        ScalarStyle::SingleQuoted => Some(b'\''),
        ScalarStyle::DoubleQuoted => Some(b'"'),
        _ => None,
    } {
        let mut j = i + 1;
        while j < r.end {
            if b[j] == b'\\' && q == b'"' || b[j] == q && b.get(j + 1) == Some(&q) && q == b'\'' {
                j += 2;
            } else if b[j] == q {
                break;
            } else {
                j += 1;
            }
        }
        r.end = r.end.min(j + 1);
    }
    covered[r.clone()].fill(true);
    let first_end = text[i..r.end].find('\n').map_or(r.end, |n| i + n);
    for j in i..r.end {
        if is_space(b[j]) {
            continue;
        }
        let ok = match style {
            ScalarStyle::Plain if key => kinds[j].is_none(),
            ScalarStyle::Plain if j < first_end => {
                kinds[j] == Some(resolve(text[i..first_end].trim_end()))
            }
            ScalarStyle::Plain | ScalarStyle::Literal | ScalarStyle::Folded => {
                kinds[j] == Some(Kind::String)
            }
            ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted => {
                matches!(kinds[j], Some(Kind::String | Kind::Escape))
            }
        };
        if !ok {
            let scalar: String = text[r.clone()].chars().take(80).collect();
            return Err(format!(
                "the scalar {scalar:?} ({style:?}, key: {key}) has {:?} at {}",
                kinds[j],
                place(text, j)
            ));
        }
    }
    Ok(())
}

/// Check the colors of a text against the events that saphyr-parser
/// reads from it. Returns None if it doesn't read it without an error.
fn check(text: &str, kinds: &[Option<Kind>]) -> Option<Result<(), String>> {
    let events = Parser::new_from_str(text)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(check_events(text, kinds, &events))
}

fn check_events(
    text: &str,
    kinds: &[Option<Kind>],
    events: &[(Event, saphyr_parser::Span)],
) -> Result<(), String> {
    // Spans count characters.
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut covered = vec![false; text.len()];
    // For each open collection, whether it's a mapping whose next node is
    // a key.
    let mut open: Vec<Option<bool>> = Vec::new();
    for (event, span) in events {
        let r = bytes[span.start.index()]..bytes[span.end.index()];
        let node = matches!(
            event,
            Event::Scalar(..)
                | Event::Alias(_)
                | Event::MappingStart(..)
                | Event::SequenceStart(..)
        );
        let mut key = false;
        if node && let Some(Some(next_is_key)) = open.last_mut() {
            key = *next_is_key;
            *next_is_key = !key;
        }
        match event {
            Event::MappingStart(..) => open.push(Some(true)),
            Event::SequenceStart(..) => open.push(None),
            Event::MappingEnd | Event::SequenceEnd => {
                open.pop();
            }
            Event::Alias(_) => {
                for j in r {
                    covered[j] = true;
                    if !(is_space(text.as_bytes()[j]) || kinds[j] == Some(Kind::Attribute)) {
                        return Err(format!("an alias has {:?} at {}", kinds[j], place(text, j)));
                    }
                }
            }
            Event::Scalar(value, style, ..) => {
                check_scalar(text, kinds, &mut covered, r, value, *style, key)?;
            }
            _ => {}
        }
    }
    // Outside of scalars, nothing is a string, a number or a constant.
    for (j, k) in kinds.iter().enumerate() {
        if !covered[j]
            && matches!(
                k,
                Some(Kind::String | Kind::Escape | Kind::Number | Kind::Constant)
            )
        {
            return Err(format!("{k:?} outside of scalars at {}", place(text, j)));
        }
    }
    Ok(())
}

/// tergi's kind for each byte of a text, from a .tok file.
fn read_kinds(tok: &str, len: usize) -> Vec<Option<Kind>> {
    let mut kinds = vec![None; len];
    for line in tok.lines() {
        let mut f = line.split(' ');
        let (Some(s), Some(e), Some(k)) = (f.next(), f.next(), f.next()) else {
            panic!("bad token line {line:?}");
        };
        let (s, e): (usize, usize) = (s.parse().expect("a start"), e.parse().expect("an end"));
        kinds[s..e].fill(Some(kind(k)));
    }
    kinds
}

fn main() -> ExitCode {
    let work = std::env::args().nth(1).expect("usage: yaml-check <work>");
    let work = Path::new(&work);
    let list = std::fs::read_to_string(work.join("files.txt")).expect("reading files.txt");
    let (mut files, mut read, mut bad) = (0, 0, 0);
    for (n, line) in list.lines().enumerate() {
        let path = line.rsplit('\t').next().unwrap_or(line);
        // tergi writes no tokens for a file that isn't UTF-8.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        files += 1;
        let tok = std::fs::read_to_string(work.join("tokens").join(format!("{n}.tok")))
            .unwrap_or_else(|e| panic!("reading the tokens of {path}: {e}"));
        let kinds = read_kinds(&tok, text.len());
        match check(&text, &kinds) {
            None => {}
            Some(Ok(())) => read += 1,
            Some(Err(e)) => {
                read += 1;
                bad += 1;
                if bad <= SHOW {
                    println!("{path}: {e}");
                }
            }
        }
    }
    println!("{files} files, {read} that saphyr-parser reads, {bad} differ");
    if bad > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
