//! Syntax highlighting for diffs, in the background.
//!
//! When we have the full text of a file, we highlight the whole file so that
//! multi-line strings and comments come out right, then pick out the lines
//! the diff shows. When we only have the diff, we highlight each hunk's old
//! and new side on its own.

use std::sync::Arc;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use two_face::theme::EmbeddedThemeName;

use crate::git::diff::{Hunk, LineKind};
use crate::util::Generation;

/// Foreground color spans for one line, as byte ranges into the line text.
pub type LineSpans = Vec<(u32, u32, Color)>;

#[derive(Debug, Clone, Default)]
pub struct FileHighlight {
    /// Indexed by hunk, then by line within the hunk.
    pub lines: Vec<Vec<LineSpans>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub generation: u64,
    pub section: u8,
    pub file: usize,
}

pub struct Job {
    pub key: Key,
    pub path: String,
    pub hunks: Vec<Hunk>,
    pub old: Option<Arc<str>>,
    pub new: Option<Arc<str>>,
}

/// Files with more lines than this are highlighted hunk by hunk.
const FULL_FILE_LINES: usize = 20_000;

pub struct Highlighter {
    tx: UnboundedSender<Msg>,
}

enum Msg {
    Job(Job),
    /// Use this syntax theme for the jobs that follow.
    Theme(EmbeddedThemeName),
}

impl Highlighter {
    /// The returned future does the work. The caller should spawn it. It
    /// ends when the `Highlighter` is dropped. Jobs whose key has a
    /// generation that is no longer current in `files` are skipped.
    pub fn new(
        theme: EmbeddedThemeName,
        files: Generation,
        done: impl Fn(Key, FileHighlight) + Send + Sync + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (tx, rx) = unbounded_channel::<Msg>();
        let worker = worker(rx, files, theme, Arc::new(done));
        (Highlighter { tx }, worker)
    }

    pub fn request(&self, job: Job) {
        let _ = self.tx.send(Msg::Job(job));
    }

    /// Switch syntax themes. Jobs already sent still get the old one, so the
    /// caller should move its generation on.
    pub fn set_theme(&self, theme: EmbeddedThemeName) {
        let _ = self.tx.send(Msg::Theme(theme));
    }
}

async fn worker(
    mut rx: UnboundedReceiver<Msg>,
    files: Generation,
    mut theme: EmbeddedThemeName,
    done: Arc<dyn Fn(Key, FileHighlight) + Send + Sync>,
) {
    // Loading the syntaxes takes a while, so do it once, off the async
    // threads. The theme set only loads a theme the first time it's used.
    let loaded =
        crate::util::blocking(|| (two_face::syntax::extra_newlines(), two_face::theme::extra()))
            .await;
    let loaded = Arc::new(loaded);
    while let Some(msg) = rx.recv().await {
        let job = match msg {
            Msg::Job(job) => job,
            Msg::Theme(t) => {
                theme = t;
                continue;
            }
        };
        if !files.is_current(job.key.generation) {
            continue;
        }
        let loaded = loaded.clone();
        let done = done.clone();
        crate::util::blocking(move || {
            let (ss, themes) = &*loaded;
            let first_line = job
                .new
                .as_deref()
                .or(job.old.as_deref())
                .and_then(|t| t.lines().next())
                .unwrap_or("");
            let Some(syntax) = find_syntax(ss, &job.path, first_line) else {
                return;
            };
            let hl = highlight_job(ss, syntax, themes.get(theme), &job);
            done(job.key, hl);
        })
        .await;
    }
}

fn find_syntax<'a>(ss: &'a SyntaxSet, path: &str, first_line: &str) -> Option<&'a SyntaxReference> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
    let s = ss
        .find_syntax_by_extension(name)
        .or_else(|| ss.find_syntax_by_extension(ext))
        .or_else(|| ss.find_syntax_by_first_line(first_line))?;
    (s.name != "Plain Text").then_some(s)
}

fn to_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Highlight `lines` in order, keeping parser state from line to line.
fn highlight_lines<'a>(
    ss: &SyntaxSet,
    syntax: &SyntaxReference,
    theme: &Theme,
    lines: impl Iterator<Item = &'a str>,
) -> Vec<LineSpans> {
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        // The syntax set expects lines with their newline.
        let owned;
        let input = if line.ends_with('\n') {
            line
        } else {
            owned = format!("{line}\n");
            &owned
        };
        if let Ok(regions) = h.highlight_line(input, ss) {
            let mut pos = 0u32;
            for (style, text) in regions {
                let len = text.len() as u32;
                spans.push((pos, pos + len, to_color(style.foreground)));
                pos += len;
            }
        }
        out.push(spans);
    }
    out
}

fn highlight_job(
    ss: &SyntaxSet,
    syntax: &SyntaxReference,
    theme: &Theme,
    job: &Job,
) -> FileHighlight {
    let full = |text: &Option<Arc<str>>, max_line: usize| -> Option<Vec<LineSpans>> {
        let t = text.as_ref()?;
        let count = t.lines().count();
        if count > FULL_FILE_LINES {
            return None;
        }
        Some(highlight_lines(
            ss,
            syntax,
            theme,
            t.split_inclusive('\n').take(max_line),
        ))
    };
    let max_old = job
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter_map(|l| l.old_no)
        .max()
        .unwrap_or(0) as usize;
    let max_new = job
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter_map(|l| l.new_no)
        .max()
        .unwrap_or(0) as usize;
    let old_full = full(&job.old, max_old);
    let new_full = full(&job.new, max_new);

    let mut result = FileHighlight::default();
    for hunk in &job.hunks {
        // Highlight each side of the hunk on its own when we don't have the
        // full text for that side.
        let old_side = if old_full.is_none() {
            let lines: Vec<&str> = hunk
                .lines
                .iter()
                .filter(|l| l.kind != LineKind::Add)
                .map(|l| l.text.as_str())
                .collect();
            Some(highlight_lines(ss, syntax, theme, lines.into_iter()))
        } else {
            None
        };
        let new_side = if new_full.is_none() {
            let lines: Vec<&str> = hunk
                .lines
                .iter()
                .filter(|l| l.kind != LineKind::Del)
                .map(|l| l.text.as_str())
                .collect();
            Some(highlight_lines(ss, syntax, theme, lines.into_iter()))
        } else {
            None
        };
        let mut oi = 0;
        let mut ni = 0;
        let mut out = Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            let from_old = |oi: usize| -> LineSpans {
                match (&old_full, &old_side) {
                    (Some(f), _) => line
                        .old_no
                        .and_then(|n| f.get(n as usize - 1))
                        .cloned()
                        .unwrap_or_default(),
                    (None, Some(s)) => s.get(oi).cloned().unwrap_or_default(),
                    _ => Vec::new(),
                }
            };
            let from_new = |ni: usize| -> LineSpans {
                match (&new_full, &new_side) {
                    (Some(f), _) => line
                        .new_no
                        .and_then(|n| f.get(n as usize - 1))
                        .cloned()
                        .unwrap_or_default(),
                    (None, Some(s)) => s.get(ni).cloned().unwrap_or_default(),
                    _ => Vec::new(),
                }
            };
            let spans = match line.kind {
                LineKind::Del => from_old(oi),
                LineKind::Add => from_new(ni),
                // Prefer the new side for context, which is what's on disk.
                LineKind::Context => {
                    if new_full.is_some() || old_full.is_none() {
                        from_new(ni)
                    } else {
                        from_old(oi)
                    }
                }
            };
            if line.kind != LineKind::Add {
                oi += 1;
            }
            if line.kind != LineKind::Del {
                ni += 1;
            }
            out.push(spans);
        }
        result.lines.push(out);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use proptest::prelude::*;
    use two_face::theme::EmbeddedLazyThemeSet;

    use super::*;
    use crate::git::commit::diff_texts;

    static LOADED: LazyLock<(SyntaxSet, EmbeddedLazyThemeSet)> =
        LazyLock::new(|| (two_face::syntax::extra_newlines(), two_face::theme::extra()));

    // JavaScript is left out until the hang in todo/bugs.md is fixed.
    const PATHS: &[&str] = &[
        "a.rs", "a.py", "a.c", "a.md", "a.html", "a.sh", "a.json", "a.toml", "Makefile",
    ];

    fn syntax(path: &str) -> &'static SyntaxReference {
        find_syntax(&LOADED.0, path, "").expect("a syntax for each test path")
    }

    fn theme() -> &'static Theme {
        LOADED.1.get(EmbeddedThemeName::Base16OceanDark)
    }

    /// A line made of pieces that start or end strings and comments in some
    /// syntax, along with tabs, wide characters and a stray carriage return.
    fn line() -> impl Strategy<Value = String> {
        let piece = prop::sample::select(vec![
            "x", "fn", " ", "\t", "\"", "'", "`", "```", "\"\"\"", "//", "/*", "*/", "#", "<!--",
            "-->", "<a>", "{", "}", "(", ")", "\\", "$x", "=", ";", "0x1F", "r#\"", "- ", "中",
            "👍", "e\u{301}", "\r",
        ]);
        prop::collection::vec(piece, 0..8).prop_map(|p| p.concat())
    }

    /// Lines picked from a small pool, so that two texts share some, each
    /// ending in LF or CRLF, and maybe without a final newline.
    fn text(pool: Vec<String>) -> impl Strategy<Value = String> {
        let n = pool.len();
        (
            prop::collection::vec((0..n, any::<bool>()), 0..20),
            any::<bool>(),
        )
            .prop_map(move |(lines, eol)| {
                let mut s = String::new();
                for (i, &(k, crlf)) in lines.iter().enumerate() {
                    s.push_str(&pool[k]);
                    if i + 1 < lines.len() || eol {
                        s.push_str(if crlf { "\r\n" } else { "\n" });
                    }
                }
                s
            })
    }

    fn texts() -> impl Strategy<Value = (String, String)> {
        prop::collection::vec(line(), 1..6).prop_flat_map(|pool| (text(pool.clone()), text(pool)))
    }

    /// The spans of a line must follow each other from its start, without
    /// gaps, to the end of the newline that the highlighter adds or keeps.
    /// `body` is the line without its `\n`. Every span must start and end
    /// on a character boundary.
    fn check_spans(body: &str, spans: &LineSpans) -> Result<(), TestCaseError> {
        prop_assert!(!spans.is_empty(), "no spans for {body:?}");
        let mut pos = 0;
        for &(s, e, _) in spans {
            prop_assert_eq!(s, pos, "gap or overlap in {:?}: {:?}", body, spans);
            prop_assert!(s < e, "empty span in {body:?}: {spans:?}");
            prop_assert!(
                body.is_char_boundary((e as usize).min(body.len())),
                "span ends inside a character of {body:?}: {spans:?}"
            );
            pos = e;
        }
        prop_assert_eq!(pos as usize, body.len() + 1, "{:?}: {:?}", body, spans);
        Ok(())
    }

    fn job(path: &str, hunks: &[Hunk], old: Option<&str>, new: Option<&str>) -> Job {
        Job {
            key: Key {
                generation: 0,
                section: 0,
                file: 0,
            },
            path: path.to_string(),
            hunks: hunks.to_vec(),
            old: old.map(Arc::from),
            new: new.map(Arc::from),
        }
    }

    proptest! {
        #[test]
        fn lines_are_covered(
            path in prop::sample::select(PATHS),
            (text, _) in texts(),
        ) {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let out = highlight_lines(&LOADED.0, syntax(path), theme(), lines.iter().copied());
            prop_assert_eq!(out.len(), lines.len());
            for (l, spans) in lines.iter().zip(&out) {
                check_spans(l.strip_suffix('\n').unwrap_or(l), spans)?;
            }
        }

        #[test]
        fn every_hunk_line_is_covered(
            path in prop::sample::select(PATHS),
            (old, new) in texts(),
            context in 0u32..4,
        ) {
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let (ss, syntax, theme) = (&LOADED.0, syntax(path), theme());
            let run = |o: bool, n: bool| {
                let j = job(path, &hunks, o.then_some(&*old), n.then_some(&*new));
                highlight_job(ss, syntax, theme, &j)
            };
            let full = run(true, true);
            let parts = run(false, false);
            for (has_old, has_new) in [(true, true), (true, false), (false, true), (false, false)] {
                let hl = run(has_old, has_new);
                prop_assert_eq!(hl.lines.len(), hunks.len());
                for (h, (hunk, spans)) in hunks.iter().zip(&hl.lines).enumerate() {
                    prop_assert_eq!(spans.len(), hunk.lines.len());
                    for (i, (line, spans)) in hunk.lines.iter().zip(spans).enumerate() {
                        check_spans(&line.text, spans)?;
                        // A line with the full text of its side gets the colors
                        // that it gets when both sides have it, and one without
                        // gets the colors that it gets when neither side has it.
                        // Context lines use the new side when they can.
                        let side = match line.kind {
                            LineKind::Del => Some(has_old),
                            LineKind::Add => Some(has_new),
                            LineKind::Context if has_new || !has_old => Some(has_new),
                            LineKind::Context => None,
                        };
                        match side {
                            Some(true) => prop_assert_eq!(spans, &full.lines[h][i]),
                            Some(false) => prop_assert_eq!(spans, &parts.lines[h][i]),
                            None => {}
                        }
                    }
                }
            }
            // A hunk that starts at the top of both files highlights the same
            // lines from the top on its own, so it gets the same colors.
            for (h, hunk) in hunks.iter().enumerate() {
                if hunk.old_start <= 1 && hunk.new_start <= 1 {
                    prop_assert_eq!(&full.lines[h], &parts.lines[h]);
                }
            }
        }
    }
}
