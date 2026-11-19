//! Syntax highlighting for diffs, in the background, with tree-sitter.
//!
//! Tree-sitter parses a whole text at once, and text further down can
//! change how a line reads. So when we have the full text of a side, we
//! highlight all of it up to the last line the diff shows, then pick out
//! those lines. When we only have the diff, we highlight each hunk's old
//! and new side on its own, which tree-sitter parses as well as it can.

mod languages;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ratatui::style::Color;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tree_sitter_highlight::{HighlightEvent, Highlighter as TsHighlighter};

use crate::git::diff::{DiffLine, Hunk, LineKind};
use crate::theme::{Syntax, rgb};
use crate::util::Generation;

use languages::Lang;

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

/// The names that highlight queries give to pieces of code, each with the
/// name that themes use for it. The queries don't all use the same names.
/// Older ones use names like `conditional` and `text.title` where newer
/// ones say `keyword.conditional` and `markup.heading`.
///
/// Tree-sitter gives a capture the longest name here whose parts it all
/// has, so `keyword.return.special` gets `keyword.return`. When two names
/// match equally well, the first one wins, so `function.method.builtin`
/// gets `function.builtin`.
const NAMES: &[(&str, &str)] = &[
    ("none", "none"),
    ("embedded", "embedded"),
    ("comment", "comment"),
    ("comment.documentation", "comment.documentation"),
    ("keyword", "keyword"),
    ("keyword.conditional", "keyword.conditional"),
    ("keyword.coroutine", "keyword.coroutine"),
    ("keyword.debug", "keyword.debug"),
    ("keyword.directive", "keyword.directive"),
    ("keyword.exception", "keyword.exception"),
    ("keyword.function", "keyword.function"),
    ("keyword.import", "keyword.import"),
    ("keyword.modifier", "keyword.modifier"),
    ("keyword.operator", "keyword.operator"),
    ("keyword.repeat", "keyword.repeat"),
    ("keyword.return", "keyword.return"),
    ("keyword.type", "keyword.type"),
    ("string", "string"),
    ("string.escape", "string.escape"),
    ("string.regexp", "string.regexp"),
    ("string.special", "string.special"),
    ("string.special.key", "string.special.key"),
    ("string.special.path", "string.special.path"),
    ("string.special.symbol", "string.special.symbol"),
    ("string.special.uri", "string.special.uri"),
    ("character", "character"),
    ("number", "number"),
    ("number.float", "number.float"),
    ("boolean", "boolean"),
    ("constant", "constant"),
    ("constant.builtin", "constant.builtin"),
    ("constant.character", "constant.character"),
    ("constant.macro", "constant.macro"),
    ("function", "function"),
    ("function.builtin", "function.builtin"),
    ("function.call", "function.call"),
    ("function.macro", "function.macro"),
    ("function.method", "function.method"),
    ("function.special", "function.special"),
    ("constructor", "constructor"),
    ("type", "type"),
    ("type.builtin", "type.builtin"),
    ("type.definition", "type.definition"),
    ("type.qualifier", "type.qualifier"),
    ("variable", "variable"),
    ("variable.builtin", "variable.builtin"),
    ("variable.parameter", "variable.parameter"),
    ("property", "property"),
    ("attribute", "attribute"),
    ("label", "label"),
    ("module", "module"),
    ("module.builtin", "module.builtin"),
    ("operator", "operator"),
    ("punctuation", "punctuation"),
    ("punctuation.bracket", "punctuation.bracket"),
    ("punctuation.delimiter", "punctuation.delimiter"),
    ("punctuation.special", "punctuation.special"),
    ("tag", "tag"),
    ("tag.attribute", "tag.attribute"),
    ("markup", "markup"),
    ("markup.heading", "markup.heading"),
    ("markup.italic", "markup.italic"),
    ("markup.link", "markup.link"),
    ("markup.link.url", "markup.link.url"),
    ("markup.list", "markup.list"),
    ("markup.quote", "markup.quote"),
    ("markup.raw", "markup.raw"),
    ("markup.strong", "markup.strong"),
    // Other names for the ones above.
    ("character.special", "string.escape"),
    ("escape", "string.escape"),
    ("string.regex", "string.regexp"),
    ("string.special.regex", "string.regexp"),
    ("variable.member", "property"),
    ("field", "property"),
    ("parameter", "variable.parameter"),
    ("method", "function.method"),
    ("conditional", "keyword.conditional"),
    ("repeat", "keyword.repeat"),
    ("include", "keyword.import"),
    ("exception", "keyword.exception"),
    ("namespace", "module"),
    ("text.title", "markup.heading"),
    ("text.literal", "markup.raw"),
    ("text.uri", "markup.link.url"),
    ("text.reference", "markup.link"),
    ("text.emphasis", "markup.italic"),
];

/// The color of each name in `NAMES`, for one theme.
#[derive(Debug, Clone)]
pub struct Colors {
    fg: Color,
    by_name: Vec<Option<Color>>,
}

impl Colors {
    /// A name the theme doesn't color takes the color of the longest name
    /// that it starts with, so `keyword` colors `keyword.return` too. With
    /// none of those, it takes the color of whatever it sits inside, such
    /// as the string around an escape. `none` and `embedded` reset the
    /// color, as in the code inside a template string.
    pub fn new(syntax: &Syntax) -> Colors {
        let fg = rgb(syntax.fg);
        let by_name = NAMES
            .iter()
            .map(|&(_, name)| {
                if name == "none" || name == "embedded" {
                    return Some(fg);
                }
                syntax
                    .colors
                    .iter()
                    .filter(|(key, _)| is_part_of(name, key))
                    .max_by_key(|(key, _)| key.len())
                    .map(|&(_, c)| rgb(c))
            })
            .collect();
        Colors { fg, by_name }
    }

    fn get(&self, highlight: usize) -> Option<Color> {
        self.by_name.get(highlight).copied().flatten()
    }
}

/// Whether `prefix` is `name` or starts it, as `keyword` does for
/// `keyword.return`.
fn is_part_of(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// Stop a job that runs longer than this, so that one huge file can't hold
/// up the files after it.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How often a running job checks whether it should stop.
const CHECK_EVERY: Duration = Duration::from_millis(50);

pub struct Highlighter {
    tx: UnboundedSender<Msg>,
}

enum Msg {
    Job(Job),
    /// Use these colors for the jobs that follow.
    Theme(Colors),
}

impl Highlighter {
    /// The returned future does the work. The caller should spawn it. It
    /// ends when the `Highlighter` is dropped. Jobs whose key has a
    /// generation that is no longer current in `files` are skipped, or
    /// stopped if they have started.
    pub fn new(
        colors: Colors,
        files: Generation,
        done: impl Fn(Key, FileHighlight) + Send + Sync + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (tx, rx) = unbounded_channel::<Msg>();
        let worker = worker(rx, files, colors, done);
        (Highlighter { tx }, worker)
    }

    pub fn request(&self, job: Job) {
        let _ = self.tx.send(Msg::Job(job));
    }

    /// Switch syntax themes. Jobs already sent still get the old colors, so
    /// the caller should move its generation on.
    pub fn set_theme(&self, colors: Colors) {
        let _ = self.tx.send(Msg::Theme(colors));
    }
}

async fn worker(
    mut rx: UnboundedReceiver<Msg>,
    files: Generation,
    colors: Colors,
    done: impl Fn(Key, FileHighlight),
) {
    let mut colors = Arc::new(colors);
    while let Some(msg) = rx.recv().await {
        let job = match msg {
            Msg::Job(job) => job,
            Msg::Theme(c) => {
                colors = Arc::new(c);
                continue;
            }
        };
        let generation = job.key.generation;
        if !files.is_current(generation) {
            continue;
        }
        let first_line = job
            .new
            .as_deref()
            .or(job.old.as_deref())
            .and_then(|t| t.lines().next())
            .unwrap_or("");
        let Some(lang) = languages::detect(&job.path, first_line) else {
            continue;
        };
        let cancel = Arc::new(AtomicUsize::new(0));
        let work = crate::util::blocking({
            let colors = colors.clone();
            let cancel = cancel.clone();
            move || highlight_job(lang, &colors, &job, &cancel).map(|hl| (job.key, hl))
        });
        tokio::pin!(work);
        let started = Instant::now();
        let result = loop {
            tokio::select! {
                result = &mut work => break result,
                () = tokio::time::sleep(CHECK_EVERY) => {
                    if !files.is_current(generation) || started.elapsed() > TIMEOUT {
                        cancel.store(1, Ordering::Relaxed);
                    }
                }
            }
        };
        if let Some((key, hl)) = result {
            done(key, hl);
        }
    }
}

/// Highlight `text` and return the spans of its first `max_lines` lines.
/// Give up with `None` if `cancel` becomes nonzero.
fn highlight_text(
    lang: &Lang,
    colors: &Colors,
    text: &str,
    max_lines: usize,
    cancel: &AtomicUsize,
) -> Option<Vec<LineSpans>> {
    let mut lines = Lines::new(text, max_lines, colors.fg);
    if max_lines == 0 {
        return Some(lines.finish());
    }
    let mut hl = TsHighlighter::new();
    let events = hl
        .highlight(
            lang.config()?,
            text.as_bytes(),
            None,
            Some(cancel),
            |name| languages::named(name)?.config(),
        )
        .ok()?;
    // The colors of the highlights we are inside, innermost last. A name
    // the theme has no color for has `None`.
    let mut stack: Vec<Option<Color>> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(colors.get(h.0)),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let color = stack.iter().rev().find_map(|c| *c).unwrap_or(colors.fg);
                if !lines.add(start, end, color) {
                    break;
                }
            }
        }
    }
    Some(lines.finish())
}

/// Collects colored byte ranges of a text into spans for each line.
struct Lines<'a> {
    text: &'a str,
    max_lines: usize,
    fg: Color,
    lines: Vec<LineSpans>,
    /// Where the line being built starts in `text`.
    line_start: usize,
    current: LineSpans,
    /// How far into `text` the ranges have reached.
    pos: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str, max_lines: usize, fg: Color) -> Self {
        Lines {
            text,
            max_lines,
            fg,
            lines: Vec::new(),
            line_start: 0,
            current: Vec::new(),
            pos: 0,
        }
    }

    /// Color the bytes from `start` to `end`. Return false once we have all
    /// the lines we want.
    fn add(&mut self, start: usize, mut end: usize, color: Color) -> bool {
        // Ranges come in order, but don't let a stray one go backwards.
        let mut start = start.max(self.pos);
        end = end.min(self.text.len());
        while start < end {
            let piece = &self.text[start..end];
            match piece.find('\n') {
                Some(i) => {
                    let nl = start + i + 1;
                    self.push(start, nl, color);
                    self.lines.push(std::mem::take(&mut self.current));
                    self.line_start = nl;
                    start = nl;
                    if self.lines.len() >= self.max_lines {
                        self.pos = nl;
                        return false;
                    }
                }
                None => {
                    self.push(start, end, color);
                    start = end;
                }
            }
        }
        self.pos = self.pos.max(end);
        true
    }

    fn push(&mut self, start: usize, end: usize, color: Color) {
        let s = (start - self.line_start) as u32;
        let e = (end - self.line_start) as u32;
        match self.current.last_mut() {
            Some(last) if last.1 == s && last.2 == color => last.1 = e,
            _ => self.current.push((s, e, color)),
        }
    }

    /// Finish the lines. Each line's spans reach one byte past its text, as
    /// if it ended in a newline, even when the text's last line doesn't.
    fn finish(mut self) -> Vec<LineSpans> {
        if self.lines.len() < self.max_lines && self.pos < self.text.len() {
            // Highlighting stopped early. Leave the rest plain.
            self.add(self.pos, self.text.len(), self.fg);
        }
        if self.lines.len() < self.max_lines && self.line_start < self.text.len() {
            let mut line = std::mem::take(&mut self.current);
            match line.last_mut() {
                Some(last) => last.1 += 1,
                None => line.push((0, 1, self.fg)),
            }
            self.lines.push(line);
        }
        self.lines.truncate(self.max_lines);
        self.lines
    }
}

fn highlight_job(
    lang: &Lang,
    colors: &Colors,
    job: &Job,
    cancel: &AtomicUsize,
) -> Option<FileHighlight> {
    let max_line = |no: fn(&DiffLine) -> Option<u32>| {
        job.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(no)
            .max()
            .unwrap_or(0) as usize
    };
    let full = |text: &Option<Arc<str>>, max_line: usize| -> Option<Option<Vec<LineSpans>>> {
        let Some(t) = text else {
            return Some(None);
        };
        highlight_text(lang, colors, t, max_line, cancel).map(Some)
    };
    let old_full = full(&job.old, max_line(|l| l.old_no))?;
    let new_full = full(&job.new, max_line(|l| l.new_no))?;

    // Highlight the lines of one side of a hunk as a text of their own.
    let side = |hunk: &Hunk, skip: LineKind| -> Option<Vec<LineSpans>> {
        let mut text = String::new();
        let mut count = 0;
        for l in hunk.lines.iter().filter(|l| l.kind != skip) {
            text.push_str(&l.text);
            if !l.text.ends_with('\n') {
                text.push('\n');
            }
            count += 1;
        }
        highlight_text(lang, colors, &text, count, cancel)
    };

    let mut result = FileHighlight::default();
    for hunk in &job.hunks {
        let old_side = match old_full {
            Some(_) => None,
            None => Some(side(hunk, LineKind::Add)?),
        };
        let new_side = match new_full {
            Some(_) => None,
            None => Some(side(hunk, LineKind::Del)?),
        };
        let mut oi = 0;
        let mut ni = 0;
        let mut out = Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            let pick = |full: &Option<Vec<LineSpans>>,
                        side: &Option<Vec<LineSpans>>,
                        no: Option<u32>,
                        i: usize|
             -> LineSpans {
                match (full, side) {
                    (Some(f), _) => no
                        .and_then(|n| f.get(n as usize - 1))
                        .cloned()
                        .unwrap_or_default(),
                    (None, Some(s)) => s.get(i).cloned().unwrap_or_default(),
                    _ => Vec::new(),
                }
            };
            let from_old = || pick(&old_full, &old_side, line.old_no, oi);
            let from_new = || pick(&new_full, &new_side, line.new_no, ni);
            let spans = match line.kind {
                LineKind::Del => from_old(),
                LineKind::Add => from_new(),
                // Prefer the new side for context, which is what's on disk.
                LineKind::Context => {
                    if new_full.is_some() || old_full.is_none() {
                        from_new()
                    } else {
                        from_old()
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
    Some(result)
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use proptest::prelude::*;

    use super::*;
    use crate::git::commit::diff_texts;
    use crate::theme::Theme;

    static COLORS: LazyLock<Colors> = LazyLock::new(|| Colors::new(Theme::default().syntax));

    const PATHS: &[&str] = &[
        "a.rs", "a.py", "a.c", "a.md", "a.html", "a.sh", "a.json", "a.toml", "Makefile", "a.js",
        "a.tsx", "a.php",
    ];

    fn lang(path: &str) -> &'static Lang {
        languages::detect(path, "").expect("a language for each test path")
    }

    fn highlight_lines(path: &str, text: &str) -> Vec<LineSpans> {
        let n = text.split_inclusive('\n').count();
        highlight_text(lang(path), &COLORS, text, n, &AtomicUsize::new(0))
            .expect("highlighting finishes")
    }

    fn highlight(path: &str, job: &Job) -> FileHighlight {
        highlight_job(lang(path), &COLORS, job, &AtomicUsize::new(0))
            .expect("highlighting finishes")
    }

    #[test]
    fn colors_code() {
        let theme = Theme::default();
        let color = |name: &str| {
            let (_, hex) = theme
                .syntax
                .colors
                .iter()
                .find(|(n, _)| *n == name)
                .expect("the theme has this name");
            rgb(*hex)
        };
        let fg = rgb(theme.syntax.fg);
        let spans = highlight_lines("a.rs", "fn main() {\n    let x = \"hi\";\n}");
        assert_eq!(
            spans,
            [
                vec![
                    (0, 2, color("keyword")),
                    (2, 3, fg),
                    (3, 7, color("function")),
                    (7, 12, fg),
                ],
                vec![
                    (0, 4, fg),
                    (4, 7, color("keyword")),
                    (7, 12, fg),
                    (12, 16, color("string")),
                    (16, 18, fg),
                ],
                vec![(0, 2, fg)],
            ]
        );
    }

    #[test]
    fn markdown_code_blocks_use_their_language() {
        let code = "const a = 2 /* c */; // d\n";
        let md = highlight_lines("a.md", &format!("# Hi\n\n```js\n{code}```\n"));
        let js = highlight_lines("a.js", code);
        assert_eq!(md[3], js[0]);
    }

    #[test]
    fn markdown_colors_inline_code() {
        let theme = Theme::default();
        let raw = theme
            .syntax
            .colors
            .iter()
            .find(|(n, _)| *n == "markup.raw")
            .map(|&(_, c)| rgb(c))
            .expect("the theme colors inline code");
        let spans = highlight_lines("a.md", "a `b = c` d\n");
        assert!(spans[0].contains(&(2, 9, raw)), "{spans:?}");
    }

    #[test]
    fn javascript_comments_after_values_finish() {
        // Syntect's JavaScript syntax looped forever on these.
        for text in [
            "a = 1 /* c */\n",
            "return x /* c */\n",
            "x = `a` /* c */\n",
            "\"`a` /*\"\n",
        ] {
            let spans = highlight_lines("a.js", text);
            assert_eq!(spans.len(), 1);
        }
    }

    #[test]
    fn cancelled_jobs_stop() {
        let text = "fn f() {}\n".repeat(1000);
        let cancel = AtomicUsize::new(1);
        assert!(highlight_text(lang("a.rs"), &COLORS, &text, 1000, &cancel).is_none());
    }

    #[test]
    fn theme_names_exist() {
        for (id, _) in Theme::list() {
            let Some(theme) = Theme::named(id) else {
                continue;
            };
            for (key, _) in theme.syntax.colors {
                assert!(
                    NAMES.iter().any(|(_, name)| is_part_of(name, key)),
                    "{id}: no highlight is called {key}"
                );
            }
        }
    }

    /// A line made of pieces that start or end strings and comments in some
    /// language, along with tabs, wide characters and a stray carriage
    /// return.
    fn line() -> impl Strategy<Value = String> {
        let piece = prop::sample::select(vec![
            "x", "fn", " ", "\t", "\"", "'", "`", "```", "\"\"\"", "//", "/*", "*/", "#", "<!--",
            "-->", "<a>", "{", "}", "(", ")", "\\", "$x", "=", ";", "0x1F", "r#\"", "- ", "中",
            "👍", "e\u{301}", "\r", "<?php", "?>", "${",
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
    /// gaps, to one byte past its end, where its newline is or would be.
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

    fn job(hunks: &[Hunk], old: Option<&str>, new: Option<&str>) -> Job {
        Job {
            key: Key {
                generation: 0,
                section: 0,
                file: 0,
            },
            path: String::new(),
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
            let out = highlight_lines(path, &text);
            prop_assert_eq!(out.len(), lines.len());
            for (l, spans) in lines.iter().zip(&out) {
                check_spans(l.strip_suffix('\n').unwrap_or(l), spans)?;
            }
        }

        #[test]
        fn fewer_lines_are_a_prefix(
            path in prop::sample::select(PATHS),
            (text, _) in texts(),
            n in 0usize..25,
        ) {
            let all = highlight_lines(path, &text);
            let some = highlight_text(lang(path), &COLORS, &text, n, &AtomicUsize::new(0));
            prop_assert_eq!(some.as_deref(), Some(&all[..n.min(all.len())]));
        }

        #[test]
        fn every_hunk_line_is_covered(
            path in prop::sample::select(PATHS),
            (old, new) in texts(),
            context in 0u32..4,
        ) {
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let run = |o: bool, n: bool| {
                highlight(path, &job(&hunks, o.then_some(&*old), n.then_some(&*new)))
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
        }
    }
}
