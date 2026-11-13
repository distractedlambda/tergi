//! The detail pane: a commit's header and diff, or the staging view, laid
//! out as a flat list of rows so it can be scrolled and drawn quickly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gix::ObjectId;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::cmd::{Cmd, Unit};
use crate::config::Config;
use crate::git::commit::{CommitDetail, FileTexts};
use crate::git::diff::{FileDiff, LineKind};
use crate::git::refs::{Label, LabelKind};
use crate::git::words::{Marks, hunk_marks};
use crate::git::workdir::Workdir;
use crate::highlight::FileHighlight;
use crate::theme::Theme;
use crate::util::{Generation, format_date};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    None,
    Workdir,
    Commit(ObjectId),
    Stash(usize, ObjectId),
}

impl Source {
    /// The commit this source shows, if it shows one.
    pub fn commit_id(&self) -> Option<ObjectId> {
        match self {
            Source::Commit(id) | Source::Stash(_, id) => Some(*id),
            Source::None | Source::Workdir => None,
        }
    }
}

/// A line selection: the section, the file index, and `(hunk, line)` pairs.
pub type LineSelection = (Section, usize, Vec<(usize, usize)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Commit = 0,
    Conflicted = 1,
    Untracked = 2,
    Unstaged = 3,
    Staged = 4,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Commit => "Files",
            Section::Conflicted => "Merge conflicts",
            Section::Untracked => "Untracked files",
            Section::Unstaged => "Unstaged changes",
            Section::Staged => "Staged changes",
        }
    }

    pub fn from_u8(n: u8) -> Section {
        match n {
            1 => Section::Conflicted,
            2 => Section::Untracked,
            3 => Section::Unstaged,
            4 => Section::Staged,
            _ => Section::Commit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Meta(usize),
    Blank,
    Section(Section),
    File {
        sec: Section,
        file: usize,
    },
    Notice {
        sec: Section,
        file: usize,
        text: String,
    },
    Hunk {
        sec: Section,
        file: usize,
        hunk: usize,
    },
    Line {
        sec: Section,
        file: usize,
        hunk: usize,
        line: usize,
    },
    Split {
        sec: Section,
        file: usize,
        hunk: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
    Empty(String),
}

impl Row {
    pub fn file(&self) -> Option<(Section, usize)> {
        match *self {
            Row::File { sec, file }
            | Row::Notice { sec, file, .. }
            | Row::Hunk { sec, file, .. }
            | Row::Line { sec, file, .. }
            | Row::Split { sec, file, .. } => Some((sec, file)),
            _ => None,
        }
    }

    pub fn section(&self) -> Option<Section> {
        match *self {
            Row::Section(s) => Some(s),
            _ => self.file().map(|f| f.0),
        }
    }
}

pub struct MetaLine {
    pub line: Line<'static>,
    pub click: Option<Cmd>,
}

/// Identifies a row across rebuilds, so the cursor stays put.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKey {
    Meta(usize),
    Section(Section),
    File(Section, String),
    Hunk(Section, String, usize),
    Line(Section, String, usize, usize),
    Other,
}

pub struct Detail {
    pub source: Source,
    pub commit: Option<Arc<CommitDetail>>,
    pub loading: bool,
    /// Moves on whenever the files shown change, so stale highlight results
    /// can be dropped. The highlighter shares it.
    pub generation: Generation,
    /// The id of the latest commit load request.
    pub request: u64,
    pub rows: Vec<Row>,
    pub meta: Vec<MetaLine>,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub scroll: usize,
    pub hscroll: usize,
    pub collapsed: HashSet<(Section, String)>,
    pub collapsed_sections: HashSet<Section>,
    seen: HashSet<(Section, String)>,
    pub highlights: HashMap<(u8, usize), Arc<FileHighlight>>,
    pub requested: HashSet<(u8, usize)>,
    pub marks: HashMap<(Section, usize), Arc<Vec<Marks>>>,
    /// Width of the line number columns.
    pub gutter: usize,
    /// File paths per section as of the last rebuild, for keeping the
    /// cursor in place.
    old_paths: HashMap<Section, Vec<String>>,
}

impl Default for Detail {
    fn default() -> Self {
        Detail {
            source: Source::None,
            commit: None,
            loading: false,
            generation: Generation::default(),
            request: 0,
            rows: Vec::new(),
            meta: Vec::new(),
            cursor: 0,
            anchor: None,
            scroll: 0,
            hscroll: 0,
            collapsed: HashSet::new(),
            collapsed_sections: HashSet::new(),
            seen: HashSet::new(),
            highlights: HashMap::new(),
            requested: HashSet::new(),
            marks: HashMap::new(),
            gutter: 4,
            old_paths: HashMap::new(),
        }
    }
}

pub fn section_files<'a>(
    workdir: Option<&'a Workdir>,
    commit: Option<&'a CommitDetail>,
    sec: Section,
) -> &'a [FileDiff] {
    match sec {
        Section::Commit => commit.map_or(&[], |c| &c.files),
        Section::Conflicted => workdir.map_or(&[], |w| &w.conflicted),
        Section::Untracked => workdir.map_or(&[], |w| &w.untracked),
        Section::Unstaged => workdir.map_or(&[], |w| &w.unstaged),
        Section::Staged => workdir.map_or(&[], |w| &w.staged),
    }
}

pub fn section_texts<'a>(
    workdir: Option<&'a Workdir>,
    commit: Option<&'a CommitDetail>,
    sec: Section,
) -> Option<&'a [FileTexts]> {
    match sec {
        Section::Commit => commit.map(|c| c.texts.as_slice()),
        Section::Untracked => workdir.map(|w| w.untracked_texts.as_slice()),
        Section::Unstaged => workdir.map(|w| w.unstaged_texts.as_slice()),
        _ => None,
    }
}

impl Detail {
    pub fn set_source(&mut self, source: Source) {
        self.source = source;
        self.commit = None;
        self.cursor = 0;
        self.anchor = None;
        self.scroll = 0;
        self.hscroll = 0;
        // Folds are remembered by path. That's right for the working
        // directory, where the same files stay around. In commits, a file
        // that was folded in one commit would stay folded in every other
        // commit that touches it.
        self.collapsed.retain(|(sec, _)| *sec != Section::Commit);
        self.seen.retain(|(sec, _)| *sec != Section::Commit);
        self.reset_for_reload();
    }

    pub fn set_commit(&mut self, c: Arc<CommitDetail>) {
        self.commit = Some(c);
        self.reset_for_reload();
    }

    /// The files changed, so cached per-file data is no longer valid.
    pub fn reset_for_reload(&mut self) {
        self.generation.next();
        self.highlights.clear();
        self.requested.clear();
        self.marks.clear();
        self.anchor = None;
    }

    pub fn rebuild(
        &mut self,
        workdir: Option<&Workdir>,
        labels: &HashMap<ObjectId, Vec<Label>>,
        theme: &Theme,
        split: bool,
        cfg: &Config,
    ) {
        let commit = self.commit.clone();
        let paths: HashMap<Section, Vec<String>> = [
            Section::Commit,
            Section::Conflicted,
            Section::Untracked,
            Section::Unstaged,
            Section::Staged,
        ]
        .into_iter()
        .map(|sec| {
            let v = section_files(workdir, commit.as_deref(), sec)
                .iter()
                .map(|f| f.path().to_string())
                .collect();
            (sec, v)
        })
        .collect();
        // Rows refer to files by index, and the indexes may change when the
        // data reloads. So look up the cursor's file by its path as of the
        // last rebuild.
        let old_key = row_key(&self.rows, self.cursor, &self.old_paths);
        let old_cursor = self.cursor;
        self.rows.clear();
        self.meta.clear();

        match &self.source {
            Source::None => {}
            Source::Commit(_) | Source::Stash(..) => match &commit {
                Some(c) => {
                    self.build_meta(c, labels, theme, cfg);
                    self.build_files(Section::Commit, &c.files, split, false);
                }
                None => self.rows.push(Row::Empty(if self.loading {
                    "Loading…".into()
                } else {
                    String::new()
                })),
            },
            Source::Workdir => match workdir {
                Some(w) => {
                    let mut any = false;
                    for (sec, files) in [
                        (Section::Conflicted, &w.conflicted),
                        (Section::Untracked, &w.untracked),
                        (Section::Unstaged, &w.unstaged),
                        (Section::Staged, &w.staged),
                    ] {
                        if files.is_empty() {
                            continue;
                        }
                        any = true;
                        self.rows.push(Row::Section(sec));
                        if !self.collapsed_sections.contains(&sec) {
                            self.build_files(sec, files, split, true);
                        }
                        self.rows.push(Row::Blank);
                    }
                    if !any {
                        self.rows.push(Row::Empty("Nothing to commit. The working directory is clean.".into()));
                    }
                }
                None => self.rows.push(Row::Empty("Loading…".into())),
            },
        }

        let max_no = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Hunk { sec, file, hunk } => section_files(workdir, commit.as_deref(), *sec)
                    .get(*file)
                    .and_then(|f| f.hunks.get(*hunk))
                    .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        self.gutter = max_no.to_string().len().max(3);

        // Put the cursor back where it was.
        let find = |key: &RowKey, rows: &[Row]| -> Option<usize> {
            (0..rows.len()).find(|&i| row_key(rows, i, &paths) == *key)
        };
        // Blank and placeholder rows all share one key, so looking one up
        // would just find the first blank row.
        let mut pos = if old_key == RowKey::Other { None } else { find(&old_key, &self.rows) };
        if pos.is_none() {
            let fallback = match &old_key {
                RowKey::Line(s, p, h, _) => Some(RowKey::Hunk(*s, p.clone(), *h)),
                _ => None,
            };
            if let Some(k) = fallback {
                pos = find(&k, &self.rows);
                if pos.is_none()
                    && let RowKey::Hunk(s, p, h) = &k
                    && *h > 0
                {
                    pos = find(&RowKey::Hunk(*s, p.clone(), h - 1), &self.rows);
                }
            }
        }
        if pos.is_none()
            && let RowKey::Line(s, p, ..) | RowKey::Hunk(s, p, _) | RowKey::File(s, p) = &old_key
        {
            pos = find(&RowKey::File(*s, p.clone()), &self.rows);
        }
        self.old_paths = paths;
        self.cursor = pos.unwrap_or(old_cursor).min(self.rows.len().saturating_sub(1));
        if let Some(a) = self.anchor
            && a >= self.rows.len()
        {
            self.anchor = None;
        }
    }

    fn build_meta(&mut self, c: &CommitDetail, labels: &HashMap<ObjectId, Vec<Label>>, theme: &Theme, cfg: &Config) {
        let _ = cfg;
        let dim = Style::default().fg(theme.dim);
        let fg = Style::default().fg(theme.fg);
        let mut push = |line: Line<'static>, click: Option<Cmd>| {
            self.meta.push(MetaLine { line, click });
            self.rows.push(Row::Meta(self.meta.len() - 1));
        };
        let mut msg_lines = c.message.trim_end().lines();
        let subject = msg_lines.next().unwrap_or("").to_string();
        push(
            Line::from(Span::styled(subject, fg.add_modifier(Modifier::BOLD))),
            None,
        );
        let body: Vec<String> = msg_lines.map(str::to_string).collect();
        let body_start = body.iter().position(|l| !l.trim().is_empty()).unwrap_or(body.len());
        if body_start < body.len() {
            push(Line::default(), None);
            for l in &body[body_start..] {
                push(Line::from(Span::styled(l.clone(), fg)), None);
            }
        }
        push(Line::default(), None);
        if let Some(ls) = labels.get(&c.id) {
            let mut spans = vec![Span::styled("Refs       ", dim)];
            for l in ls {
                let bg = match l.kind {
                    LabelKind::Head => theme.badge_head,
                    LabelKind::Local => theme.badge_local,
                    LabelKind::Remote => theme.badge_remote,
                    LabelKind::Tag => theme.badge_tag,
                };
                spans.push(Span::styled(format!(" {} ", l.name), Style::default().bg(bg).fg(theme.badge_fg)));
                spans.push(Span::raw(" "));
            }
            push(Line::from(spans), None);
        }
        let person = |p: &crate::git::commit::Person| {
            format!("{} <{}>  {}", p.name, p.email, format_date(p.time, p.offset))
        };
        push(
            Line::from(vec![Span::styled("Author     ", dim), Span::styled(person(&c.author), fg)]),
            None,
        );
        if c.committer.name != c.author.name
            || c.committer.email != c.author.email
            || c.committer.time != c.author.time
        {
            push(
                Line::from(vec![Span::styled("Committer  ", dim), Span::styled(person(&c.committer), fg)]),
                None,
            );
        }
        push(
            Line::from(vec![
                Span::styled("Commit     ", dim),
                Span::styled(c.id.to_string(), Style::default().fg(theme.accent)),
            ]),
            Some(Cmd::CopyHash(c.id)),
        );
        for (i, p) in c.parents.iter().enumerate() {
            push(
                Line::from(vec![
                    Span::styled(if i == 0 { "Parents    " } else { "           " }, dim),
                    Span::styled(p.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::UNDERLINED)),
                ]),
                Some(Cmd::JumpTo(*p)),
            );
        }
        let (mut adds, mut dels) = (0, 0);
        for f in &c.files {
            let (a, d) = f.added_removed();
            adds += a;
            dels += d;
        }
        push(
            Line::from(vec![
                Span::styled("Changes    ", dim),
                Span::styled(
                    format!("{} file{}", c.files.len(), if c.files.len() == 1 { "" } else { "s" }),
                    fg,
                ),
                Span::raw("  "),
                Span::styled(format!("+{adds}"), Style::default().fg(theme.add_fg)),
                Span::raw(" "),
                Span::styled(format!("−{dels}"), Style::default().fg(theme.del_fg)),
            ]),
            None,
        );
        push(Line::default(), None);
    }

    fn build_files(&mut self, sec: Section, files: &[FileDiff], split: bool, staging: bool) {
        let many = files.len() > 40;
        for (fi, f) in files.iter().enumerate() {
            let key = (sec, f.path().to_string());
            if self.seen.insert(key.clone())
                && (f.line_count() > 400 || (many && !staging))
            {
                self.collapsed.insert(key.clone());
            }
            self.rows.push(Row::File { sec, file: fi });
            if self.collapsed.contains(&key) {
                continue;
            }
            let notice = if f.binary {
                Some("Binary file".to_string())
            } else if f.too_large {
                Some("This diff is large. Press Enter here to show it anyway.".to_string())
            } else if f.hunks.is_empty() {
                match (f.old_mode.as_deref(), f.new_mode.as_deref()) {
                    (Some(a), Some(b)) if a != b => Some(format!("Mode changed from {a} to {b}")),
                    _ if f.status == crate::git::diff::FileStatus::Renamed => {
                        Some("Renamed without changes".to_string())
                    }
                    _ => Some("No content changes".to_string()),
                }
            } else {
                None
            };
            if let Some(text) = notice {
                self.rows.push(Row::Notice { sec, file: fi, text });
                continue;
            }
            self.marks
                .entry((sec, fi))
                .or_insert_with(|| Arc::new(f.hunks.iter().map(hunk_marks).collect()));
            for (hi, h) in f.hunks.iter().enumerate() {
                self.rows.push(Row::Hunk { sec, file: fi, hunk: hi });
                if split {
                    push_split_rows(&mut self.rows, sec, fi, hi, &h.lines);
                } else {
                    for li in 0..h.lines.len() {
                        self.rows.push(Row::Line { sec, file: fi, hunk: hi, line: li });
                    }
                }
            }
        }
    }

    /// Rows covered by the line selection, if there is one.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        Some((a.min(self.cursor), a.max(self.cursor)))
    }

    /// The changed lines in the selection that belong to the cursor's file.
    pub fn selected_lines<'a>(
        &self,
        files_of: impl Fn(Section) -> &'a [FileDiff],
    ) -> Option<LineSelection> {
        let (lo, hi) = self.selection()?;
        let (sec, file) = self.rows.get(self.cursor)?.file()?;
        let f = files_of(sec).get(file)?;
        let changed = |h: usize, l: usize| {
            f.hunks
                .get(h)
                .and_then(|h| h.lines.get(l))
                .is_some_and(|l| l.kind != LineKind::Context)
        };
        let mut out = Vec::new();
        for row in self.rows.get(lo..=hi)? {
            match *row {
                Row::Line { sec: s, file: fl, hunk, line } if s == sec && fl == file => {
                    if changed(hunk, line) {
                        out.push((hunk, line));
                    }
                }
                Row::Split { sec: s, file: fl, hunk, left, right } if s == sec && fl == file => {
                    for l in [left, right].into_iter().flatten() {
                        if changed(hunk, l) && !out.contains(&(hunk, l)) {
                            out.push((hunk, l));
                        }
                    }
                }
                _ => {}
            }
        }
        if out.is_empty() { None } else { Some((sec, file, out)) }
    }

    /// The piece of the diff that "stage" or "discard" applies to, based on
    /// the cursor and the line selection.
    pub fn unit_at_cursor<'a>(&self, files_of: impl Fn(Section) -> &'a [FileDiff]) -> Option<Unit> {
        let row = self.rows.get(self.cursor)?;
        if self.anchor.is_some()
            && let Some((s, f, lines)) = self.selected_lines(&files_of)
        {
            return Some(Unit::Lines(s, f, lines));
        }
        Some(match *row {
            Row::Section(s) => Unit::Section(s),
            Row::File { sec, file } | Row::Notice { sec, file, .. } => Unit::File(sec, file),
            Row::Hunk { sec, file, hunk } => Unit::Hunk(sec, file, hunk),
            Row::Line { sec, file, hunk, line } => {
                let kind = files_of(sec).get(file)?.hunks.get(hunk)?.lines.get(line)?.kind;
                if kind == LineKind::Context {
                    Unit::Hunk(sec, file, hunk)
                } else {
                    Unit::Lines(sec, file, vec![(hunk, line)])
                }
            }
            Row::Split { sec, file, hunk, left, right } => {
                let h = files_of(sec).get(file)?.hunks.get(hunk)?;
                let lines: Vec<(usize, usize)> = [left, right]
                    .into_iter()
                    .flatten()
                    .filter(|&l| h.lines.get(l).is_some_and(|x| x.kind != LineKind::Context))
                    .map(|l| (hunk, l))
                    .collect();
                if lines.is_empty() {
                    Unit::Hunk(sec, file, hunk)
                } else {
                    Unit::Lines(sec, file, lines)
                }
            }
            _ => return None,
        })
    }

    /// Move the cursor to the next or previous row matching `pred`.
    pub fn jump(&mut self, forward: bool, pred: impl Fn(&Row) -> bool) {
        let n = self.rows.len();
        if forward {
            if let Some(i) = (self.cursor + 1..n).find(|&i| pred(&self.rows[i])) {
                self.cursor = i;
            }
        } else if let Some(i) = (0..self.cursor).rev().find(|&i| pred(&self.rows[i])) {
            self.cursor = i;
        }
    }
}

fn row_key(rows: &[Row], i: usize, paths: &HashMap<Section, Vec<String>>) -> RowKey {
    let path = |sec: Section, file: usize| {
        paths
            .get(&sec)
            .and_then(|v| v.get(file))
            .cloned()
            .unwrap_or_default()
    };
    match rows.get(i) {
        Some(Row::Meta(m)) => RowKey::Meta(*m),
        Some(Row::Section(s)) => RowKey::Section(*s),
        Some(Row::File { sec, file }) | Some(Row::Notice { sec, file, .. }) => {
            RowKey::File(*sec, path(*sec, *file))
        }
        Some(Row::Hunk { sec, file, hunk }) => RowKey::Hunk(*sec, path(*sec, *file), *hunk),
        Some(Row::Line { sec, file, hunk, line }) => {
            RowKey::Line(*sec, path(*sec, *file), *hunk, *line)
        }
        Some(Row::Split { sec, file, hunk, left, right }) => RowKey::Line(
            *sec,
            path(*sec, *file),
            *hunk,
            right.or(*left).unwrap_or(0),
        ),
        _ => RowKey::Other,
    }
}

fn push_split_rows(
    rows: &mut Vec<Row>,
    sec: Section,
    file: usize,
    hunk: usize,
    lines: &[crate::git::diff::DiffLine],
) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            rows.push(Row::Split { sec, file, hunk, left: Some(i), right: Some(i) });
            i += 1;
            continue;
        }
        let mut dels = Vec::new();
        let mut adds = Vec::new();
        while i < lines.len() && lines[i].kind == LineKind::Del {
            dels.push(i);
            i += 1;
        }
        while i < lines.len() && lines[i].kind == LineKind::Add {
            adds.push(i);
            i += 1;
        }
        for k in 0..dels.len().max(adds.len()) {
            rows.push(Row::Split {
                sec,
                file,
                hunk,
                left: dels.get(k).copied(),
                right: adds.get(k).copied(),
            });
        }
    }
}
