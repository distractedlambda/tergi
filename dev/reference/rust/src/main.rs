//! Checks tergi's Rust tokens against rustc's own lexer.
//!
//! rust-analyzer publishes rustc's lexer as the crate ra-ap-rustc_lexer,
//! and the code that reads escapes as rustc-literal-escaper. This program
//! runs them over the files listed in <work>/files.txt and works out which
//! color each byte should get. It compares that with the colors in tergi's
//! tokens, which it reads from <work>/tokens. It also checks that each
//! identifier starts and ends in the same place, since identifiers get
//! their colors from their names.
//!
//! It can also write random texts for the check, since real files rarely
//! hold the odd corners of the syntax.
//!
//! Usage: rust-check compare <work>
//!        rust-check generate <directory> <cases> <seed>

use std::ops::Range;
use std::path::Path;
use std::process::ExitCode;

use ra_ap_rustc_lexer::{
    Cursor, FrontmatterAllowed, LiteralKind, RawStrError, TokenKind, is_ident, strip_shebang,
    validate_raw_str,
};

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

fn kind(name: &str) -> Kind {
    use Kind::*;
    match name {
        "Comment" => Comment,
        "String" => String,
        "Escape" => Escape,
        "Number" => Number,
        "Constant" => Constant,
        "Keyword" => Keyword,
        "Type" => Type,
        "Attribute" => Attribute,
        "Lifetime" => Lifetime,
        "Operator" => Operator,
        "Punctuation" => Punctuation,
        "Identifier" => Identifier,
        _ => panic!("unknown kind {name:?}"),
    }
}

/// The color a byte should get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Is(Option<Kind>),
    /// Either of two colors is right. A string's escape that rustc
    /// rejects may be colored as an escape or as string.
    Either(Kind, Kind),
    /// Any color is right. This is for tokens that are errors in rustc,
    /// where tergi only needs to get back on track afterwards.
    Any,
}

impl Want {
    fn allows(self, got: Option<Kind>) -> bool {
        match self {
            Want::Is(k) => got == k,
            Want::Either(a, b) => got == Some(a) || got == Some(b),
            Want::Any => true,
        }
    }
}

/// The strict keywords that tergi colors. It leaves out the reserved ones
/// that nobody uses yet, such as `abstract` and `yield`.
const KEYWORDS: &[&str] = &[
    "Self", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while",
];

fn word(s: &str) -> Kind {
    match s {
        "true" | "false" => Kind::Constant,
        _ if KEYWORDS.contains(&s) => Kind::Keyword,
        _ => Kind::Identifier,
    }
}

/// rustc's tokens for a whole file, as their kinds and byte ranges.
///
/// rustc's parser reads some tokens differently in older editions, and
/// tergi doesn't know the edition. So this reads them as Rust 2021 does.
/// The guarded string prefixes `#"` and `##` are only reserved in Rust
/// 2024, so before that the `#` is a token of its own and lexing starts
/// again after it. The same goes for `k#` in `k#fn`, which needs an
/// unstable feature.
fn tokens(text: &str) -> Vec<(TokenKind, Range<usize>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    if let Some(n) = strip_shebang(text) {
        out.push((TokenKind::LineComment { doc_style: None }, 0..n));
        pos = n;
    }
    let mut frontmatter = FrontmatterAllowed::Yes;
    'restart: loop {
        let mut cursor = Cursor::new(&text[pos..], frontmatter);
        frontmatter = FrontmatterAllowed::No;
        loop {
            let token = cursor.advance_token();
            let len = token.len as usize;
            match token.kind {
                TokenKind::Eof => return out,
                TokenKind::GuardedStrPrefix => {
                    out.push((TokenKind::Pound, pos..pos + 1));
                    pos += 1;
                    continue 'restart;
                }
                TokenKind::ForcedKeywordIdent => {
                    out.push((TokenKind::Ident, pos..pos + 1));
                    pos += 1;
                    continue 'restart;
                }
                kind => out.push((kind, pos..pos + len)),
            }
            pos += len;
        }
    }
}

fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
    )
}

/// Whether the token is a name that can be part of an attribute's path.
fn is_path_name(kind: TokenKind, text: &str) -> bool {
    match kind {
        TokenKind::Ident | TokenKind::UnknownPrefix => word(text) == Kind::Identifier,
        TokenKind::RawIdent => true,
        _ => false,
    }
}

/// What comes next in the path at the start of an attribute, such as
/// `#[rustfmt::skip`. tergi colors it as part of the attribute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrPath {
    Name,
    Sep,
}

struct Expected {
    want: Vec<Want>,
    identifiers: Vec<Range<usize>>,
}

fn expected(text: &str) -> Expected {
    let toks = tokens(text);
    let mut want = vec![Want::Is(None); text.len()];
    let mut identifiers = Vec::new();
    let mut attr = None;
    let mut i = 0;
    while i < toks.len() {
        let (kind, r) = toks[i].clone();
        let slice = &text[r.clone()];
        // The kinds of the tokens right after this one, with nothing
        // between them. rustc's parser glues those into tokens such as
        // `..=` and `::`.
        let next = |n: usize| toks.get(i + n).map(|t| t.0);
        if is_trivia(kind) {
            if kind != TokenKind::Whitespace {
                want[r].fill(Want::Is(Some(Kind::Comment)));
            }
            i += 1;
            continue;
        }
        let path = attr.take();
        // How many tokens this one takes up, and their color.
        let (n, color) = match kind {
            TokenKind::Pound
                if next(1) == Some(TokenKind::OpenBracket)
                    || next(1) == Some(TokenKind::Bang)
                        && next(2) == Some(TokenKind::OpenBracket) =>
            {
                attr = Some(AttrPath::Name);
                let n = if next(1) == Some(TokenKind::Bang) {
                    3
                } else {
                    2
                };
                (n, Want::Is(Some(Kind::Attribute)))
            }
            _ if path == Some(AttrPath::Name) && is_path_name(kind, slice) => {
                attr = Some(AttrPath::Sep);
                (1, Want::Is(Some(Kind::Attribute)))
            }
            TokenKind::Colon
                if path == Some(AttrPath::Sep) && next(1) == Some(TokenKind::Colon) =>
            {
                attr = Some(AttrPath::Name);
                (2, Want::Is(Some(Kind::Attribute)))
            }
            TokenKind::Ident | TokenKind::UnknownPrefix => {
                let k = word(slice);
                if k == Kind::Identifier {
                    identifiers.push(r.clone());
                }
                (1, Want::Is(Some(k)))
            }
            TokenKind::RawIdent => {
                identifiers.push(r.clone());
                (1, Want::Is(Some(Kind::Identifier)))
            }
            TokenKind::Literal { kind, suffix_start } => {
                literal(text, r.clone(), kind, suffix_start as usize, &mut want);
                (1, Want::Is(Some(Kind::Number)))
            }
            // rustc reads lifetimes such as `'1a` and `'a🦀` to report
            // them.
            TokenKind::Lifetime { .. } | TokenKind::UnknownPrefixLifetime
                if !is_ident(&slice[1..]) =>
            {
                (1, Want::Any)
            }
            TokenKind::Lifetime { .. }
            | TokenKind::RawLifetime
            | TokenKind::UnknownPrefixLifetime => (1, Want::Is(Some(Kind::Lifetime))),
            TokenKind::Dot => match (next(1), next(2)) {
                (Some(TokenKind::Dot), Some(TokenKind::Dot | TokenKind::Eq)) => {
                    (3, Want::Is(Some(Kind::Operator)))
                }
                (Some(TokenKind::Dot), _) => (2, Want::Is(Some(Kind::Operator))),
                _ => (1, Want::Is(Some(Kind::Punctuation))),
            },
            TokenKind::Colon if next(1) == Some(TokenKind::Colon) => {
                (2, Want::Is(Some(Kind::Punctuation)))
            }
            TokenKind::Semi
            | TokenKind::Comma
            | TokenKind::OpenParen
            | TokenKind::CloseParen
            | TokenKind::OpenBrace
            | TokenKind::CloseBrace
            | TokenKind::OpenBracket
            | TokenKind::CloseBracket
            | TokenKind::Pound
            | TokenKind::Colon
            | TokenKind::Dollar => (1, Want::Is(Some(Kind::Punctuation))),
            TokenKind::At
            | TokenKind::Tilde
            | TokenKind::Question
            | TokenKind::Eq
            | TokenKind::Bang
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::Minus
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Plus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Caret
            | TokenKind::Percent => (1, Want::Is(Some(Kind::Operator))),
            // tergi doesn't know the frontmatter of a cargo script, which
            // is still unstable, and lexes it as code.
            TokenKind::InvalidIdent | TokenKind::Frontmatter { .. } => (1, Want::Any),
            TokenKind::Unknown => (1, Want::Is(None)),
            TokenKind::Whitespace
            | TokenKind::LineComment { .. }
            | TokenKind::BlockComment { .. }
            | TokenKind::GuardedStrPrefix
            | TokenKind::ForcedKeywordIdent
            | TokenKind::Eof => unreachable!("{kind:?} is handled above"),
        };
        let end = toks[i + n - 1].1.end;
        // Literals fill in their own colors.
        if !matches!(kind, TokenKind::Literal { .. }) || n > 1 {
            want[r.start..end].fill(color);
        }
        i += n;
    }
    Expected { want, identifiers }
}

/// Fill in the colors of a literal. Numbers are numbers. Strings and
/// characters are strings, except for their escapes.
fn literal(text: &str, r: Range<usize>, kind: LiteralKind, suffix: usize, want: &mut [Want]) {
    use LiteralKind::*;
    let suffix = r.start + suffix;
    let (prefix, terminated, mode) = match kind {
        Int { .. } | Float { .. } => {
            want[r].fill(Want::Is(Some(Kind::Number)));
            return;
        }
        RawStr { .. } | RawByteStr { .. } | RawCStr { .. } => {
            // rustc reads `r##` or `r#~` as a raw string that doesn't
            // start, to report it. tergi skips it as an error.
            let prefix = if text[r.clone()].starts_with('r') {
                1
            } else {
                2
            };
            let w = match validate_raw_str(&text[r.clone()], prefix) {
                Err(RawStrError::InvalidStarter { .. }) => Want::Any,
                _ => Want::Is(Some(Kind::String)),
            };
            want[r].fill(w);
            return;
        }
        Char { terminated } => (0, terminated, Mode::Str),
        Byte { terminated } => (1, terminated, Mode::Bytes),
        Str { terminated } => (0, terminated, Mode::Str),
        ByteStr { terminated } => (1, terminated, Mode::Bytes),
        CStr { terminated } => (1, terminated, Mode::C),
    };
    let body = r.start + prefix + 1..if terminated { suffix - 1 } else { suffix };
    want[r.clone()].fill(Want::Is(Some(Kind::String)));
    // A character has the same escapes as a string. rustc reads the body
    // of one that it rejects, such as `'ab'`, the same way.
    escapes(text, body, mode, want);
}

#[derive(Clone, Copy)]
enum Mode {
    Str,
    Bytes,
    C,
}

/// Mark the escapes in the body of a string or character.
///
/// rustc turns each CRLF into LF before it lexes a file, so we do that
/// with the body before reading its escapes, and map the ranges back.
fn escapes(text: &str, body: Range<usize>, mode: Mode, want: &mut [Want]) {
    let src = &text[body.clone()];
    let mut norm = std::string::String::with_capacity(src.len());
    // The offset in src of each byte of norm, and of its end.
    let mut at = Vec::with_capacity(src.len() + 1);
    for (j, c) in src.char_indices() {
        if c == '\r' && src[j + 1..].starts_with('\n') {
            continue;
        }
        norm.push(c);
        at.extend((0..c.len_utf8()).map(|k| j + k));
    }
    at.push(src.len());
    // Whether rustc reported each byte of the body as part of a
    // character, an escape or an error.
    let mut covered = vec![false; src.len()];
    let mut mark = |nr: Range<usize>, ok: Option<bool>| {
        let Some(ok) = ok else {
            return;
        };
        let start = at[nr.start];
        let end = if nr.is_empty() {
            start
        } else {
            at[nr.end - 1] + 1
        };
        covered[start..end].fill(true);
        let w = if !ok {
            Want::Either(Kind::String, Kind::Escape)
        } else if src.as_bytes()[start] == b'\\' {
            Want::Is(Some(Kind::Escape))
        } else {
            Want::Is(Some(Kind::String))
        };
        want[body.start + start..body.start + end].fill(w);
    };
    // Warnings come with ranges that overlap the others, so they're
    // left out.
    let ok = |e: Option<&rustc_literal_escaper::EscapeError>| match e {
        None => Some(true),
        Some(e) if e.is_fatal() => Some(false),
        Some(_) => None,
    };
    match mode {
        Mode::Str => rustc_literal_escaper::unescape_str(&norm, |nr, res| {
            mark(nr, ok(res.as_ref().err()));
        }),
        Mode::Bytes => rustc_literal_escaper::unescape_byte_str(&norm, |nr, res| {
            mark(nr, ok(res.as_ref().err()));
        }),
        Mode::C => rustc_literal_escaper::unescape_c_str(&norm, |nr, res| {
            mark(nr, ok(res.as_ref().err()));
        }),
    }
    // What rustc didn't report is a backslash at the end of a line, and
    // the whitespace that it skips after it. tergi colors the backslash
    // and the line break as an escape, and the rest as string.
    let b = src.as_bytes();
    let mut j = 0;
    while j < b.len() {
        if covered[j] {
            j += 1;
            continue;
        }
        let w = if b[j] == b'\\' {
            let mut e = j + 1;
            if b.get(e) == Some(&b'\r') {
                e += 1;
            }
            if b.get(e) == Some(&b'\n') {
                e += 1;
            }
            want[body.start + j..body.start + e].fill(Want::Is(Some(Kind::Escape)));
            j = e;
            continue;
        } else {
            Want::Either(Kind::String, Kind::Escape)
        };
        want[body.start + j] = w;
        j += 1;
    }
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
    (
        text[..j].matches('\n').count() + 1,
        j - start + 1,
        &text[start..end],
    )
}

fn describe(w: Want) -> std::string::String {
    match w {
        Want::Is(Some(k)) => format!("{k:?}"),
        Want::Is(None) => "nothing".into(),
        Want::Either(a, b) => format!("{a:?} or {b:?}"),
        Want::Any => "anything".into(),
    }
}

/// Compare one file. Returns the number of disagreements, and prints
/// them while fewer than SHOW have been printed in all.
fn compare_file(path: &str, text: &str, tok: &str, shown: &mut usize) -> usize {
    let exp = expected(text);
    let (got, got_idents) = read_tokens(tok, text.len());
    let mut bad = 0;
    let mut j = 0;
    while j < text.len() {
        if exp.want[j].allows(got[j]) {
            j += 1;
            continue;
        }
        bad += 1;
        let (line, column, line_text) = place(text, j);
        if *shown < SHOW {
            *shown += 1;
            let got = got[j].map_or("nothing".into(), |k| format!("{k:?}"));
            println!(
                "{path}:{line}:{column}: rustc wants {}, tergi has {got}\n    {line_text:?}",
                describe(exp.want[j])
            );
        }
        // One disagreement on each line is enough.
        j = text[j..].find('\n').map_or(text.len(), |n| j + n + 1);
    }
    // Identifiers inside tokens that rustc rejects don't count.
    let keep = |r: &Range<usize>| exp.want[r.clone()].iter().all(|&w| w != Want::Any);
    let want_idents: Vec<_> = exp.identifiers.into_iter().filter(keep).collect();
    let got_idents: Vec<_> = got_idents.into_iter().filter(keep).collect();
    if want_idents != got_idents {
        bad += 1;
        if *shown < SHOW {
            *shown += 1;
            print!(
                "{path}: identifiers differ, rustc has {} and tergi {}",
                want_idents.len(),
                got_idents.len()
            );
            if let Some((w, g)) = want_idents.iter().zip(&got_idents).find(|(w, g)| w != g) {
                let (line, _, _) = place(text, w.start.min(g.start));
                print!(
                    ". The first difference is on line {line}, where rustc has {:?} and tergi {:?}",
                    &text[w.clone()],
                    &text[g.clone()]
                );
            }
            println!();
        }
    }
    bad
}

fn compare(work: &Path) -> ExitCode {
    let list = std::fs::read_to_string(work.join("files.txt")).expect("reading files.txt");
    let (mut files, mut bytes, mut skipped, mut bad, mut bad_files, mut shown) = (0, 0, 0, 0, 0, 0);
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
        let b = compare_file(path, &text, &tok, &mut shown);
        if b > 0 {
            bad += b;
            bad_files += 1;
        }
    }
    println!("{files} files ({bytes} bytes) compared, {skipped} skipped because they aren't UTF-8");
    println!("{bad} disagreements in {bad_files} files");
    if bad > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Pieces that are likely to start or end tokens, or to confuse a lexer.
const PIECES: &[&str] = &[
    "\"", "'", "r\"", "r#\"", "\"#", "#", "##", "b'", "b\"", "br\"", "br#\"", "c\"", "cr#\"", "//",
    "///", "//!", "/*", "/**", "*/", "\\", "\\\n", "\\\r\n", "\n", "\r\n", " ", "\t", "x", "a",
    "r", "b", "c", "k", "_", "é", "中", "🦀", "\u{301}", "fn", "true", "self", "r#", "'a", "'r#",
    "'_", "'static", "1", "9", "0x", "0o", "0b", "e", "E", "e+", "e-", ".", "..", "u8", "i128",
    "usize", "f32", "f64", "\\n", "\\x41", "\\xff", "\\u{e9}", "\\u{", "}", "{", "\\'", "\\\"",
    "\\0", "\\q", "$", "?", ":", "::", "<", ">", "=", "-", "+", "!", "&", "|", "[", "]", "(", ")",
    ";", ",", "@", "#[", "#![", "'a'", "\"s\"", "/", "*", "#!", "1.", "f16", "e_", "\\u{_", "©",
    "\u{b}", "\u{85}", "\u{200e}", "\u{2028}",
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
        let len = 1 + rng.below(30);
        let text: std::string::String = (0..len).map(|_| PIECES[rng.below(PIECES.len())]).collect();
        std::fs::write(dir.join(format!("{i}.rs")), text).expect("writing a text");
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(|s| s.as_str()).collect::<Vec<_>>()[..] {
        ["compare", work] => compare(Path::new(work)),
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
                "usage: rust-check compare <work>\n       rust-check generate <directory> <cases> <seed>"
            );
            ExitCode::from(2)
        }
    }
}
