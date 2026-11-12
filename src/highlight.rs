//! Syntax highlighting for diffs, in the background.
//!
//! When we have the full text of a file, we highlight the whole file so that
//! multi-line strings and comments come out right, then pick out the lines
//! the diff shows. When we only have the diff, we highlight each hunk's old
//! and new side on its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::git::diff::{Hunk, LineKind};

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
    tx: UnboundedSender<Job>,
    current: Arc<AtomicU64>,
}

impl Highlighter {
    /// The returned future does the work. The caller should spawn it. It
    /// ends when the `Highlighter` is dropped.
    pub fn new(
        dark: bool,
        done: impl Fn(Key, FileHighlight) + Send + Sync + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (tx, rx) = unbounded_channel::<Job>();
        let current = Arc::new(AtomicU64::new(0));
        let worker = worker(rx, current.clone(), dark, Arc::new(done));
        (Highlighter { tx, current }, worker)
    }

    /// Jobs from older generations are skipped.
    pub fn set_generation(&self, generation: u64) {
        self.current.store(generation, Ordering::Relaxed);
    }

    pub fn request(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

async fn worker(
    mut rx: UnboundedReceiver<Job>,
    current: Arc<AtomicU64>,
    dark: bool,
    done: Arc<dyn Fn(Key, FileHighlight) + Send + Sync>,
) {
    // Loading the syntaxes takes a while, so do it once, off the async
    // threads.
    let loaded = crate::util::blocking(move || {
        let ss = two_face::syntax::extra_newlines();
        let themes = two_face::theme::extra();
        let theme = themes
            .get(if dark {
                two_face::theme::EmbeddedThemeName::OneHalfDark
            } else {
                two_face::theme::EmbeddedThemeName::OneHalfLight
            })
            .clone();
        (ss, theme)
    })
    .await;
    let loaded = Arc::new(loaded);
    while let Some(job) = rx.recv().await {
        if job.key.generation < current.load(Ordering::Relaxed) {
            continue;
        }
        let loaded = loaded.clone();
        let done = done.clone();
        crate::util::blocking(move || {
            let (ss, theme) = &*loaded;
            let first_line = job
                .new
                .as_deref()
                .or(job.old.as_deref())
                .and_then(|t| t.lines().next())
                .unwrap_or("");
            let Some(syntax) = find_syntax(ss, &job.path, first_line) else {
                return;
            };
            let hl = highlight_job(ss, syntax, theme, &job);
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

fn highlight_job(ss: &SyntaxSet, syntax: &SyntaxReference, theme: &Theme, job: &Job) -> FileHighlight {
    let full = |text: &Option<Arc<str>>, max_line: usize| -> Option<Vec<LineSpans>> {
        let t = text.as_ref()?;
        let count = t.lines().count();
        if count > FULL_FILE_LINES {
            return None;
        }
        Some(highlight_lines(ss, syntax, theme, t.split_inclusive('\n').take(max_line)))
    };
    let max_old = job.hunks.iter().flat_map(|h| &h.lines).filter_map(|l| l.old_no).max().unwrap_or(0) as usize;
    let max_new = job.hunks.iter().flat_map(|h| &h.lines).filter_map(|l| l.new_no).max().unwrap_or(0) as usize;
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
